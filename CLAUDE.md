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
- `zip-rewriter/` — standalone ZIP member processor CLI
- `archive-assistant/` — directory walker and archive converter CLI
