use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use tracing::info;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use processor::{apply_rule, Config, ProcessResult};

const MANIFEST_NAME: &str = "archive-assistant.txt";
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(name = "zip-rewriter", about = "Process ZIP members through configured tools")]
struct Args {
    /// ZIP file to process
    zip_file: PathBuf,

    /// Config file
    #[arg(long, default_value = "zip-rewrite.toml")]
    config: PathBuf,

    /// Write result to a different path instead of in-place
    #[arg(long)]
    output: Option<PathBuf>,

    /// Print what would be done without modifying the file
    #[arg(long)]
    dry_run: bool,

    /// Reprocess even if archive-assistant.txt manifest is present
    #[arg(long)]
    force: bool,

    /// Log each member being processed
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

    let output_path = args.output.as_deref().unwrap_or(&args.zip_file);

    process_zip(&args.zip_file, output_path, &config, args.dry_run, args.force)?;

    Ok(())
}

fn process_zip(
    input_path: &Path,
    output_path: &Path,
    config: &Config,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let file = std::fs::File::open(input_path)
        .with_context(|| format!("failed to open {:?}", input_path))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("failed to read ZIP {:?}", input_path))?;

    // Idempotency check: skip if manifest present (unless --force).
    if !force && archive.by_name(MANIFEST_NAME).is_ok() {
        info!("{:?}: already processed (manifest present), skipping", input_path);
        return Ok(());
    }

    // Read all members into memory, applying processors as we go.
    let mut members: Vec<(String, Vec<u8>, zip::DateTime)> = Vec::new();
    let mut modified_members: Vec<String> = Vec::new();

    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).map(|f| f.name().to_owned()))
        .collect::<Result<_, _>>()?;

    for name in &names {
        if name == MANIFEST_NAME {
            // Drop any existing manifest; we'll write a fresh one.
            continue;
        }

        let mut entry = archive.by_name(name)?;
        let last_modified = entry.last_modified().unwrap_or_default();

        // Directories: preserve as-is.
        if entry.is_dir() {
            members.push((name.clone(), Vec::new(), last_modified));
            continue;
        }

        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        drop(entry);

        // Extract the filename component for pattern matching.
        let filename = Path::new(name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(name);

        if let Some(rule) = config.find_rule(filename) {
            info!("  processing member: {}", name);
            if dry_run {
                info!("    [dry-run] would apply rule for pattern '{}'", rule.r#match);
                members.push((name.clone(), data, last_modified));
            } else {
                match apply_rule(rule, &data, filename)? {
                    ProcessResult::Modified(new_data) => {
                        info!("    modified: {}", name);
                        modified_members.push(name.clone());
                        members.push((name.clone(), new_data, last_modified));
                    }
                    ProcessResult::Unchanged => {
                        info!("    unchanged: {}", name);
                        members.push((name.clone(), data, last_modified));
                    }
                }
            }
        } else {
            members.push((name.clone(), data, last_modified));
        }
    }

    // If nothing changed and no force, skip repack.
    if modified_members.is_empty() && !force {
        info!("{:?}: no members modified, skipping repack", input_path);
        return Ok(());
    }

    if dry_run {
        info!("{:?}: [dry-run] would repack with {} modified member(s)", input_path, modified_members.len());
        return Ok(());
    }

    // Build manifest content.
    let manifest = build_manifest(&modified_members, None);

    // Write new ZIP to a temp file, then move into place.
    let parent = output_path.parent().unwrap_or(Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut writer = ZipWriter::new(tmp.reopen()?);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        for (name, data, _last_modified) in &members {
            if data.is_empty() && name.ends_with('/') {
                writer.add_directory(name, options)?;
            } else {
                writer.start_file(name, options)?;
                writer.write_all(data)?;
            }
        }

        // Write manifest.
        writer.start_file(MANIFEST_NAME, options)?;
        writer.write_all(manifest.as_bytes())?;

        writer.finish()?;
    }

    // Persist to output path.
    tmp.persist(output_path)
        .with_context(|| format!("failed to write output to {:?}", output_path))?;

    info!(
        "{:?}: repacked ({} member(s) modified)",
        output_path,
        modified_members.len()
    );

    Ok(())
}

fn build_manifest(modified: &[String], converted_from: Option<&str>) -> String {
    let mut s = format!("archive-assistant v{}\n", VERSION);
    s.push_str(&format!("processed: {}\n", Utc::now().format("%Y-%m-%dT%H:%M:%SZ")));
    for name in modified {
        s.push_str(&format!("modified: {}\n", name));
    }
    if let Some(fmt) = converted_from {
        s.push_str(&format!("converted-from: {}\n", fmt));
    }
    s
}
