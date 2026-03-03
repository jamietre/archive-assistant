# archive-assistant Plan

## Overview

Two standalone CLI tools in a Cargo workspace:

1. **`zip-rewriter`** — processes members of a ZIP file using a config-driven pipeline
   of external tools. Standalone and independently useful.

2. **`archive-assistant`** — walks a directory tree, converts non-ZIP archives to ZIP,
   and invokes `zip-rewriter` logic on each archive. Designed for NAS preprocessing.

---

## `zip-rewriter`

### What it does

Opens a ZIP file, iterates members, applies configured processors to matching files,
repacks the ZIP with processed results, and writes an `archive-assistant.txt` manifest.

### CLI

```
zip-rewriter [OPTIONS] <ZIP_FILE>

Arguments:
  <ZIP_FILE>    ZIP file to process

Options:
  --config <PATH>       Config file [default: zip-rewrite.toml in CWD]
  --dry-run             Print what would be done without modifying the file
  --verbose             Log each member being processed
  --output <PATH>       Write result to a different path instead of in-place
```

### Config file (`zip-rewrite.toml`)

```toml
# Explicit chain: list of processors applied in sequence for matching members
[[processor]]
match = "*.pdf"
chain = [
    { io = "in-place", command = "ocrmypdf", args = ["--skip-text", "--quiet", "{input}", "{input}"] },
]

# File-to-file example
[[processor]]
match = "*.png"
chain = [
    { io = "file-to-file", command = "convert", args = ["{input}", "{output}"] },
]

# Shell passthrough — arbitrary pipeline, escape hatch for complex cases
[[processor]]
match = "*.txt"
shell = "cat {input} | tr '[:lower:]' '[:upper:]'"
io = "stdin-stdout"   # describes how the shell expression produces output
```

### I/O modes

| Mode | Description |
|------|-------------|
| `in-place` | `{input}` == `{output}`. Tool modifies file at the given path. Read back same path. |
| `file-to-file` | Separate `{input}` and `{output}` temp paths. Read back `{output}`. |
| `file-to-stdout` | Pass `{input}` path as arg. Capture stdout as result. |
| `stdin-stdout` | Pipe extracted bytes to stdin. Capture stdout as result. |

For explicit `chain`, each step's output feeds the next step's input. The final
step's output is the new member content. The mode applies per-step.

For `shell`, the expression is passed to `sh -c "..."` with `{input}` substituted
with the extracted temp path. Output is captured from stdout. This is the escape
hatch for arbitrary piped shell commands.

### Manifest (`archive-assistant.txt`)

Written into the ZIP after processing:

```
archive-assistant v0.1.0
processed: 2026-03-03T12:34:56Z
ocr: taxes/w2.pdf
ocr: taxes/1040.pdf
```

Written only when at least one member was actually modified. If no processors
matched or all members were skipped, the ZIP is not repacked and is unchanged.

If `archive-assistant.txt` already exists in the ZIP, `zip-rewriter` skips the
file entirely (idempotent). Pass `--force` to reprocess anyway.

---

## `archive-assistant`

### What it does

Walks a directory tree and preprocesses every archive for compatibility with
`find-anything`:

- **OCRs top-level image-only PDFs** using `ocrmypdf`
- **Converts non-ZIP archives** (7z, tar, tar.gz, rar, etc.) to ZIP
- **Processes PDFs inside archives** — uses `zip-rewriter` with the OCR processor
- **Idempotent** — state DB + embedded manifest

### CLI

```
archive-assistant [OPTIONS] <PATH>

Arguments:
  <PATH>    Directory to process

Options:
  --state-db <PATH>     SQLite database for tracking processed files
  --temp-dir <PATH>     Local temp directory [default: system temp]
  --dry-run             Print what would be done without modifying files
  --ocr-only            Only OCR PDFs, skip archive conversion
  --convert-only        Only convert archives, skip OCR
  --no-archive-pdfs     Don't process PDFs inside archives
  --jobs <N>            Parallel workers [default: CPUs / 2]
  --verbose             Log each file processed
```

### Design decisions

#### Running over SMB

The NAS is ARM-based and cannot run the tool directly. All heavy processing uses
a **local temp directory** — files are copied from SMB to local temp, processed
entirely locally, then written back. This avoids excessive SMB round-trips.

Write-back uses `fs::copy` + delete rather than rename (cross-filesystem rename fails).

#### mtime+60s convention

After modifying a file, mtime is set to `original_mtime + 60s`. This ensures
`find-scan` sees the file as changed and re-indexes it, without appreciably altering
the timestamp for other purposes.

#### State database (`--state-db`)

```sql
CREATE TABLE processed (
    path         TEXT    NOT NULL PRIMARY KEY,
    mtime        INTEGER NOT NULL,
    processed_at INTEGER NOT NULL
);
```

`(path, mtime)` is checked before any I/O. A record is written after any outcome
(modified or checked-and-skipped). Stale records (mtime changed) are re-processed.
Provides crash resumability for long runs.

#### Top-level file processing

For each non-archive file encountered by the walk, `archive-assistant` checks the
processor config (same as `zip-rewriter`) and applies matching processors. Before
invoking a processor, it applies cheap skip checks to avoid unnecessary I/O:

0. State DB hit `(path, mtime)` — skip with no I/O
1. Copy to local temp (needed for processing; avoids repeated SMB reads)
2. For `.pdf` specifically: run `pdftotext file.pdf -` — if stdout non-empty, has
   a text layer → skip processor invocation, record in DB
3. For `.pdf` specifically: check XMP via `lopdf` for ocrmypdf stamp → skip, record in DB
4. Invoke processor chain as configured, write back result, set mtime+60s, record in DB

The PDF-specific checks (steps 2–3) are `archive-assistant`-level optimisations,
not part of the shared processor library. The processor itself also gets
`--skip-text` so ocrmypdf won't re-OCR if text is already present (belt-and-suspenders).

#### Archive idempotency

- **Non-ZIP**: always process (if it exists, archive-assistant hasn't seen it)
- **ZIP**: check for `archive-assistant.txt` inside — if present, skip

#### Archive conversion

1. Copy to local temp
2. Extract all members to a second local temp dir
3. Repack as `{original_name}.zip` in local temp, with manifest
4. Copy ZIP back to source, delete original, set mtime+60s

Supported input formats: 7z, tar, tar.gz, tar.bz2, tar.xz, gz, bz2, xz, rar

**RAR**: Extracted via `unrar` CLI (external dependency).

---

## Shared config format

Both tools use the same config format and the same processor dispatch logic,
implemented in a shared `processor` library crate.

`archive-assistant` uses the processor config for **top-level files** encountered
during the directory walk (e.g. OCR a `.pdf` directly on disk). `zip-rewriter`
uses the same config for **ZIP members**. The config file is the same file — both
tools accept `--config <PATH>` pointing at it.

Example config used by both:

```toml
[[processor]]
match = "*.pdf"
chain = [
    { io = "in-place", command = "ocrmypdf", args = ["--skip-text", "--quiet", "{input}", "{input}"] },
]

[[processor]]
match = "*.txt"
shell = "cat {input} | sed 's/foo/bar/g'"
io = "stdin-stdout"
```

When `archive-assistant` encounters a top-level `.pdf`, it applies the `*.pdf`
processor chain exactly as `zip-rewriter` would for a ZIP member — copy to temp,
run, write back, set mtime+60s.

---

## Workspace structure

```
archive-assistant/         (workspace root)
  Cargo.toml               (workspace manifest)
  processor/               shared library crate
    Cargo.toml
    src/
      lib.rs               re-exports
      config.rs            Config file parsing (TOML): ProcessorConfig, ChainStep, IoMode
      dispatch.rs          Processor dispatch: chain, shell, all I/O modes
  zip-rewriter/
    Cargo.toml
    src/
      main.rs              CLI, zip open/repack loop, manifest writing
  archive-assistant/
    Cargo.toml
    src/
      main.rs              CLI, walk loop, dispatch
      archive.rs           Archive extraction, format conversion (non-ZIP → ZIP)
      state.rs             SQLite state database
      mtime.rs             mtime read/set (utimes on Linux)
```

---

## Dependencies

### processor (shared lib)

```toml
toml         = "0.8"
serde        = { version = "1", features = ["derive"] }
glob         = "0.3"
anyhow       = "1"
tempfile     = "3"
tracing      = "0.1"
```

### zip-rewriter

```toml
processor    = { path = "../processor" }
clap         = { version = "4", features = ["derive"] }
zip          = "2"
anyhow       = "1"
tracing      = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
chrono       = "0.4"
```

### archive-assistant

```toml
processor    = { path = "../processor" }
clap         = { version = "4", features = ["derive"] }
walkdir      = "2"
zip          = "2"
tar          = "0.4"
flate2       = "1"
bzip2        = "0.4"
xz2          = "0.1"
sevenz-rust2 = "0.6"
rusqlite     = { version = "0.31", features = ["bundled"] }
anyhow       = "1"
tempfile     = "3"
rayon        = "1"
tracing      = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
chrono       = "0.4"
```

External runtime dependencies:
- `ocrmypdf` — `pipx install ocrmypdf`
- `pdftotext` — poppler-utils (`apt install poppler-utils`)
- `unrar` — for RAR extraction (`apt install unrar`)
- Tesseract language packs (`apt install tesseract-ocr-eng`)

---

## Open questions

- **Large archives**: Peak temp disk ~3× archive size (original + extracted + repacked).
  Document and recommend `--temp-dir` pointing at a large local drive.
- **Nested archives**: Recurse into archives-within-archives up to depth 10.
- **Parallel OCR**: Default `--jobs 1` for OCR-heavy runs (ocrmypdf uses multiple cores).
  Higher values useful for convert-only or mixed workloads.
