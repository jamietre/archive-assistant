# archive-assistant

A preprocessing tool for document archives. Designed to complement
[find-anything](../find-anything) by ensuring PDFs are text-searchable and archives
are in a format find-anything can browse efficiently.

Two tools in one workspace:

- **`zip-rewriter`** — processes members of a single ZIP file through configured tools
- **`archive-assistant`** — walks a directory tree, converts non-ZIP archives to ZIP,
  and runs processor rules against files it encounters

## What it does

- **OCRs image-only PDFs** — embeds a text layer so content is searchable
- **Converts non-ZIP archives** (7z, tar, tar.gz, rar, etc.) to ZIP
- **Processes files inside archives** — extracts, applies rules, repacks
- **Idempotent** — archives carry an embedded manifest; optional SQLite state DB for top-level files

## Requirements

```sh
# OCR support (pipx manages the venv automatically)
pipx install ocrmypdf
apt install tesseract-ocr-eng   # or other language packs as needed

# PDF text detection
apt install poppler-utils       # provides pdftotext

# RAR extraction (optional)
apt install unrar
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

## `zip-rewriter`

Process members of a single ZIP file. Rules can come from a config file, inline
flags, or both — inline flags define a rule that is prepended to any config-file rules.

```sh
# Using a config file
zip-rewriter archive.zip --config zip-rewrite.toml

# Inline rule — no config file needed
zip-rewriter archive.zip \
  --match '*.pdf' \
  --command ocrmypdf \
  --arg '--skip-text' --arg '--quiet' --arg '{input}' --arg '{input}'

# Inline shell expression
zip-rewriter archive.zip \
  --match '*.txt' \
  --shell 'cat {input} | tr a-z A-Z' \
  --io stdin-stdout

# Combine: inline rule runs first, then config-file rules
zip-rewriter archive.zip \
  --config zip-rewrite.toml \
  --match '*.png' --command convert --arg '{input}' --arg '{output}' --io file-to-file

# Dry run
zip-rewriter --dry-run archive.zip --config zip-rewrite.toml

# Reprocess even if already processed
zip-rewriter --force archive.zip --config zip-rewrite.toml

# Write result to a new file instead of in-place
zip-rewriter archive.zip --config zip-rewrite.toml --output archive-processed.zip
```

### Options

```
zip-rewriter [OPTIONS] <ZIP_FILE>

Arguments:
  <ZIP_FILE>    ZIP file to process

Config source (at least one required):
  --config <PATH>       Config file defining processor rules

Inline rule (alternative or supplement to --config):
  --match <GLOB>        Filename pattern [default: * when --command/--shell given]
  --command <CMD>       Command to run on matching members
  --arg <ARG>           Argument for the command (repeatable); use {input}, {output}
  --io <MODE>           I/O mode: in-place, file-to-file, file-to-stdout, stdin-stdout
                        [default: in-place]
  --shell <EXPR>        Shell expression via sh -c (alternative to --command)

General:
  --output <PATH>       Write result here instead of modifying the ZIP in-place
  --dry-run             Print what would be done without modifying the file
  --force               Reprocess even if archive-assistant.txt manifest is present
  --verbose             Log each member being processed
```

After processing, `archive-assistant.txt` is written into the ZIP as a manifest.
On subsequent runs the ZIP is skipped unless `--force` is passed.

## `archive-assistant`

Walk a directory tree and preprocess everything:

```sh
archive-assistant /path/to/documents --config zip-rewrite.toml

# With state DB for fast re-runs (skips already-processed files with no I/O)
archive-assistant /path/to/documents --config zip-rewrite.toml \
    --state-db /path/to/state.db

# Dry run
archive-assistant --dry-run /path/to/documents --config zip-rewrite.toml

# Only convert archives, don't process top-level files
archive-assistant --convert-only /path/to/documents --config zip-rewrite.toml

# Only process top-level files, skip archive conversion
archive-assistant --ocr-only /path/to/documents --config zip-rewrite.toml

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
  --config <PATH>       Config file [default: zip-rewrite.toml]
  --state-db <PATH>     SQLite DB for tracking processed files (recommended for large collections)
  --temp-dir <PATH>     Local temp directory [default: system temp]
  --dry-run             Print what would be done without modifying files
  --ocr-only            Only process top-level files, skip archive conversion
  --convert-only        Only convert archives, skip top-level file processing
  --no-archive-files    Don't process files inside archives
  --jobs <N>            Parallel workers [default: CPUs / 2]
  --verbose             Log each file being processed
```

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

Binaries are at `target/release/zip-rewriter` and `target/release/archive-assistant`.

See [PLAN.md](PLAN.md) for full design documentation.
