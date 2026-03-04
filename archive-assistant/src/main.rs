mod archive;
mod mtime;
mod state;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use tracing::info;
use walkdir::WalkDir;

use processor::{apply_rule, Config, ProcessResult};

use crate::archive::{is_archive, is_zip, repack_output_path, zip_has_manifest};
use crate::mtime::{bump_mtime, get_mtime};
use crate::state::StateDb;

#[derive(Parser, Debug)]
#[command(name = "archive-assistant", about = "Preprocess document archives for find-anything")]
struct Args {
    /// Directory to process
    path: PathBuf,

    /// Config file for processor rules (passed to archive-repack for archives,
    /// applied directly to top-level files)
    #[arg(long)]
    config: Option<PathBuf>,

    /// SQLite database for tracking processed files
    #[arg(long)]
    state_db: Option<PathBuf>,

    /// Local temp directory for processing
    #[arg(long)]
    temp_dir: Option<PathBuf>,

    /// Print what would be done without modifying files
    #[arg(long)]
    dry_run: bool,

    /// Only process top-level files, skip archive conversion
    #[arg(long)]
    files_only: bool,

    /// Only convert archives, skip top-level file processing
    #[arg(long)]
    archives_only: bool,

    /// Don't process files inside archives (pass --no-process-members to archive-repack)
    #[arg(long)]
    no_archive_files: bool,

    /// Parallel workers [default: CPUs / 2, minimum 1]
    #[arg(long)]
    jobs: Option<usize>,

    /// Log each file being processed
    #[arg(long)]
    verbose: bool,
}

// ── Tracing + indicatif integration ──────────────────────────────────────────

struct MpWriter(Arc<MultiProgress>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MpWriter {
    type Writer = MpLine;
    fn make_writer(&'a self) -> MpLine {
        MpLine { mp: Arc::clone(&self.0), buf: Vec::new() }
    }
}

struct MpLine {
    mp: Arc<MultiProgress>,
    buf: Vec<u8>,
}

impl Write for MpLine {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for MpLine {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            let s = String::from_utf8_lossy(&self.buf);
            let _ = self.mp.println(s.trim_end_matches('\n'));
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mp = Arc::new(MultiProgress::new());

    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(if args.verbose { "debug".parse()? } else { "info".parse()? }),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_writer(MpWriter(Arc::clone(&mp))),
        )
        .init();

    // Resolve the config path once; both top-level processing and archive-repack use it.
    let config_path = args.config.as_deref();
    let config = match config_path {
        Some(p) => Config::load(p).with_context(|| format!("failed to load config {:?}", p))?,
        None => Config::default(),
    };

    // Find archive-repack binary — expect it next to our own binary.
    let archive_repack = find_sibling_binary("archive-repack")?;

    // Set ARCHIVE_REPACK_CONFIG so recursive calls inside archive-repack inherit the config.
    if let Some(cfg) = config_path {
        std::env::set_var("ARCHIVE_REPACK_CONFIG", cfg);
    }

    let jobs = args.jobs.unwrap_or_else(|| (num_cpus() / 2).max(1));
    rayon::ThreadPoolBuilder::new().num_threads(jobs).build_global()?;

    let state_db: Option<std::sync::Mutex<StateDb>> = args
        .state_db
        .as_deref()
        .map(|p| StateDb::open(p).map(std::sync::Mutex::new))
        .transpose()?;

    let paths: Vec<PathBuf> = WalkDir::new(&args.path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();

    let progress = {
        let pb = mp.add(ProgressBar::new(paths.len() as u64));
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        pb.set_message("scanning...");
        pb
    };

    paths.par_iter().try_for_each(|path| -> Result<()> {
        let path_str = path.to_string_lossy();

        // State DB check.
        if let Some(db) = &state_db {
            let mtime = get_mtime(path)?;
            if db.lock().unwrap().is_current(&path_str, mtime)? {
                progress.inc(1);
                return Ok(());
            }
        }

        progress.set_message(path_str.to_string());

        if is_archive(path) {
            if args.files_only {
                progress.inc(1);
                return Ok(());
            }
            process_archive(
                path,
                &archive_repack,
                config_path,
                &state_db,
                args.dry_run,
                args.verbose,
            )?;
        } else if !args.archives_only {
            process_file(path, &config, &state_db, args.dry_run)?;
        }

        progress.inc(1);
        Ok(())
    })?;

    progress.finish_with_message("done");

    Ok(())
}

/// Process an archive by invoking archive-repack.
fn process_archive(
    path: &Path,
    archive_repack: &Path,
    config_path: Option<&Path>,
    state_db: &Option<std::sync::Mutex<StateDb>>,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    // ZIP with manifest = already processed, skip.
    if is_zip(path) && zip_has_manifest(path) {
        info!("{:?}: already processed, skipping", path);
        return Ok(());
    }

    let orig_mtime = get_mtime(path)?;

    if dry_run {
        info!("{:?}: [dry-run] would call archive-repack", path);
        return Ok(());
    }

    let output_path = repack_output_path(path);

    let mut cmd = Command::new(archive_repack);
    cmd.arg(path).arg("--output").arg(&output_path).arg("--write-manifest");
    if let Some(cfg) = config_path {
        cmd.arg("--config").arg(cfg);
    }
    // Suppress the interactive progress bar when called as a subprocess.
    cmd.arg("--no-progress");
    if verbose {
        cmd.arg("--verbose");
    }

    info!("{:?}: calling archive-repack", path);
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn archive-repack for {:?}", path))?;

    if !status.success() {
        bail!("archive-repack failed for {:?}", path);
    }

    // If format changed (non-ZIP → ZIP), delete original.
    if output_path != path {
        std::fs::remove_file(path)?;
        info!("{:?}: removed original (replaced by {:?})", path, output_path);
    }

    bump_mtime(&output_path, orig_mtime)?;

    if let Some(db) = state_db {
        let new_mtime = get_mtime(&output_path)?;
        db.lock().unwrap().record(&output_path.to_string_lossy(), new_mtime)?;
    }

    Ok(())
}

/// Apply processor rules to a top-level (non-archive) file.
fn process_file(
    path: &Path,
    config: &Config,
    state_db: &Option<std::sync::Mutex<StateDb>>,
    dry_run: bool,
) -> Result<()> {
    let path_str = path.to_string_lossy();
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&path_str);

    let Some(rule) = config.find_rule(filename) else {
        return Ok(());
    };

    let orig_mtime = get_mtime(path)?;

    // PDF-specific cheap skip checks.
    if filename.to_ascii_lowercase().ends_with(".pdf") {
        if pdf_has_text(path) {
            info!("{:?}: has text layer, skipping", path);
            if let Some(db) = state_db {
                db.lock().unwrap().record(&path_str, orig_mtime)?;
            }
            return Ok(());
        }
        if pdf_has_ocrmypdf_stamp(path) {
            info!("{:?}: has ocrmypdf stamp, skipping", path);
            if let Some(db) = state_db {
                db.lock().unwrap().record(&path_str, orig_mtime)?;
            }
            return Ok(());
        }
    }

    if dry_run {
        info!("{:?}: [dry-run] would apply rule '{}'", path, rule.r#match);
        return Ok(());
    }

    let tmp_dir = tempfile::TempDir::new()?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let tmp_path = tmp_dir.path().join(format!("input.{}", ext));
    std::fs::copy(path, &tmp_path)?;

    let data = std::fs::read(&tmp_path)?;
    match apply_rule(rule, &data, filename)? {
        ProcessResult::Modified(new_data) => {
            info!("{:?}: modified", path);
            std::fs::copy(
                {
                    std::fs::write(&tmp_path, &new_data)?;
                    &tmp_path
                },
                path,
            )?;
            bump_mtime(path, orig_mtime)?;
            if let Some(db) = state_db {
                let new_mtime = get_mtime(path)?;
                db.lock().unwrap().record(&path_str, new_mtime)?;
            }
        }
        ProcessResult::Unchanged => {
            info!("{:?}: unchanged", path);
            if let Some(db) = state_db {
                db.lock().unwrap().record(&path_str, orig_mtime)?;
            }
        }
    }

    Ok(())
}

/// Locate a binary that lives next to the current executable.
fn find_sibling_binary(name: &str) -> Result<PathBuf> {
    let exe_dir = std::env::current_exe()
        .context("cannot determine current executable path")?;
    let exe_dir = exe_dir.parent().context("executable has no parent directory")?;
    let candidate = exe_dir.join(name);
    if candidate.exists() {
        return Ok(candidate);
    }
    // Fall back to PATH (useful during `cargo run`).
    Ok(PathBuf::from(name))
}

fn pdf_has_text(path: &Path) -> bool {
    match Command::new("pdftotext").arg(path).arg("-").output() {
        Ok(o) => !o.stdout.iter().all(|b| b.is_ascii_whitespace()),
        Err(_) => false,
    }
}

fn pdf_has_ocrmypdf_stamp(path: &Path) -> bool {
    let doc = match lopdf::Document::load(path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let catalog = match doc.catalog() {
        Ok(d) => d,
        Err(_) => return false,
    };
    let metadata_obj = match catalog.get(b"Metadata") {
        Ok(o) => o,
        Err(_) => return false,
    };
    let metadata_id = match metadata_obj {
        lopdf::Object::Reference(r) => *r,
        _ => return false,
    };
    match doc
        .get_object(metadata_id)
        .and_then(|o| o.as_stream())
        .and_then(|s| s.decompressed_content())
    {
        Ok(data) => data.windows(8).any(|w| w.eq_ignore_ascii_case(b"ocrmypdf")),
        Err(_) => false,
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}
