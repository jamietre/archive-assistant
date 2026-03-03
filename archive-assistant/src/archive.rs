use std::path::Path;

use zip::ZipArchive;

pub const MANIFEST_NAME: &str = "archive-assistant.txt";

/// Returns true if the file extension is a supported archive format.
pub fn is_archive(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    name.ends_with(".zip")
        || name.ends_with(".7z")
        || name.ends_with(".tar")
        || name.ends_with(".tar.gz")
        || name.ends_with(".tgz")
        || name.ends_with(".tar.bz2")
        || name.ends_with(".tbz2")
        || name.ends_with(".tar.xz")
        || name.ends_with(".txz")
        || name.ends_with(".gz")
        || name.ends_with(".bz2")
        || name.ends_with(".xz")
        || name.ends_with(".rar")
}

/// Returns true if this is a ZIP that already contains the manifest,
/// meaning archive-repack has already processed it.
pub fn zip_has_manifest(path: &Path) -> bool {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut archive = match ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return false,
    };
    let result = archive.by_name(MANIFEST_NAME);
    result.is_ok()
}

/// True if this path has a .zip extension.
pub fn is_zip(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("zip"))
        .unwrap_or(false)
}

/// The output path archive-repack will produce for a given input
/// (same directory, .zip extension, compound suffixes stripped).
pub fn repack_output_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let stem = strip_archive_suffix(&name);
    path.with_file_name(format!("{}.zip", stem))
}

fn strip_archive_suffix(name: &str) -> &str {
    for suffix in &[".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz"] {
        if let Some(s) = name.strip_suffix(suffix) {
            return s;
        }
    }
    std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
}
