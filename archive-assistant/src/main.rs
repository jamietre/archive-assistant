mod archive;
mod mtime;
mod state;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result};
use clap::Parser;
use rayon::prelude::*;
use tracing::info;
use walkdir::WalkDir;

use processor::{apply_rule, Config, ProcessResult};

use crate::archive::{archive_kind, is_archive, process_archive, ArchiveKind};
use crate::mtime::{bump_mtime, get_mtime};
use crate::state::StateDb;

#[derive(Parser, Debug)]
#[command(name = "archive-assistant", about = "Preprocess document archives for find-anything")]
struct Args {
    /// Directory to process
    path: PathBuf,

    /// Config file for processor rules
    #[arg(long, default_value = "zip-rewrite.toml")]
    config: PathBuf,

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
    ocr_only: bool,

    /// Only convert archives, skip top-level file processing
    #[arg(long)]
    convert_only: bool,

    /// Don't process files inside archives
    #[arg(long)]
    no_archive_files: bool,

    /// Parallel workers [default: CPUs / 2, minimum 1]
    #[arg(long)]
    jobs: Option<usize>,

    /// Log each file being processed
    #[arg(long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(if args.verbose { "debug".parse()? } else { "info".parse()? }),
        )
        .with_target(false)
        .init();

    let config = Config::load(&args.config)
        .with_context(|| format!("failed to load config from {:?}", args.config))?;

    let jobs = args.jobs.unwrap_or_else(|| {
        (num_cpus() / 2).max(1)
    });
    rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build_global()?;

    let state_db: Option<Mutex<StateDb>> = args
        .state_db
        .as_deref()
        .map(|p| StateDb::open(p).map(Mutex::new))
        .transpose()?;

    // Collect all file paths first (walkdir is not Send).
    let paths: Vec<PathBuf> = WalkDir::new(&args.path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();

    paths.par_iter().try_for_each(|path| -> Result<()> {
        let path_str = path.to_string_lossy();

        // State DB check.
        if let Some(db) = &state_db {
            let mtime = get_mtime(path)?;
            if db.lock().unwrap().is_current(&path_str, mtime)? {
                return Ok(());
            }
        }

        if is_archive(path) {
            if !args.ocr_only {
                let modified = process_archive(
                    path,
                    &config,
                    !args.no_archive_files && !args.convert_only,
                    args.dry_run,
                )?;

                if modified && !args.dry_run {
                    // Determine the resulting path (may have changed to .zip).
                    let result_path = if archive_kind(path) != Some(ArchiveKind::Zip) {
                        let zip_name = path
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                            + ".zip";
                        path.with_file_name(zip_name)
                    } else {
                        path.clone()
                    };

                    let orig_mtime = get_mtime(path).unwrap_or(0);
                    bump_mtime(&result_path, orig_mtime)?;

                    if let Some(db) = &state_db {
                        let new_mtime = get_mtime(&result_path)?;
                        db.lock().unwrap().record(&result_path.to_string_lossy(), new_mtime)?;
                    }
                }
            }
        } else if !args.convert_only {
            // Top-level non-archive file: apply processor rules.
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&path_str);

            if let Some(rule) = config.find_rule(filename) {
                let orig_mtime = get_mtime(path)?;

                // PDF-specific cheap skip checks before invoking the processor.
                if filename.to_ascii_lowercase().ends_with(".pdf") {
                    if pdf_has_text(path) {
                        info!("{:?}: has text layer, skipping", path);
                        if let Some(db) = &state_db {
                            db.lock().unwrap().record(&path_str, orig_mtime)?;
                        }
                        return Ok(());
                    }
                    if pdf_has_ocrmypdf_stamp(path) {
                        info!("{:?}: has ocrmypdf stamp, skipping", path);
                        if let Some(db) = &state_db {
                            db.lock().unwrap().record(&path_str, orig_mtime)?;
                        }
                        return Ok(());
                    }
                }

                if args.dry_run {
                    info!("{:?}: [dry-run] would apply rule '{}'", path, rule.r#match);
                    return Ok(());
                }

                // Copy to temp, process, write back.
                let tmp_dir = tempfile::TempDir::new()?;
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let tmp_path = tmp_dir.path().join(format!("input.{}", ext));
                std::fs::copy(path, &tmp_path)?;

                let data = std::fs::read(&tmp_path)?;
                match apply_rule(rule, &data, filename)? {
                    ProcessResult::Modified(new_data) => {
                        info!("{:?}: modified", path);
                        std::fs::write(&tmp_path, &new_data)?;
                        std::fs::copy(&tmp_path, path)?;
                        bump_mtime(path, orig_mtime)?;
                        if let Some(db) = &state_db {
                            let new_mtime = get_mtime(path)?;
                            db.lock().unwrap().record(&path_str, new_mtime)?;
                        }
                    }
                    ProcessResult::Unchanged => {
                        info!("{:?}: unchanged", path);
                        if let Some(db) = &state_db {
                            db.lock().unwrap().record(&path_str, orig_mtime)?;
                        }
                    }
                }
            }
        }

        Ok(())
    })?;

    Ok(())
}

/// Check if a PDF has any extractable text via pdftotext.
fn pdf_has_text(path: &Path) -> bool {
    let out = Command::new("pdftotext")
        .arg(path)
        .arg("-")
        .output();
    match out {
        Ok(o) => !o.stdout.iter().all(|b| b.is_ascii_whitespace()),
        Err(_) => false,
    }
}

/// Check XMP metadata via lopdf for ocrmypdf stamp.
fn pdf_has_ocrmypdf_stamp(path: &Path) -> bool {
    let doc = match lopdf::Document::load(path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    // Look for "ocrmypdf" anywhere in the XMP metadata stream.
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
    let stream = match doc.get_object(metadata_id).and_then(|o| o.as_stream()) {
        Ok(s) => s,
        Err(_) => return false,
    };
    match stream.decompressed_content() {
        Ok(data) => data.windows(8).any(|w| w == b"ocrmypdf"),
        Err(_) => false,
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}
