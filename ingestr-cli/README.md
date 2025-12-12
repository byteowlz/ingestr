# Ingestr Markdown Service

A background Rust service that watches a directory, converts documents to Markdown with [markitdown](https://crates.io/crates/markitdown), adds YAML frontmatter linking to the source file, and builds a fast Tantivy index for searching.

## Quick Start

- Install Rust stable and fetch dependencies:
  ```bash
  cargo fetch
  ```
- Create a config with defaults:
  ```bash
  cargo run -- init
  ```
- Start the watcher (Ctrl+C to stop):
  ```bash
  cargo run -- serve
  ```
- Query the index:
  ```bash
  cargo run -- search "quarterly report" --limit 5
  ```

## Configuration

- Default global config: `$XDG_CONFIG_HOME/ingestr-cli/config.toml` (or `~/.config/ingestr-cli/config.toml`).
- Priority: CLI flags → config file named on the CLI (`--config`) → environment (`INGESTR_CLI__*`) → local `./config.toml` → global config.
- Defaults:
  - Watch directory: `~/Documents`
  - Markdown output: `~/markdown`
  - Index directory: `$XDG_DATA_HOME/ingestr-cli/index`
  - Hidden files are skipped; debounce: 750ms
- See `examples/config.toml` for a commented template with XDG-compliant paths.

## Service Behavior

- Converts supported documents to Markdown with YAML frontmatter:
  - `source_path`, `output_path`, `source_modified`, `converted_at`, `title`
- Writes Markdown to the configured output directory, mirroring the watched folder structure.
- Updates a Tantivy index for fast search; disable with `--disable-index`.
- Respects `--watch-dir`, `--output-dir`, and `--index-dir` overrides on `serve`.

## Development

- Format: `cargo fmt`
- Check: `cargo check -p ingestr-cli`
- Tests (if added later): `cargo test -p ingestr-cli`
