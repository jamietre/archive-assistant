# archive-assistant

A preprocessing tool for document archives. Designed to complement
[find-anything](../find-anything) by ensuring PDFs are text-searchable and archives
are in a format find-anything can browse efficiently.

## What it does

- **OCRs image-only PDFs** using `ocrmypdf`, embedding a searchable text layer
- **Converts non-ZIP archives** (7z, tar, tar.gz, etc.) to ZIP format
- **Processes PDFs inside archives** — extracts, OCRs, repacks
- **Idempotent** — tracks processed files in a local SQLite database; safe to re-run

## Requirements

- `ocrmypdf` (pip install ocrmypdf)
- `pdftotext` (poppler-utils / apt install poppler-utils)
- Tesseract language packs as needed (e.g. `apt install tesseract-ocr-eng`)

## Usage

```sh
archive-assistant /path/to/documents

# Dry run — see what would be processed
archive-assistant --dry-run /path/to/documents

# OCR only, skip archive conversion
archive-assistant --ocr-only /path/to/documents
```

See [PLAN.md](PLAN.md) for full design documentation.
