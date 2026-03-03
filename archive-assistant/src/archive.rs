use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use processor::{apply_rule, Config, ProcessResult};

pub const MANIFEST_NAME: &str = "archive-assistant.txt";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// True if the file extension indicates a supported archive format.
pub fn is_archive(path: &Path) -> bool {
    archive_kind(path).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    SevenZip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    Gz,
    Bz2,
    Xz,
    Rar,
}

pub fn archive_kind(path: &Path) -> Option<ArchiveKind> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Some(ArchiveKind::TarGz)
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
        Some(ArchiveKind::TarBz2)
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        Some(ArchiveKind::TarXz)
    } else {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "zip" => Some(ArchiveKind::Zip),
            "7z" => Some(ArchiveKind::SevenZip),
            "tar" => Some(ArchiveKind::Tar),
            "gz" => Some(ArchiveKind::Gz),
            "bz2" => Some(ArchiveKind::Bz2),
            "xz" => Some(ArchiveKind::Xz),
            "rar" => Some(ArchiveKind::Rar),
            _ => None,
        }
    }
}

pub fn format_name(kind: ArchiveKind) -> &'static str {
    match kind {
        ArchiveKind::Zip => "zip",
        ArchiveKind::SevenZip => "7z",
        ArchiveKind::Tar => "tar",
        ArchiveKind::TarGz => "tar.gz",
        ArchiveKind::TarBz2 => "tar.bz2",
        ArchiveKind::TarXz => "tar.xz",
        ArchiveKind::Gz => "gz",
        ArchiveKind::Bz2 => "bz2",
        ArchiveKind::Xz => "xz",
        ArchiveKind::Rar => "rar",
    }
}

/// A flat in-memory member of an archive.
pub struct Member {
    pub name: String,
    pub data: Vec<u8>,
    pub is_dir: bool,
}

/// Extract all members from an archive of any supported format into a flat list.
pub fn extract(path: &Path) -> Result<Vec<Member>> {
    let kind = archive_kind(path).context("not a recognised archive format")?;
    match kind {
        ArchiveKind::Zip => extract_zip(path),
        ArchiveKind::SevenZip => extract_7z(path),
        ArchiveKind::Tar => extract_tar(path, None),
        ArchiveKind::TarGz => {
            let file = std::fs::File::open(path)?;
            let gz = flate2::read::GzDecoder::new(file);
            extract_tar_reader(gz)
        }
        ArchiveKind::TarBz2 => {
            let file = std::fs::File::open(path)?;
            let bz = bzip2::read::BzDecoder::new(file);
            extract_tar_reader(bz)
        }
        ArchiveKind::TarXz => {
            let file = std::fs::File::open(path)?;
            let xz = xz2::read::XzDecoder::new(file);
            extract_tar_reader(xz)
        }
        ArchiveKind::Gz => {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("file");
            let file = std::fs::File::open(path)?;
            let mut gz = flate2::read::GzDecoder::new(file);
            let mut data = Vec::new();
            gz.read_to_end(&mut data)?;
            Ok(vec![Member { name: stem.to_owned(), data, is_dir: false }])
        }
        ArchiveKind::Bz2 => {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("file");
            let file = std::fs::File::open(path)?;
            let mut bz = bzip2::read::BzDecoder::new(file);
            let mut data = Vec::new();
            bz.read_to_end(&mut data)?;
            Ok(vec![Member { name: stem.to_owned(), data, is_dir: false }])
        }
        ArchiveKind::Xz => {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("file");
            let file = std::fs::File::open(path)?;
            let mut xz = xz2::read::XzDecoder::new(file);
            let mut data = Vec::new();
            xz.read_to_end(&mut data)?;
            Ok(vec![Member { name: stem.to_owned(), data, is_dir: false }])
        }
        ArchiveKind::Rar => extract_rar(path),
    }
}

fn extract_zip(path: &Path) -> Result<Vec<Member>> {
    let file = std::fs::File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
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
    sevenz_rust2::decompress_file_with_extract_fn(path, Path::new("/dev/null"), |entry, reader, _| {
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
        members.push(Member {
            name: entry.name().to_owned(),
            data,
            is_dir: false,
        });
        Ok(true)
    })?;
    Ok(members)
}

fn extract_tar(path: &Path, _compression: Option<()>) -> Result<Vec<Member>> {
    let file = std::fs::File::open(path)?;
    extract_tar_reader(file)
}

fn extract_tar_reader<R: Read>(reader: R) -> Result<Vec<Member>> {
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
    // unrar extracts to a temp directory; we collect from there.
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
    let mut members = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(root)?.to_string_lossy().into_owned();
        if path.is_dir() {
            members.push(Member { name: format!("{}/", rel), data: Vec::new(), is_dir: true });
            members.extend(collect_dir(root, &path)?);
        } else {
            let data = std::fs::read(&path)?;
            members.push(Member { name: rel, data, is_dir: false });
        }
    }
    Ok(members)
}

/// Process an archive: extract, apply processors to members, repack as ZIP.
/// Returns true if the archive was modified (and written back to source_path).
pub fn process_archive(
    source_path: &Path,
    config: &Config,
    process_pdfs: bool,
    dry_run: bool,
) -> Result<bool> {
    let kind = archive_kind(source_path).context("not a recognised archive format")?;

    // For ZIP, check for manifest first (cheap).
    if kind == ArchiveKind::Zip {
        let file = std::fs::File::open(source_path)?;
        if let Ok(mut za) = ZipArchive::new(file) {
            if za.by_name(MANIFEST_NAME).is_ok() {
                tracing::info!("{:?}: already processed (manifest present)", source_path);
                return Ok(false);
            }
        }
    }

    // Copy to local temp for processing.
    let tmp_dir = tempfile::TempDir::new()?;
    let tmp_archive = tmp_dir.path().join(
        source_path.file_name().unwrap_or_default()
    );
    std::fs::copy(source_path, &tmp_archive)?;

    // Extract.
    let mut members = extract(&tmp_archive)
        .with_context(|| format!("failed to extract {:?}", source_path))?;

    let mut modified_names: Vec<String> = Vec::new();

    // Apply processors to members.
    if process_pdfs {
        for member in &mut members {
            if member.is_dir {
                continue;
            }
            let filename = Path::new(&member.name)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&member.name);

            if let Some(rule) = config.find_rule(filename) {
                tracing::info!("  processing archive member: {}", member.name);
                if !dry_run {
                    match apply_rule(rule, &member.data, filename)? {
                        ProcessResult::Modified(new_data) => {
                            modified_names.push(member.name.clone());
                            member.data = new_data;
                        }
                        ProcessResult::Unchanged => {}
                    }
                } else {
                    tracing::info!("  [dry-run] would process: {}", member.name);
                }
            }
        }
    }

    // Non-ZIP formats must always be converted; ZIP only rewritten if content changed.
    let needs_repack = kind != ArchiveKind::Zip || !modified_names.is_empty();

    if !needs_repack {
        tracing::info!("{:?}: no changes needed", source_path);
        return Ok(false);
    }

    if dry_run {
        tracing::info!("{:?}: [dry-run] would repack as ZIP", source_path);
        return Ok(false);
    }

    // Build the output ZIP path (same dir as source, .zip extension).
    let zip_name = source_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
        + ".zip";
    let zip_path = source_path.with_file_name(&zip_name);

    let manifest = build_manifest(
        &modified_names,
        if kind != ArchiveKind::Zip { Some(format_name(kind)) } else { None },
    );

    // Write new ZIP.
    let tmp_zip = tempfile::NamedTempFile::new_in(tmp_dir.path())?;
    {
        let mut writer = ZipWriter::new(tmp_zip.reopen()?);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        for member in &members {
            if member.is_dir {
                writer.add_directory(&member.name, options)?;
            } else {
                writer.start_file(&member.name, options)?;
                writer.write_all(&member.data)?;
            }
        }

        writer.start_file(MANIFEST_NAME, options)?;
        writer.write_all(manifest.as_bytes())?;
        writer.finish()?;
    }

    // Copy result back to destination.
    std::fs::copy(tmp_zip.path(), &zip_path)?;

    // If format changed, delete original.
    if kind != ArchiveKind::Zip {
        std::fs::remove_file(source_path)?;
        tracing::info!("{:?}: converted to {:?}", source_path, zip_path);
    } else {
        tracing::info!("{:?}: repacked ({} member(s) modified)", source_path, modified_names.len());
    }

    Ok(true)
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
