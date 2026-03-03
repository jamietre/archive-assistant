use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::Parser;
use tracing::{debug, info, warn};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use processor::{apply_rule, ChainStep, Config, IoMode, ProcessorRule, ProcessResult};

const MANIFEST_NAME: &str = "archive-assistant.txt";
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "archive-repack",
    about = "Repack any archive as a ZIP, applying processor rules to members",
    long_about = "Repack any archive (zip, 7z, tar, tar.gz, tar.bz2, tar.xz, gz, bz2, xz, rar) \
        as a ZIP, applying configured processor rules to matching members.\n\n\
        Nested archives found inside are recursively repacked by shelling out to archive-repack.\n\n\
        Rules can be supplied via a config file, inline flags, or both. \
        Inline flags define a single rule prepended to any config-file rules.\n\n\
        Examples:\n  \
          archive-repack input.7z --config zip-rewrite.toml\n  \
          archive-repack input.tar.gz --output repacked.zip --write-manifest\n  \
          archive-repack input.zip \
            --match '*.pdf' --command ocrmypdf \
            --arg '--skip-text' --arg '--quiet' --arg '{input}' --arg '{input}'"
)]
struct Args {
    /// Input archive (any supported format)
    input: PathBuf,

    /// Output ZIP path [default: input path with .zip extension]
    #[arg(long)]
    output: Option<PathBuf>,

    /// Config file defining processor rules
    #[arg(long)]
    config: Option<PathBuf>,

    // ── Inline rule flags ────────────────────────────────────────────────────
    /// Filename glob for the inline rule [default: * when --command/--shell given]
    #[arg(long, value_name = "GLOB")]
    r#match: Option<String>,

    /// Command for the inline rule
    #[arg(long, value_name = "CMD")]
    command: Option<String>,

    /// Argument for the inline rule command (repeatable). Use {input} and {output}.
    #[arg(long = "arg", value_name = "ARG")]
    args: Vec<String>,

    /// I/O mode for the inline rule [default: in-place]
    #[arg(long, value_name = "MODE")]
    io: Option<IoMode>,

    /// Shell expression for the inline rule (alternative to --command)
    #[arg(long, value_name = "EXPR", conflicts_with = "command")]
    shell: Option<String>,

    // ── General options ──────────────────────────────────────────────────────
    /// Embed archive-assistant.txt manifest in the output ZIP
    #[arg(long)]
    write_manifest: bool,

    /// Print what would be done without writing any output
    #[arg(long)]
    dry_run: bool,

    /// Log each member being processed
    #[arg(long)]
    verbose: bool,
}

impl Args {
    fn effective_config(&self) -> Result<Config> {
        let mut config = match &self.config {
            Some(path) => Config::load(path)
                .with_context(|| format!("failed to load config from {:?}", path))?,
            None => Config::default(),
        };

        let has_inline = self.command.is_some() || self.shell.is_some();
        if has_inline {
            let pattern = self.r#match.clone().unwrap_or_else(|| "*".to_owned());
            let io = self.io.unwrap_or(IoMode::InPlace);

            let rule = if let Some(shell_expr) = &self.shell {
                ProcessorRule {
                    r#match: pattern,
                    chain: vec![],
                    shell: Some(shell_expr.clone()),
                    io,
                }
            } else {
                ProcessorRule {
                    r#match: pattern,
                    chain: vec![ChainStep {
                        command: self.command.clone().unwrap(),
                        args: self.args.clone(),
                        io,
                    }],
                    shell: None,
                    io: IoMode::InPlace,
                }
            };

            config.processor.insert(0, rule);
        } else if self.r#match.is_some() || self.io.is_some() || !self.args.is_empty() {
            bail!("--match, --arg, and --io require either --command or --shell");
        }

        Ok(config)
    }

    fn output_path(&self) -> PathBuf {
        if let Some(p) = &self.output {
            return p.clone();
        }
        let name = self
            .input
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        self.input.with_file_name(format!("{}.zip", archive_stem(&name)))
    }
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

    // Propagate the config path so recursive calls on nested archives inherit it.
    if let Some(cfg) = &args.config {
        if let Ok(abs) = cfg.canonicalize() {
            std::env::set_var("ARCHIVE_REPACK_CONFIG", abs);
        }
    }

    let config = args.effective_config()?;
    let output_path = args.output_path();

    repack(&args.input, &output_path, &config, args.write_manifest, args.dry_run)
}

// ── Archive detection ────────────────────────────────────────────────────────

fn is_archive(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".zip")
        || n.ends_with(".7z")
        || n.ends_with(".tar")
        || n.ends_with(".tar.gz")
        || n.ends_with(".tgz")
        || n.ends_with(".tar.bz2")
        || n.ends_with(".tbz2")
        || n.ends_with(".tar.xz")
        || n.ends_with(".txz")
        || n.ends_with(".gz")
        || n.ends_with(".bz2")
        || n.ends_with(".xz")
        || n.ends_with(".rar")
}

fn archive_stem(name: &str) -> &str {
    for suffix in &[".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz"] {
        if let Some(s) = name.strip_suffix(suffix) {
            return s;
        }
    }
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
}

// ── Streaming iteration ──────────────────────────────────────────────────────
//
// Each `for_each_*` function calls `callback(name, is_dir, reader)` once per
// member. The callback receives a `&mut dyn Read` for the member's content and
// is responsible for consuming it (or not — unread bytes are discarded).
// Only one member's data is live at a time.

type MemberFn<'a> = dyn FnMut(&str, bool, &mut dyn Read) -> Result<()> + 'a;

fn for_each_member(path: &Path, callback: &mut MemberFn<'_>) -> Result<()> {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return for_each_tar(flate2::read::GzDecoder::new(std::fs::File::open(path)?), callback);
    }
    if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
        return for_each_tar(bzip2::read::BzDecoder::new(std::fs::File::open(path)?), callback);
    }
    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        return for_each_tar(xz2::read::XzDecoder::new(std::fs::File::open(path)?), callback);
    }

    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("zip") => for_each_zip(path, callback),
        Some("7z") => for_each_7z(path, callback),
        Some("tar") => for_each_tar(std::fs::File::open(path)?, callback),
        Some("gz") => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            let mut dec = flate2::read::GzDecoder::new(std::fs::File::open(path)?);
            callback(stem, false, &mut dec)
        }
        Some("bz2") => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            let mut dec = bzip2::read::BzDecoder::new(std::fs::File::open(path)?);
            callback(stem, false, &mut dec)
        }
        Some("xz") => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            let mut dec = xz2::read::XzDecoder::new(std::fs::File::open(path)?);
            callback(stem, false, &mut dec)
        }
        Some("rar") => for_each_rar(path, callback),
        _ => bail!("unsupported archive format: {:?}", path),
    }
}

fn for_each_zip(path: &Path, callback: &mut MemberFn<'_>) -> Result<()> {
    use zip::ZipArchive;
    let mut archive = ZipArchive::new(std::fs::File::open(path)?)?;
    // Collect names first to avoid borrow-checker issues with by_index inside loop.
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).map(|e| e.name().to_owned()))
        .collect::<Result<_, _>>()?;
    for name in names {
        let mut entry = archive.by_name(&name)?;
        let is_dir = entry.is_dir();
        callback(&name, is_dir, &mut entry)?;
    }
    Ok(())
}

fn for_each_7z(path: &Path, callback: &mut MemberFn<'_>) -> Result<()> {
    // The sevenz callback uses its own error type, so we stash any callback
    // error in an Option and re-raise it after iteration completes.
    let mut saved_error: Option<anyhow::Error> = None;

    sevenz_rust2::decompress_file_with_extract_fn(
        path,
        Path::new("/dev/null"),
        |entry, reader, _| {
            if saved_error.is_some() {
                return Ok(false);
            }
            if let Err(e) = callback(entry.name(), entry.is_directory(), reader) {
                saved_error = Some(e);
                return Ok(false);
            }
            Ok(true)
        },
    )
    .map_err(|e| anyhow::anyhow!("7z extraction failed: {}", e))?;

    if let Some(e) = saved_error {
        return Err(e);
    }
    Ok(())
}

fn for_each_tar<R: Read>(reader: R, callback: &mut MemberFn<'_>) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        let is_dir = entry.header().entry_type().is_dir();
        callback(&name, is_dir, &mut entry)?;
    }
    Ok(())
}

fn for_each_rar(path: &Path, callback: &mut MemberFn<'_>) -> Result<()> {
    // The unrar crate iterates members one at a time via a typestate machine.
    // Individual member content is returned as Vec<u8> (no streaming Read
    // handle available), but only one member is in memory at a time.
    let archive = unrar::Archive::new(path)
        .open_for_processing()
        .map_err(|e| anyhow::anyhow!("failed to open RAR {:?}: {}", path, e))?;

    let mut cursor = archive;
    loop {
        let header = cursor
            .read_header()
            .map_err(|e| anyhow::anyhow!("RAR read error: {}", e))?;

        let Some(header) = header else { break };

        let entry = header.entry();
        let name = entry.filename.to_string_lossy().into_owned();

        if entry.is_directory() {
            let (_, rest) = header
                .read()
                .map_err(|e| anyhow::anyhow!("RAR skip dir error: {}", e))?;
            callback(&format!("{}/", name), true, &mut io::empty())?;
            cursor = rest;
        } else {
            let (data, rest) = header
                .read()
                .map_err(|e| anyhow::anyhow!("RAR read member '{}': {}", name, e))?;
            callback(&name, false, &mut data.as_slice())?;
            cursor = rest;
        }
    }
    Ok(())
}

// ── Core repack logic ────────────────────────────────────────────────────────

fn repack(
    input: &Path,
    output: &Path,
    config: &Config,
    write_manifest: bool,
    dry_run: bool,
) -> Result<()> {
    info!("repacking {:?} -> {:?}", input, output);

    if dry_run {
        // In dry-run mode just iterate and report; no ZipWriter needed.
        for_each_member(input, &mut |name, is_dir, _reader| {
            if is_dir {
                return Ok(());
            }
            let filename =
                Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or(name);
            if is_archive(filename) {
                info!("  [dry-run] would repack nested archive: {}", name);
            } else if let Some(rule) = config.find_rule(filename) {
                info!("  [dry-run] would apply rule '{}' to: {}", rule.r#match, name);
            }
            Ok(())
        })
        .with_context(|| format!("failed to read {:?}", input))?;
        info!("[dry-run] would write {:?}", output);
        return Ok(());
    }

    let self_exe = std::env::current_exe()?;
    let tmp_dir = tempfile::TempDir::new()?;
    let mut modified: Vec<String> = Vec::new();

    // Open the output ZIP immediately; members are written as they are processed.
    let parent = output.parent().unwrap_or(Path::new("."));
    let tmp_zip = tempfile::NamedTempFile::new_in(parent)?;
    let mut writer = ZipWriter::new(tmp_zip.reopen()?);
    let options =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for_each_member(input, &mut |name, is_dir, reader| {
        if is_dir {
            writer.add_directory(name, options)?;
            return Ok(());
        }

        let filename =
            Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or(name);

        // Nested archive: write to temp, shell out, stream result into ZIP.
        if is_archive(filename) {
            debug!("  nested archive: {}", name);
            let nested_stem = archive_stem(filename).to_owned();
            let dir_prefix = name.strip_suffix(filename).unwrap_or("");
            let nested_zip_name = format!("{}{}.zip", dir_prefix, nested_stem);

            let nested_input = tmp_dir.path().join(filename);
            let nested_output = tmp_dir.path().join(format!("{}.zip", nested_stem));

            // Write nested archive bytes to disk (unavoidable — subprocess needs a file).
            {
                let mut f = std::fs::File::create(&nested_input)?;
                io::copy(reader, &mut f)?;
            }

            let status =
                build_self_cmd(&self_exe, &nested_input, &nested_output, write_manifest)
                    .status()
                    .with_context(|| {
                        format!("failed to spawn archive-repack for {:?}", nested_input)
                    })?;

            if !status.success() {
                warn!("archive-repack failed for {}, keeping original", name);
                // Re-read the original from disk and store it as-is.
                let mut f = std::fs::File::open(&nested_input)?;
                writer.start_file(name, options)?;
                io::copy(&mut f, &mut writer)?;
                return Ok(());
            }

            writer.start_file(&nested_zip_name, options)?;
            io::copy(&mut std::fs::File::open(&nested_output)?, &mut writer)?;
            info!("  repacked nested: {} -> {}", name, nested_zip_name);
            modified.push(name.to_owned());
            return Ok(());
        }

        // Regular member: apply processor rule or stream straight through.
        if let Some(rule) = config.find_rule(filename) {
            info!("  processing member: {}", name);
            // Must buffer this member to pass to the processor tool.
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;

            match apply_rule(rule, &data, filename)? {
                ProcessResult::Modified(new_data) => {
                    info!("  modified: {}", name);
                    modified.push(name.to_owned());
                    writer.start_file(name, options)?;
                    writer.write_all(&new_data)?;
                }
                ProcessResult::Unchanged => {
                    debug!("  unchanged: {}", name);
                    writer.start_file(name, options)?;
                    writer.write_all(&data)?;
                }
            }
        } else {
            // No rule: copy directly from extraction reader to ZIP writer.
            writer.start_file(name, options)?;
            io::copy(reader, &mut writer)?;
        }

        Ok(())
    })
    .with_context(|| format!("failed to process {:?}", input))?;

    if write_manifest {
        writer.start_file(MANIFEST_NAME, options)?;
        writer.write_all(build_manifest(&modified).as_bytes())?;
    }

    writer.finish()?;
    tmp_zip
        .persist(output)
        .with_context(|| format!("failed to write {:?}", output))?;

    info!("wrote {:?} ({} member(s) modified)", output, modified.len());
    Ok(())
}

fn build_self_cmd(
    exe: &Path,
    input: &Path,
    output: &Path,
    write_manifest: bool,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(input).arg("--output").arg(output);
    if let Ok(cfg_path) = std::env::var("ARCHIVE_REPACK_CONFIG") {
        cmd.arg("--config").arg(cfg_path);
    }
    if write_manifest {
        cmd.arg("--write-manifest");
    }
    cmd
}

fn build_manifest(modified: &[String]) -> String {
    let mut s = format!("archive-assistant v{}\n", VERSION);
    s.push_str(&format!("processed: {}\n", Utc::now().format("%Y-%m-%dT%H:%M:%SZ")));
    for name in modified {
        s.push_str(&format!("modified: {}\n", name));
    }
    s
}
