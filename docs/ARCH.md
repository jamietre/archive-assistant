# Architecture

## Workspace layout

```
archive-assistant/          workspace root
  processor/                shared library crate
    src/
      lib.rs                re-exports
      config.rs             Config parsing (TOML): Config, ProcessorRule, ChainStep, IoMode
      dispatch.rs           Processor dispatch: chain execution, shell, all I/O modes
  archive-repack/           standalone archive-to-ZIP repacker
    src/
      main.rs               CLI, streaming member loop, manifest, progress bar
  archive-assistant/        directory walker and orchestrator
    src/
      main.rs               CLI, parallel walk, top-level file processing
      archive.rs            Archive type detection, ZIP manifest check
      state.rs              SQLite state database
      mtime.rs              mtime read/bump (utimes)
```

## processor (shared library)

Config is parsed from TOML into `Config { exclude, processor }`. Each `ProcessorRule`
has a glob `match` pattern, an optional `chain` of `ChainStep`s, or a `shell` expression.

`apply_rule` dispatches to `apply_chain` or `apply_shell`. Both ultimately call
`run_step`, which writes input bytes to a temp file, invokes the external command,
and reads the result back. The four I/O modes control how stdin/stdout/files are wired:

| Mode | Input | Output |
|------|-------|--------|
| `in-place` | `{input}` path (same as output) | re-read same path |
| `file-to-file` | `{input}` path | `{output}` path |
| `file-to-stdout` | `{input}` path as arg | captured stdout |
| `stdin-stdout` | piped to stdin | captured stdout |

Subprocess stderr is captured and emitted via `debug!` on success, or included in
the error on failure (never leaks to the terminal).

## archive-repack

### Streaming member loop

All archive formats are normalised through a single callback interface:

```rust
type MemberFn<'a> = dyn FnMut(&str, bool, &mut dyn Read) -> Result<()> + 'a;
```

`for_each_member` dispatches to format-specific iterators (`for_each_zip`,
`for_each_7z`, `for_each_tar`, `for_each_rar`). Only one member's data is live
at a time — members are streamed directly into the output ZIP writer without
buffering the whole archive.

**Important**: when skipping a member (excluded, already processed, encrypted),
the reader must be drained with `io::copy(reader, &mut io::sink())` before
returning. Failing to do so leaves the stream in an inconsistent state and
causes checksum errors in sevenz-rust2.

### Exclusion

Members whose full path matches any glob in `Config::exclude` (or `--exclude` flags)
are drained and skipped. The progress bar is incremented before the exclusion check
so excluded members are counted in the total.

### PDF pre-checks

Before applying any processor rule to a PDF, two in-memory byte scans are performed:

1. **Encrypted**: search the last 64 KB for `/Encrypt`. Encrypted PDFs cannot be
   processed by ocrmypdf; they are written through unchanged.
2. **Already OCR'd**: case-insensitive search for `ocrmypdf` (matches `OCRmyPDF`
   written by ocrmypdf into the XMP metadata stream). Already-processed PDFs are
   written through unchanged.

Both checks are O(n) byte scans with no dependencies — no lopdf, no subprocess.

### Manifest

`archive-assistant.txt` is written into the output ZIP when `--write-manifest` is
passed, or when the input already contained a manifest (in which case the new run
is appended after a `---` separator). The manifest member is consumed from the input
and never copied directly to the output.

### Output path for ZIP inputs

When no `--output` is given and the input is already a ZIP, writing to the same
path would overwrite the input. The default output is instead `foo.2.zip`,
`foo.3.zip`, etc. — the existing version suffix is stripped and incremented so
repeated runs produce `foo.2.zip`, `foo.3.zip`, not `foo.2.2.zip`.

### Progress bar

For ZIP and 7z, member count is determined cheaply from archive metadata (central
directory / header block) before extraction begins, giving a determinate progress
bar. For RAR and tar, headers are sequential so a spinner is used instead.
The count excludes directory entries since those don't increment the counter.

### Nested archives

`--nested` (default `passthrough`) controls nested archive handling:
- `passthrough`: copy nested archive bytes unchanged into the output ZIP
- `repack`: shell out to a fresh `archive-repack` process; the config path is
  forwarded via `ARCHIVE_REPACK_CONFIG` so deeply nested archives inherit config
- `flatten`: extract nested archive contents into a subdirectory in the output ZIP

## archive-assistant

### Walk and parallelism

`WalkDir` produces a flat list of all files under the target path. The list is
processed in parallel with Rayon (`--jobs`, default CPUs/2). Each file is handled
independently with no shared mutable state beyond the progress bar and state DB
(both behind a `Mutex`).

### Archive idempotency

- **Non-ZIP archives**: always processed — their existence means they have not yet
  been through `archive-repack` (which would have produced a ZIP).
- **ZIP archives**: inspected for `archive-assistant.txt`. If present, skipped.

### Top-level file processing

For each non-archive file matching a processor rule:

1. State DB check `(path, mtime)` — skip with no I/O if already recorded.
2. For `.pdf`: check for `/Encrypt` in file tail (64 KB seek) — skip if encrypted.
3. For `.pdf`: run `pdftotext path -` and check stdout — skip if text layer present.
4. For `.pdf`: check XMP metadata via lopdf for ocrmypdf stamp — skip if found.
5. Copy to local temp, apply processor chain, write result back, set mtime+60s,
   record in state DB.

Steps 2–4 are cheap checks that avoid spawning ocrmypdf unnecessarily.

### mtime+60s convention

After modifying a file, mtime is set to `original_mtime + 60s`. This ensures
downstream indexers (e.g. find-anything) see the file as changed and re-index it,
without appreciably altering the timestamp for other purposes.

### State database

```sql
CREATE TABLE processed (
    path         TEXT    NOT NULL PRIMARY KEY,
    mtime        INTEGER NOT NULL,
    processed_at INTEGER NOT NULL
);
```

`(path, mtime)` is checked before any I/O. A record is written after both
modified and checked-and-skipped outcomes. Stale records (mtime changed) are
re-processed. This provides crash resumability for long runs.

### SMB / network mounts

All heavy I/O (extraction, OCR, repacking) uses a local temp directory
(`--temp-dir`). Only the final result is written back to the mount, minimising
SMB round-trips. `fs::copy` + delete is used rather than rename because
cross-filesystem renames fail.

## External runtime dependencies

| Tool | Purpose | Install |
|------|---------|---------|
| `ocrmypdf` | PDF OCR | `pipx install ocrmypdf` |
| `pdftotext` | PDF text detection | `apt install poppler-utils` |
| Tesseract | OCR engine (used by ocrmypdf) | `apt install tesseract-ocr-eng` |
