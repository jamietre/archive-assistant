//! Integration tests for archive-repack.
//!
//! Each test builds one or more in-memory ZIPs, writes them to a tempdir,
//! invokes the archive-repack binary, and inspects the output ZIP.

use std::io::{Cursor, Write};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const BIN: &str = env!("CARGO_BIN_EXE_archive-repack");

// ── Helpers ───────────────────────────────────────────────────────────────────

fn stored() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)
}

/// Build a ZIP in memory from (path, bytes) pairs.
fn build_zip(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    for &(name, data) in members {
        if name.ends_with('/') {
            w.add_directory(name, stored()).unwrap();
        } else {
            w.start_file(name, stored()).unwrap();
            w.write_all(data).unwrap();
        }
    }
    w.finish().unwrap().into_inner()
}

/// Run archive-repack with the given args plus --no-progress.
fn repack(args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .arg("--no-progress")
        .output()
        .expect("failed to spawn archive-repack")
}

/// Return all file (non-directory) member names in a ZIP.
fn zip_names(path: &Path) -> Vec<String> {
    let mut a = ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    (0..a.len())
        .map(|i| a.by_index_raw(i).unwrap().name().to_owned())
        .filter(|n| !n.ends_with('/'))
        .collect()
}

/// Read the bytes of a specific ZIP member.
fn zip_read(path: &Path, member: &str) -> Vec<u8> {
    let mut a = ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut e = a.by_name(member).unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut e, &mut buf).unwrap();
    buf
}

// ── PDF stubs ─────────────────────────────────────────────────────────────────
//
// These are not valid PDFs that ocrmypdf can process — they only contain the
// specific byte sequences that our detection logic scans for.

fn plain_pdf() -> Vec<u8> {
    b"%PDF-1.4\n%%EOF\n".to_vec()
}

/// Has `/Encrypt` near the end — triggers our encrypted-PDF detection.
fn encrypted_pdf() -> Vec<u8> {
    let mut v = plain_pdf();
    v.extend_from_slice(b"\n/Encrypt 99 0 R\n");
    v
}

/// Has `OCRmyPDF` in the body — triggers our already-OCR'd detection.
fn ocrd_pdf() -> Vec<u8> {
    let mut v = plain_pdf();
    v.extend_from_slice(b"\n<CreatorTool>OCRmyPDF 16.0</CreatorTool>\n");
    v
}

// ── Exclusion ─────────────────────────────────────────────────────────────────

#[test]
fn test_exclude_cli_flags() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[
        ("docs/readme.txt",          b"hello"),
        (".nuget/packages/foo.dll",  b"binary"),
        ("src/main.rs",              b"fn main() {}"),
        ("node_modules/index.js",    b"module.exports = {}"),
    ])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--exclude", "**/.nuget/**",
        "--exclude", "**/node_modules/**",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let names = zip_names(&output);
    assert!(names.contains(&"docs/readme.txt".to_owned()));
    assert!(names.contains(&"src/main.rs".to_owned()));
    assert!(!names.iter().any(|n| n.contains(".nuget")), ".nuget should be excluded");
    assert!(!names.iter().any(|n| n.contains("node_modules")), "node_modules should be excluded");
}

#[test]
fn test_exclude_config_file() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[
        ("keep.txt",         b"keep me"),
        (".git/config",      b"[core]"),
        (".vs/settings.json", b"{}"),
    ])).unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, r#"exclude = ["**/.git/**", "**/.vs/**"]"#).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--config", config.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let names = zip_names(&output);
    assert!(names.contains(&"keep.txt".to_owned()));
    assert!(!names.iter().any(|n| n.contains(".git")), ".git should be excluded");
    assert!(!names.iter().any(|n| n.contains(".vs")), ".vs should be excluded");
}

#[test]
fn test_exclude_config_and_cli_combined() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[
        ("keep.txt",              b"keep"),
        (".git/HEAD",             b"ref: refs/heads/main"),
        ("dist/bundle.js",        b"(function(){})()"),
        ("Thumbs.db",             b"binary"),
    ])).unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, r#"exclude = ["**/.git/**"]"#).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--config", config.to_str().unwrap(),
        "--exclude", "**/dist/**",
        "--exclude", "Thumbs.db",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let names = zip_names(&output);
    assert!(names.contains(&"keep.txt".to_owned()));
    assert!(!names.iter().any(|n| n.contains(".git")));
    assert!(!names.iter().any(|n| n.contains("dist")));
    assert!(!names.contains(&"Thumbs.db".to_owned()));
}

// ── PDF pre-checks ────────────────────────────────────────────────────────────
//
// These tests use a processor rule (`--shell "printf 'MODIFIED'"`) that replaces
// any PDF it processes with the bytes "MODIFIED". Skip logic is verified by
// checking that the output bytes are unchanged.

#[test]
fn test_encrypted_pdf_not_processed() {
    let tmp = TempDir::new().unwrap();
    let enc = encrypted_pdf();
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[("doc.pdf", &enc)])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--match", "*.pdf",
        "--shell", "printf 'MODIFIED'",
        "--io", "stdin-stdout",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(zip_read(&output, "doc.pdf"), enc, "encrypted PDF should be passed through unchanged");
}

#[test]
fn test_ocrd_pdf_not_processed() {
    let tmp = TempDir::new().unwrap();
    let ocrd = ocrd_pdf();
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[("doc.pdf", &ocrd)])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--match", "*.pdf",
        "--shell", "printf 'MODIFIED'",
        "--io", "stdin-stdout",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(zip_read(&output, "doc.pdf"), ocrd, "already-OCR'd PDF should be passed through unchanged");
}

#[test]
fn test_plain_pdf_is_processed() {
    // Verify the processor rule actually fires when neither skip condition is met.
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[("doc.pdf", &plain_pdf())])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--match", "*.pdf",
        "--shell", "printf 'MODIFIED'",
        "--io", "stdin-stdout",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(zip_read(&output, "doc.pdf"), b"MODIFIED", "plain PDF should be processed by the rule");
}

// ── Nested archives ───────────────────────────────────────────────────────────

#[test]
fn test_nested_passthrough() {
    let tmp = TempDir::new().unwrap();
    let inner = build_zip(&[("inner.txt", b"inner content")]);
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[
        ("outer.txt",    b"outer content"),
        ("sub/inner.zip", &inner),
    ])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--nested", "passthrough",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let names = zip_names(&output);
    assert!(names.contains(&"outer.txt".to_owned()));
    assert!(names.contains(&"sub/inner.zip".to_owned()), "inner ZIP should be present as-is");

    // Inner ZIP must still be a valid readable archive.
    let inner_bytes = zip_read(&output, "sub/inner.zip");
    let inner_archive = ZipArchive::new(Cursor::new(inner_bytes)).unwrap();
    assert_eq!(inner_archive.len(), 1, "inner ZIP should have 1 member");
}

#[test]
fn test_nested_flatten() {
    let tmp = TempDir::new().unwrap();
    let inner = build_zip(&[("file.txt", b"from inner")]);
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[("sub/inner.zip", &inner)])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--nested", "flatten",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let names = zip_names(&output);
    assert!(!names.contains(&"sub/inner.zip".to_owned()), "inner ZIP should be gone after flatten");
    assert!(names.contains(&"sub/inner/file.txt".to_owned()), "flattened content should appear");
}

#[test]
fn test_nested_repack() {
    let tmp = TempDir::new().unwrap();
    let inner = build_zip(&[("data.txt", b"payload")]);
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[("archive/inner.zip", &inner)])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--nested", "repack",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let names = zip_names(&output);
    // inner.zip is already a ZIP so it stays as inner.zip (no format change).
    assert!(names.contains(&"archive/inner.zip".to_owned()), "repacked inner ZIP should be present");

    // The repacked inner ZIP should still be readable.
    let inner_bytes = zip_read(&output, "archive/inner.zip");
    let inner_archive = ZipArchive::new(Cursor::new(inner_bytes)).unwrap();
    assert_eq!(inner_archive.len(), 1);
}

// ── Manifest ──────────────────────────────────────────────────────────────────

#[test]
fn test_manifest_written() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[("a.txt", b"hello")])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--write-manifest",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let names = zip_names(&output);
    assert!(names.contains(&"archive-assistant.txt".to_owned()));

    let text = String::from_utf8(zip_read(&output, "archive-assistant.txt")).unwrap();
    assert!(text.contains("archive-assistant v"), "manifest should contain version");
    assert!(text.contains("processed:"), "manifest should contain timestamp");
}

#[test]
fn test_manifest_appended() {
    let tmp = TempDir::new().unwrap();
    let prior = "archive-assistant v0.0.1\nprocessed: 2026-01-01T00:00:00Z\n";
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[
        ("a.txt",                 b"hello"),
        ("archive-assistant.txt", prior.as_bytes()),
    ])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        "--write-manifest",
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let text = String::from_utf8(zip_read(&output, "archive-assistant.txt")).unwrap();
    assert!(text.contains("2026-01-01T00:00:00Z"), "prior manifest content should be preserved");
    assert!(text.contains("---"), "runs should be separated by ---");
    assert_eq!(text.matches("processed:").count(), 2, "should have two processed: entries");
}

#[test]
fn test_manifest_preserved_without_flag() {
    // An existing manifest in the input is always carried forward, even
    // without --write-manifest.
    let tmp = TempDir::new().unwrap();
    let prior = "archive-assistant v0.0.1\nprocessed: 2026-01-01T00:00:00Z\n";
    let input = tmp.path().join("input.zip");
    std::fs::write(&input, build_zip(&[
        ("a.txt",                 b"hello"),
        ("archive-assistant.txt", prior.as_bytes()),
    ])).unwrap();
    let output = tmp.path().join("output.zip");

    let out = repack(&[
        input.to_str().unwrap(),
        "--output", output.to_str().unwrap(),
        // intentionally no --write-manifest
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        zip_names(&output).contains(&"archive-assistant.txt".to_owned()),
        "manifest should be preserved even without --write-manifest"
    );
}

// ── Versioned output naming ───────────────────────────────────────────────────

#[test]
fn test_versioned_output_initial() {
    // foo.zip → foo.2.zip when no --output is given.
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("foo.zip");
    std::fs::write(&input, build_zip(&[("a.txt", b"hi")])).unwrap();

    let out = repack(&[input.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(tmp.path().join("foo.2.zip").exists(), "foo.2.zip should be created");
    assert!(!tmp.path().join("foo.zip.zip").exists());
}

#[test]
fn test_versioned_output_increments() {
    // foo.2.zip → foo.3.zip, not foo.2.2.zip.
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("foo.2.zip");
    std::fs::write(&input, build_zip(&[("a.txt", b"hi")])).unwrap();

    let out = repack(&[input.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(tmp.path().join("foo.3.zip").exists(), "foo.3.zip should be created");
    assert!(!tmp.path().join("foo.2.2.zip").exists(), "foo.2.2.zip must not be created");
}

#[test]
fn test_versioned_output_skips_existing() {
    // If foo.2.zip already exists, the output should be foo.3.zip.
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("foo.zip");
    std::fs::write(&input, build_zip(&[("a.txt", b"hi")])).unwrap();
    // Pre-create foo.2.zip so it must be skipped.
    std::fs::write(tmp.path().join("foo.2.zip"), b"placeholder").unwrap();

    let out = repack(&[input.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(tmp.path().join("foo.3.zip").exists(), "foo.3.zip should be created when foo.2.zip exists");
}
