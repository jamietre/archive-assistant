# archive-assistant

## Before committing

Always run clippy and ensure it passes before creating a commit:

```sh
cargo clippy --workspace --all-targets -- -D warnings
```

## Build

```sh
mise run build
# or
cargo build --workspace
```

## Workspace structure

- `processor/` — shared library: config parsing, processor dispatch
- `archive-repack/` — standalone archive-to-ZIP repacker CLI
- `archive-assistant/` — directory walker and archive converter CLI
