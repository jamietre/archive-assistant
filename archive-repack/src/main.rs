use std::io::{Read, Write};
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
        // Strip compound extensions (.tar.gz etc) before appending .zip.
        let name = self
            .input
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let stem = archive_stem(&name);
        self.input.with_file_name(format!("{}.zip", stem))
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

    // Propagate the config path to recursive calls via env var so nested
    // archives inherit the same rules. CLI --config takes precedence over
    // any value already in the environment (set by archive-assistant).
    if let Some(cfg) = &args.config {
        // Canonicalise so the path is valid regardless of where the child runs.
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

/// Strip compound extensions (.tar.gz etc) to get the stem for naming the output ZIP.
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

// ── Extraction ───────────────────────────────────────────────────────────────

struct Member {
    name: String,
    data: Vec<u8>,
    is_dir: bool,
}

fn extract(path: &Path) -> Result<Vec<Member>> {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return extract_tar(flate2::read::GzDecoder::new(std::fs::File::open(path)?));
    }
    if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
        return extract_tar(bzip2::read::BzDecoder::new(std::fs::File::open(path)?));
    }
    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        return extract_tar(xz2::read::XzDecoder::new(std::fs::File::open(path)?));
    }

    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("zip") => extract_zip(path),
        Some("7z") => extract_7z(path),
        Some("tar") => extract_tar(std::fs::File::open(path)?),
        Some("gz") => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            let mut data = Vec::new();
            flate2::read::GzDecoder::new(std::fs::File::open(path)?).read_to_end(&mut data)?;
            Ok(vec![Member { name: stem.to_owned(), data, is_dir: false }])
        }
        Some("bz2") => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            let mut data = Vec::new();
            bzip2::read::BzDecoder::new(std::fs::File::open(path)?).read_to_end(&mut data)?;
            Ok(vec![Member { name: stem.to_owned(), data, is_dir: false }])
        }
        Some("xz") => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            let mut data = Vec::new();
            xz2::read::XzDecoder::new(std::fs::File::open(path)?).read_to_end(&mut data)?;
            Ok(vec![Member { name: stem.to_owned(), data, is_dir: false }])
        }
        Some("rar") => extract_rar(path),
        _ => bail!("unsupported archive format: {:?}", path),
    }
}

fn extract_zip(path: &Path) -> Result<Vec<Member>> {
    use zip::ZipArchive;
    let mut archive = ZipArchive::new(std::fs::File::open(path)?)?;
    let mut members = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_owned();
        if entry.is_dir() {
            members.push(Member { name, data: Vec::new(), is_dir: true });
        } else {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            members.push(Member { name, data, is_dir: false });
        }
    }
    Ok(members)
}

fn extract_7z(path: &Path) -> Result<Vec<Member>> {
    let mut members = Vec::new();
    sevenz_rust2::decompress_file_with_extract_fn(
        path,
        Path::new("/dev/null"),
        |entry, reader, _| {
            if entry.is_directory() {
                members.push(Member {
                    name: entry.name().to_owned(),
                    data: Vec::new(),
                    is_dir: true,
                });
                return Ok(true);
            }
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;
            members.push(Member { name: entry.name().to_owned(), data, is_dir: false });
            Ok(true)
        },
    )?;
    Ok(members)
}

fn extract_tar<R: Read>(reader: R) -> Result<Vec<Member>> {
    let mut archive = tar::Archive::new(reader);
    let mut members = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if entry.header().entry_type().is_dir() {
            members.push(Member { name, data: Vec::new(), is_dir: true });
        } else {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            members.push(Member { name, data, is_dir: false });
        }
    }
    Ok(members)
}

fn extract_rar(path: &Path) -> Result<Vec<Member>> {
    let tmp = tempfile::TempDir::new()?;
    let status = std::process::Command::new("unrar")
        .args(["x", "-y"])
        .arg(path)
        .arg(tmp.path())
        .status()
        .context("failed to spawn unrar (is it installed?)")?;
    if !status.success() {
        bail!("unrar exited with {}", status);
    }
    collect_dir(tmp.path(), tmp.path())
}

fn collect_dir(root: &Path, dir: &Path) -> Result<Vec<Member>> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.path());
    let mut members = Vec::new();
    for entry in entries {
        let path = entry.path();
        let rel = path.strip_prefix(root)?.to_string_lossy().into_owned();
        if path.is_dir() {
            members.push(Member { name: format!("{}/", rel), data: Vec::new(), is_dir: true });
            members.extend(collect_dir(root, &path)?);
        } else {
            members.push(Member { name: rel, data: std::fs::read(&path)?, is_dir: false });
        }
    }
    Ok(members)
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

    let members =
        extract(input).with_context(|| format!("failed to extract {:?}", input))?;

    let self_exe = std::env::current_exe()?;
    let tmp_dir = tempfile::TempDir::new()?;
    let mut out_members: Vec<(String, Vec<u8>)> = Vec::new();
    let mut modified: Vec<String> = Vec::new();

    for member in members {
        if member.is_dir {
            out_members.push((member.name, Vec::new()));
            continue;
        }

        let filename = Path::new(&member.name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&member.name);

        // Nested archive: shell out to self.
        if is_archive(filename) {
            debug!("  nested archive: {}", member.name);
            let nested_stem = archive_stem(filename).to_owned();
            let dir_prefix = member
                .name
                .strip_suffix(filename)
                .unwrap_or("")
                .to_owned();
            let nested_zip_name = format!("{}{}.zip", dir_prefix, nested_stem);

            if dry_run {
                info!("  [dry-run] would repack nested archive: {}", member.name);
                out_members.push((member.name, member.data));
                continue;
            }

            let nested_input = tmp_dir.path().join(filename);
            let nested_output = tmp_dir.path().join(format!("{}.zip", nested_stem));
            std::fs::write(&nested_input, &member.data)?;

            let status = build_self_cmd(&self_exe, config, &nested_input, &nested_output, write_manifest)
                .status()
                .with_context(|| format!("failed to spawn archive-repack for {:?}", nested_input))?;

            if !status.success() {
                warn!("archive-repack failed for nested archive {}, keeping original", member.name);
                out_members.push((member.name, member.data));
                continue;
            }

            let zip_data = std::fs::read(&nested_output)?;
            info!("  repacked nested: {} -> {}", member.name, nested_zip_name);
            modified.push(member.name.clone());
            out_members.push((nested_zip_name, zip_data));
            continue;
        }

        // Regular member: apply processor rule if one matches.
        if let Some(rule) = config.find_rule(filename) {
            info!("  processing member: {}", member.name);
            if dry_run {
                info!("  [dry-run] would apply rule '{}' to: {}", rule.r#match, member.name);
                out_members.push((member.name, member.data));
                continue;
            }

            match apply_rule(rule, &member.data, filename)? {
                ProcessResult::Modified(new_data) => {
                    info!("  modified: {}", member.name);
                    modified.push(member.name.clone());
                    out_members.push((member.name, new_data));
                }
                ProcessResult::Unchanged => {
                    debug!("  unchanged: {}", member.name);
                    out_members.push((member.name, member.data));
                }
            }
        } else {
            out_members.push((member.name, member.data));
        }
    }

    if dry_run {
        info!(
            "[dry-run] would write {:?} ({} member(s) would be modified)",
            output,
            modified.len()
        );
        return Ok(());
    }

    // Write output ZIP.
    let parent = output.parent().unwrap_or(Path::new("."));
    let tmp_zip = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut writer = ZipWriter::new(tmp_zip.reopen()?);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        for (name, data) in &out_members {
            if data.is_empty() && name.ends_with('/') {
                writer.add_directory(name, options)?;
            } else {
                writer.start_file(name, options)?;
                writer.write_all(data)?;
            }
        }

        if write_manifest {
            writer.start_file(MANIFEST_NAME, options)?;
            writer.write_all(build_manifest(&modified).as_bytes())?;
        }

        writer.finish()?;
    }

    tmp_zip
        .persist(output)
        .with_context(|| format!("failed to write {:?}", output))?;

    info!("wrote {:?} ({} member(s) modified)", output, modified.len());
    Ok(())
}

/// Build a Command that invokes archive-repack on a nested archive, forwarding
/// the config path (if any) and relevant flags. Processor rules defined only
/// as inline flags are not forwarded to the child — pass --config for recursive use.
fn build_self_cmd(
    exe: &Path,
    config: &Config,
    input: &Path,
    output: &Path,
    write_manifest: bool,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(input).arg("--output").arg(output);

    // Forward config path via the ARCHIVE_REPACK_CONFIG env var if set,
    // so recursive calls inherit it without re-parsing the in-memory config.
    if let Ok(cfg_path) = std::env::var("ARCHIVE_REPACK_CONFIG") {
        cmd.arg("--config").arg(cfg_path);
    }

    // Suppress verbose noise from child unless we're in verbose mode ourselves.
    let _ = config; // config forwarding via env var above; unused directly here

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
