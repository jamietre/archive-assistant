use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tracing::{debug, info, warn};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use processor::{apply_rule, ChainStep, Config, IoMode, ProcessorRule, ProcessResult};

const MANIFEST_NAME: &str = "archive-assistant.txt";
const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Tracing + indicatif integration ──────────────────────────────────────────
//
// Routes tracing output through indicatif's MultiProgress so that log lines
// are printed above the progress bar without overlapping it.

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

    /// Log each member name as it is processed
    #[arg(long)]
    verbose: bool,

    /// Glob pattern to exclude from the output archive (repeatable).
    /// Matched against the full member path, e.g. --exclude "*.DS_Store"
    #[arg(long = "exclude", value_name = "GLOB")]
    excludes: Vec<String>,

    /// Disable the interactive progress bar (use when output is consumed
    /// by another tool, e.g. archive-assistant)
    #[arg(long)]
    no_progress: bool,

    /// How to handle nested archives found inside the input.
    /// passthrough (default): copy the nested archive as-is without processing.
    /// repack: shell out to archive-repack recursively, producing a nested ZIP.
    /// flatten: expand contents into a subdirectory named after the archive.
    #[arg(long, value_name = "MODE", default_value = "passthrough")]
    nested: NestedMode,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[clap(rename_all = "kebab-case")]
enum NestedMode {
    /// Recursively repack nested archives as ZIPs
    Repack,
    /// Expand nested archive contents into a subdirectory
    Flatten,
    /// Copy nested archives into the output ZIP without processing (default)
    #[default]
    Passthrough,
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

        config.exclude.extend(self.excludes.iter().cloned());

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
        let stem = archive_stem(&name);
        let candidate = self.input.with_file_name(format!("{}.zip", stem));

        // If the default output collides with the input (e.g. input is already a ZIP),
        // find the next available versioned name: foo.2.zip, foo.3.zip, …
        if candidate == self.input {
            // Strip any existing version suffix (.N) so foo.2.zip → foo, not foo.2.
            let (base, start) = match stem.rfind('.') {
                Some(i) => match stem[i + 1..].parse::<u32>() {
                    Ok(n) => (&stem[..i], n + 1),
                    Err(_) => (stem, 2),
                },
                None => (stem, 2),
            };
            for n in start.. {
                let versioned = self.input.with_file_name(format!("{}.{}.zip", base, n));
                if !versioned.exists() {
                    return versioned;
                }
            }
            unreachable!()
        }

        candidate
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

    // Propagate the config path so recursive calls on nested archives inherit it.
    if let Some(cfg) = &args.config {
        if let Ok(abs) = cfg.canonicalize() {
            std::env::set_var("ARCHIVE_REPACK_CONFIG", abs);
        }
    }

    let config = args.effective_config()?;
    let output_path = args.output_path();
    let progress = make_progress_bar(&mp, &args.input, args.no_progress);

    repack(
        &args.input,
        &output_path,
        &config,
        args.write_manifest,
        args.dry_run,
        args.nested,
        &progress,
    )
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

/// Recursively flatten a nested archive into `writer` under `prefix/`.
///
/// Every member of `input` is written as `{prefix}{member_name}`. Nested
/// archives found inside are flattened further into `{prefix}{stem}/`.
/// Processor rules are applied to regular members exactly as in the main loop.
#[allow(clippy::too_many_arguments)]
fn flatten_into_zip<W: Write + std::io::Seek>(
    input: &Path,
    prefix: &str,
    mut writer: &mut ZipWriter<W>,
    config: &Config,
    options: SimpleFileOptions,
    modified: &mut Vec<String>,
    tmp_dir: &tempfile::TempDir,
    tmp_counter: &mut u32,
    progress: &ProgressBar,
) -> Result<()> {
    for_each_member(input, &mut |name, is_dir, reader| {
        let full_name = format!("{}{}", prefix, name);
        let filename = Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or(name);

        if is_dir {
            writer.add_directory(&full_name, options)?;
            return Ok(());
        }

        progress.set_message(full_name.clone());
        progress.inc(1);

        if config.is_excluded(&full_name) {
            debug!("excluded: {}", full_name);
            io::copy(reader, &mut io::sink())?;
            return Ok(());
        }

        debug!("{}", full_name);

        // Nested archive inside a flattened archive: flatten recursively.
        if is_archive(filename) {
            let nested_stem = archive_stem(filename).to_owned();
            let dir_prefix = name.strip_suffix(filename).unwrap_or("");
            let flat_prefix = format!("{}{}{}/", prefix, dir_prefix, nested_stem);

            *tmp_counter += 1;
            let nested_input = tmp_dir.path().join(format!("{}-{}", tmp_counter, filename));
            {
                let mut f = std::fs::File::create(&nested_input)?;
                io::copy(reader, &mut f)?;
            }

            info!("flattening nested: {} -> {}", full_name, flat_prefix);
            return flatten_into_zip(
                &nested_input,
                &flat_prefix,
                writer,
                config,
                options,
                modified,
                tmp_dir,
                tmp_counter,
                progress,
            );
        }

        // Regular member: apply processor rule or stream straight through.
        if let Some(rule) = config.find_rule(filename) {
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;
            match apply_rule(rule, &data, filename) {
                Ok(ProcessResult::Modified(new_data)) => {
                    info!("modified: {}", full_name);
                    modified.push(full_name.clone());
                    writer.start_file(&full_name, options)?;
                    writer.write_all(&new_data)?;
                }
                Ok(ProcessResult::Unchanged) => {
                    writer.start_file(&full_name, options)?;
                    writer.write_all(&data)?;
                }
                Err(e) => {
                    warn!("processor failed for '{}', keeping original: {:#}", full_name, e);
                    writer.start_file(&full_name, options)?;
                    writer.write_all(&data)?;
                }
            }
        } else {
            writer.start_file(&full_name, options)?;
            io::copy(reader, &mut writer)?;
        }

        Ok(())
    })
}

fn repack(
    input: &Path,
    output: &Path,
    config: &Config,
    write_manifest: bool,
    dry_run: bool,
    nested: NestedMode,
    progress: &ProgressBar,
) -> Result<()> {
    info!("repacking {:?} -> {:?}", input, output);

    if dry_run {
        for_each_member(input, &mut |name, is_dir, _reader| {
            if is_dir {
                return Ok(());
            }
            if config.is_excluded(name) {
                info!("  [dry-run] would exclude: {}", name);
                return Ok(());
            }
            let filename =
                Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or(name);
            if is_archive(filename) {
                match nested {
                    NestedMode::Repack => info!("  [dry-run] would repack nested archive: {}", name),
                    NestedMode::Flatten => info!("  [dry-run] would flatten nested archive: {}", name),
                    NestedMode::Passthrough => info!("  [dry-run] would pass through nested archive: {}", name),
                }
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
    let mut tmp_counter: u32 = 0;
    let mut existing_manifest: Option<String> = None;

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

        // Capture existing manifest so we can append to it; don't copy it through.
        if name == MANIFEST_NAME {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            existing_manifest = Some(String::from_utf8_lossy(&bytes).into_owned());
            return Ok(());
        }

        progress.set_message(name.to_owned());
        progress.inc(1);

        if config.is_excluded(name) {
            debug!("excluded: {}", name);
            io::copy(reader, &mut io::sink())?;
            return Ok(());
        }

        let filename =
            Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or(name);

        debug!("{}", name);

        // Nested archive: repack, flatten, or pass through depending on --nested.
        if is_archive(filename) {
            let nested_stem = archive_stem(filename).to_owned();
            let dir_prefix = name.strip_suffix(filename).unwrap_or("");

            match nested {
                NestedMode::Passthrough => {
                    // Copy the nested archive into the output ZIP unchanged.
                    writer.start_file(name, options)?;
                    io::copy(reader, &mut writer)?;
                    return Ok(());
                }
                NestedMode::Flatten => {
                    tmp_counter += 1;
                    let nested_input =
                        tmp_dir.path().join(format!("{}-{}", tmp_counter, filename));
                    {
                        let mut f = std::fs::File::create(&nested_input)?;
                        io::copy(reader, &mut f)?;
                    }
                    let flat_prefix = format!("{}{}/", dir_prefix, nested_stem);
                    info!("flattening nested: {} -> {}", name, flat_prefix);
                    flatten_into_zip(
                        &nested_input,
                        &flat_prefix,
                        &mut writer,
                        config,
                        options,
                        &mut modified,
                        &tmp_dir,
                        &mut tmp_counter,
                        progress,
                    )?;
                    return Ok(());
                }
                NestedMode::Repack => {
                    tmp_counter += 1;
                    let nested_input =
                        tmp_dir.path().join(format!("{}-{}", tmp_counter, filename));
                    {
                        let mut f = std::fs::File::create(&nested_input)?;
                        io::copy(reader, &mut f)?;
                    }
                    let nested_zip_name = format!("{}{}.zip", dir_prefix, nested_stem);
                    let nested_output =
                        tmp_dir.path().join(format!("{}-{}.zip", tmp_counter, nested_stem));

                    let status =
                        build_self_cmd(&self_exe, &nested_input, &nested_output, write_manifest, nested)
                            .status()
                            .with_context(|| {
                                format!("failed to spawn archive-repack for {:?}", nested_input)
                            })?;

                    if !status.success() {
                        warn!("archive-repack failed for {}, keeping original", name);
                        writer.start_file(name, options)?;
                        io::copy(&mut std::fs::File::open(&nested_input)?, &mut writer)?;
                        return Ok(());
                    }

                    writer.start_file(&nested_zip_name, options)?;
                    io::copy(&mut std::fs::File::open(&nested_output)?, &mut writer)?;
                    info!("repacked nested: {} -> {}", name, nested_zip_name);
                    modified.push(name.to_owned());
                    return Ok(());
                }
            }
        }

        // Regular member: apply processor rule or stream straight through.
        if let Some(rule) = config.find_rule(filename) {
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;

            // Skip PDFs that have already been processed by ocrmypdf: the XMP
            // metadata stream is stored uncompressed and contains the string
            // "ocrmypdf", so a raw byte search is sufficient.
            if pdf_has_ocrmypdf_stamp(&data) {
                debug!("already ocr'd, skipping: {}", name);
                writer.start_file(name, options)?;
                writer.write_all(&data)?;
                return Ok(());
            }

            match apply_rule(rule, &data, filename) {
                Ok(ProcessResult::Modified(new_data)) => {
                    info!("modified: {}", name);
                    modified.push(name.to_owned());
                    writer.start_file(name, options)?;
                    writer.write_all(&new_data)?;
                }
                Ok(ProcessResult::Unchanged) => {
                    writer.start_file(name, options)?;
                    writer.write_all(&data)?;
                }
                Err(e) => {
                    warn!("processor failed for '{}', keeping original: {:#}", name, e);
                    writer.start_file(name, options)?;
                    writer.write_all(&data)?;
                }
            }
        } else {
            writer.start_file(name, options)?;
            io::copy(reader, &mut writer)?;
        }

        Ok(())
    })
    .with_context(|| format!("failed to process {:?}", input))?;

    if write_manifest || existing_manifest.is_some() {
        writer.start_file(MANIFEST_NAME, options)?;
        writer.write_all(build_manifest(existing_manifest.as_deref(), &modified).as_bytes())?;
    }

    writer.finish()?;
    tmp_zip
        .persist(output)
        .with_context(|| format!("failed to write {:?}", output))?;

    progress.finish_with_message(format!(
        "done ({} modified)",
        modified.len()
    ));
    info!("wrote {:?} ({} member(s) modified)", output, modified.len());
    Ok(())
}

// ── Progress bar ─────────────────────────────────────────────────────────────

fn make_progress_bar(mp: &MultiProgress, input: &Path, no_progress: bool) -> ProgressBar {
    if no_progress {
        return ProgressBar::hidden();
    }

    // For ZIP and 7z we can get the total cheaply by reading archive metadata.
    // For RAR and tar we'd need a full sequential scan, so use a spinner instead.
    let total = member_count(input);

    let pb = match total {
        Some(n) => {
            let pb = mp.add(ProgressBar::new(n as u64));
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}",
                )
                .unwrap()
                .progress_chars("=>-"),
            );
            pb
        }
        None => {
            let pb = mp.add(ProgressBar::new_spinner());
            pb.set_style(
                ProgressStyle::with_template("{spinner:.green} {pos} members {msg}")
                    .unwrap(),
            );
            pb
        }
    };

    pb.set_message("...");
    pb
}

/// Return the number of members in an archive if it can be determined cheaply
/// (without scanning the whole file). Returns `None` for formats that require
/// a sequential scan (RAR, tar) — use a spinner for those.
fn member_count(path: &Path) -> Option<usize> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();

    // ZIP: central directory at end of file — O(1) seek.
    // Count only files; directories are added to the output but don't increment
    // the progress counter, so including them inflates the total.
    if name.ends_with(".zip") {
        let mut archive = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
        let mut count = 0usize;
        for i in 0..archive.len() {
            if let Ok(entry) = archive.by_index_raw(i) {
                if !entry.is_dir() {
                    count += 1;
                }
            }
        }
        return Some(count);
    }

    // 7z: header block at end of file — reads metadata only, no decompression.
    if name.ends_with(".7z") {
        let archive = sevenz_rust2::Archive::open(path).ok()?;
        return Some(archive.files.iter().filter(|f| !f.is_directory()).count());
    }

    // RAR / tar: headers are sequential — counting requires a full pass.
    None
}

fn build_self_cmd(
    exe: &Path,
    input: &Path,
    output: &Path,
    write_manifest: bool,
    nested: NestedMode,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(input).arg("--output").arg(output);
    if let Ok(cfg_path) = std::env::var("ARCHIVE_REPACK_CONFIG") {
        cmd.arg("--config").arg(cfg_path);
    }
    if write_manifest {
        cmd.arg("--write-manifest");
    }
    let nested_str = match nested {
        NestedMode::Repack => "repack",
        NestedMode::Flatten => "flatten",
        NestedMode::Passthrough => "passthrough",
    };
    cmd.arg("--nested").arg(nested_str);
    cmd
}

fn pdf_has_ocrmypdf_stamp(data: &[u8]) -> bool {
    data.windows(8).any(|w| w.eq_ignore_ascii_case(b"ocrmypdf"))
}

fn build_manifest(prior: Option<&str>, modified: &[String]) -> String {
    let mut s = String::new();
    if let Some(prior) = prior {
        s.push_str(prior.trim_end());
        s.push('\n');
        s.push_str("---\n");
    }
    s.push_str(&format!("archive-assistant v{}\n", VERSION));
    s.push_str(&format!("processed: {}\n", Utc::now().format("%Y-%m-%dT%H:%M:%SZ")));
    for name in modified {
        s.push_str(&format!("modified: {}\n", name));
    }
    s
}
