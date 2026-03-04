# Changelog

## Unreleased

### archive-repack

- **`exclude` patterns**: members whose full path matches any glob in the `exclude` config key (or `--exclude` CLI flag, repeatable) are omitted from the output ZIP. Patterns are matched against the full member path so directory-prefix patterns like `**/.nuget/**` work correctly. Excluded members are still counted by the progress bar.
- **`--nested` default changed to `passthrough`**: nested archives inside the input are now copied unchanged by default. Use `--nested repack` or `--nested flatten` to get the previous recursive-repack behaviour.
- **Versioned output for ZIP inputs**: when no `--output` is given and the input is already a ZIP, the output is named `foo.2.zip`, `foo.3.zip`, etc. (incrementing the existing suffix rather than appending a new one).
- **Manifest append**: if the input archive already contains an `archive-assistant.txt` manifest from a previous run, the new run's entry is appended after a `---` separator rather than replacing it. The manifest is preserved even when `--write-manifest` is not passed.
- **PDF idempotency**: PDFs that already carry an ocrmypdf stamp (case-insensitive search for `OCRmyPDF` in the XMP metadata) are skipped rather than re-processed, making repeated runs idempotent.
- **Progress bar counts files only**: the ZIP/7z member count used for the progress bar total now excludes directory entries, matching what the counter actually increments.
- **Subprocess stderr routed through tracing**: stderr from processor commands is captured and emitted via `debug!` (visible with `--verbose`) instead of leaking directly to the terminal. On failure the stderr is included in the error/warn message.
- **7z checksum errors fixed**: excluded members now drain their reader before returning, preventing false `ChecksumVerificationFailed` errors from sevenz-rust2.

### archive-assistant

- **PDF idempotency stamp check**: `pdf_has_ocrmypdf_stamp` now uses a case-insensitive byte search (`OCRmyPDF` vs `ocrmypdf`), fixing the check that was silently never matching.

### Config

- **`exclude` key**: top-level array of glob patterns in the TOML config. Must appear before any `[[processor]]` sections (standard TOML scoping rule).
