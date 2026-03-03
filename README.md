# archive-assistant

A preprocessing tool for document archives. Designed to complement
[find-anything](../find-anything) by ensuring PDFs are text-searchable and archives
are in a format find-anything can browse efficiently.

Two tools in one workspace:

- **`archive-repack`** — reads any archive format, applies processor rules to members,
  writes a ZIP. Standalone and useful on its own.
- **`archive-assistant`** — walks a directory tree, decides which archives need
  processing, calls `archive-repack`, and manages idempotency (state DB, mtime+60s).

## What it does

- **OCRs image-only PDFs** — embeds a text layer so content is searchable
- **Converts any archive to ZIP** — 7z, tar, tar.gz, tar.bz2, tar.xz, rar → ZIP
- **Processes files inside archives** — extracts, applies rules, repacks
- **Handles nested archives** — `archive-repack` shells out to itself recursively
- **Idempotent** — ZIPs with an embedded `archive-assistant.txt` manifest are skipped;
  optional SQLite state DB for top-level files

## Requirements

```sh
# OCR support (pipx manages the venv automatically)
pipx install ocrmypdf
apt install tesseract-ocr-eng   # or other language packs as needed

# PDF text detection
apt install poppler-utils       # provides pdftotext

# RAR extraction is handled by the unrar Rust crate (no external binary needed),
# which requires the unrar shared library on some systems:
# apt install libunrar-dev   # if the build fails looking for unrar headers
```

## Config file

Both tools use the same TOML config to define what happens to each file type.

```toml
# zip-rewrite.toml

# OCR image-only PDFs using ocrmypdf (in-place)
[[processor]]
match = "*.pdf"
chain = [
    { io = "in-place", command = "ocrmypdf", args = ["--skip-text", "--quiet", "{input}", "{input}"] },
]

# Shell passthrough example — arbitrary pipeline via sh -c
[[processor]]
match = "*.txt"
shell = "cat {input} | tr '[:lower:]' '[:upper:]'"
io = "stdin-stdout"
```

### I/O modes

| Mode | Description |
|------|-------------|
| `in-place` | Tool modifies the file at `{input}` directly |
| `file-to-file` | Tool reads `{input}`, writes to `{output}` |
| `file-to-stdout` | Tool reads `{input}`, result captured from stdout |
| `stdin-stdout` | Input piped to stdin, result captured from stdout |

For `chain`, each step's output feeds the next. For `shell`, the expression is
passed to `sh -c` with `{input}` substituted.

## `archive-repack`

Reads any archive, applies processor rules to members, writes a ZIP.
Nested archives found inside are recursively repacked by shelling out to itself.

```sh
# From a config file
archive-repack input.7z --config zip-rewrite.toml

# Inline rule — no config file needed
archive-repack input.7z \
  --match '*.pdf' \
  --command ocrmypdf \
  --arg '--skip-text' --arg '--quiet' --arg '{input}' --arg '{input}'

# Inline shell expression
archive-repack input.tar.gz \
  --match '*.txt' \
  --shell 'cat {input} | tr a-z A-Z' \
  --io stdin-stdout

# Combine: inline rule runs first, then config-file rules
archive-repack input.zip \
  --config zip-rewrite.toml \
  --match '*.png' --command convert --arg '{input}' --arg '{output}' --io file-to-file

# Write manifest into output ZIP (archive-assistant always passes this)
archive-repack input.7z --config zip-rewrite.toml --write-manifest

# Dry run
archive-repack --dry-run input.7z --config zip-rewrite.toml

# Explicit output path
archive-repack input.tar.gz --config zip-rewrite.toml --output /tmp/repacked.zip
```

### Options

```
archive-repack [OPTIONS] <INPUT>

Arguments:
  <INPUT>    Input archive (any supported format)

Options:
  --output <PATH>       Output ZIP path [default: input stem + .zip, same directory]
  --config <PATH>       Config file defining processor rules

Inline rule (alternative or supplement to --config):
  --match <GLOB>        Filename pattern [default: * when --command/--shell given]
  --command <CMD>       Command to run on matching members
  --arg <ARG>           Argument for the command (repeatable); use {input}, {output}
  --io <MODE>           I/O mode: in-place, file-to-file, file-to-stdout, stdin-stdout
                        [default: in-place]
  --shell <EXPR>        Shell expression via sh -c (alternative to --command)

General:
  --write-manifest      Embed archive-assistant.txt manifest in the output ZIP
  --dry-run             Print what would be done without writing any output
  --verbose             Log each member being processed
```

If `ARCHIVE_REPACK_CONFIG` is set in the environment, it is used as the config
path for recursive calls on nested archives (set automatically by `archive-assistant`).

## `archive-assistant`

Walk a directory tree and preprocess everything. Calls `archive-repack` for
archives; applies processor rules directly to top-level non-archive files.

```sh
archive-assistant /path/to/documents --config zip-rewrite.toml

# With state DB for fast re-runs (skips already-processed files with no I/O)
archive-assistant /path/to/documents --config zip-rewrite.toml \
    --state-db /path/to/state.db

# Dry run
archive-assistant --dry-run /path/to/documents --config zip-rewrite.toml

# Only convert archives, don't process top-level files
archive-assistant --archives-only /path/to/documents --config zip-rewrite.toml

# Only process top-level files, skip archives
archive-assistant --files-only /path/to/documents --config zip-rewrite.toml

# Use a local temp directory (recommended when source is a network mount)
archive-assistant /mnt/nas/documents --config zip-rewrite.toml \
    --temp-dir /tmp/archive-work --state-db /tmp/state.db
```

### Options

```
archive-assistant [OPTIONS] <PATH>

Arguments:
  <PATH>    Directory to process

Options:
  --config <PATH>       Config file (applied to top-level files and forwarded to archive-repack)
  --state-db <PATH>     SQLite DB for tracking processed files
  --temp-dir <PATH>     Local temp directory [default: system temp]
  --dry-run             Print what would be done without modifying files
  --files-only          Only process top-level files, skip archives
  --archives-only       Only convert archives, skip top-level file processing
  --no-archive-files    Don't apply processor rules to files inside archives
  --jobs <N>            Parallel workers [default: CPUs / 2]
  --verbose             Log each file processed
```

`archive-assistant` expects `archive-repack` to be in the same directory as
itself (the case when both are installed from this workspace). Falls back to PATH.

### Idempotency

- **Non-ZIP archives**: always processed — their existence means they haven't
  been through `archive-repack` yet (which would have produced a ZIP).
- **ZIP archives**: skipped if they contain `archive-assistant.txt`. Processed otherwise.
- **Top-level files**: skipped if `(path, mtime)` is in the state DB. PDFs are also
  checked for an existing text layer (`pdftotext`) or ocrmypdf stamp before invoking
  the processor.

## Running over SMB / network mounts

When your documents are on a NAS mounted via SMB, run the tools on a local machine
with `--temp-dir` pointing at local storage. All heavy I/O (extraction, OCR, repacking)
stays local; only the final result is written back to the mount.

```sh
archive-assistant /mnt/nas/documents \
    --config config/zip-rewrite.toml \
    --temp-dir /tmp/archive-work \
    --state-db /tmp/archive-state.db
```

## Build

```sh
cargo build --workspace --release
```

Binaries are at `target/release/archive-repack` and `target/release/archive-assistant`.

See [PLAN.md](PLAN.md) for full design documentation.
