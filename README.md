# ingestr

Converts documents to Markdown: PDFs, Office files, HTML, images and more, one
file or a whole directory tree. Ships a CLI, an optional watch service that
converts files as they appear, and an MCP server so AI assistants can convert
documents on demand. ingestr does not index or search; search over its Markdown
output belongs to whatever you already use for that (see ADR-0003).

## Features

- **Document conversion**: PDF, DOCX, PPTX, XLSX, HTML, CSV, images and more to Markdown. PDFs and presentations/documents (PPTX/DOCX) use [LiteParse](https://github.com/run-llama/liteparse) (fast native extraction, page-aware OCR merge, embedded-image extraction); spreadsheets and other formats use [markitdown](https://crates.io/crates/markitdown).
- **OCR for scanned pages and images**: PP-OCR (PaddleOCR family) via a bundled ONNX runtime by default, CPU-only; models download on first use. Tesseract and other backends are available. OCR only runs on pages that need it.
- **Batch conversion that scales**: resumable by content hash, a document-format allowlist, per-file failure isolation, parallel workers, `--dry-run` and machine-readable `--json` output.
- **Watch service**: converts documents as they land in a directory.
- **MCP server**: exposes conversion to AI assistants over the Model Context Protocol (official `rmcp` SDK).
- **XDG compliant**: respects `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_STATE_HOME`.

## Installation

### From Source

```bash
git clone https://github.com/byteowlz/ingestr.git
cd ingestr

# Install both binaries
cargo install --path ingestr-cli
cargo install --path ingestr-mcp
```

Or use just:

```bash
just install-all
```

## Quick Start

Convert a single file to stdout:

```bash
ingestr convert report.pdf
```

Convert every document in the current directory (add `-r` for subdirectories):

```bash
ingestr convert . -r --output out/
```

Optionally create a config file and run the watch service:

```bash
ingestr init
ingestr service run
```

## CLI Usage

```
ingestr <COMMAND>

Commands:
  convert      Convert documents to Markdown (file, directory, or URL)
  service      Manage the background conversion service
  init         Create config directories and default files
  config       Inspect and manage configuration
  cache        Inspect or clear the conversion cache
  completions  Generate shell completions
  doctor       Report which external tools are installed
```

### External dependencies

PDF conversion, spreadsheets and OCR need **no external tools**: LiteParse
bundles PDFium, and the default `paddle` OCR backend bundles an ONNX runtime
(PP-OCR models are fetched from Hugging Face into the HF cache on first use). Converting PPTX/DOCX
requires **LibreOffice** (`soffice`); some optional OCR backends use Poppler
(`pdftoppm`). Run `ingestr doctor` to see what is installed and what is missing.

### Converting documents

Convert a single file (to stdout):

```bash
ingestr convert report.pdf
```

Convert **every document in the current directory** to `.md`:

```bash
ingestr convert .
```

Include subdirectories, write to a folder, or preview the plan:

```bash
ingestr convert . --recursive --output out/
ingestr convert . --recursive --dry-run
ingestr convert . --recursive --json
```

Batch runs are resumable: converted files are cached by content hash, so
re-running after an interruption only converts what is new or changed (and
re-creates missing outputs from the cache). By default only known document
formats are attempted; use `--extensions pdf,docx` to narrow, `--all-files` to
try everything, and `--engine liteparse|markitdown` to force an engine. Add
`--ocr` for scanned documents; it only OCRs pages that actually need it.
Add `--layout` (with `-o`) to crop charts and diagrams that are drawn as vector
graphics or sit inside scans into `fig_pN_K.png` files linked from the
Markdown; it runs a layout model (~125 MB, ~1 s per selected page on CPU) only
on pages that can hold such figures.

PDFs that only restrict printing or copying convert as they are. For PDFs
that need a password to open, ingestr tries candidates in order:
`--password` (repeatable; visible in the process list), `--password-file`
(one per line, `-` for stdin), `INGESTR_PDF_PASSWORDS` (one per line), then
the lines printed by `[processors.pdf] password_command`, which only runs when
a PDF needs it. With kyz:

```bash
kyz pipe ingestr/pdf-passwords ingestr convert locked.pdf --password-file -
# or in config.toml: password_command = "kyz get --service ingestr pdf-passwords"
```

### Service Commands

```bash
# Start in foreground (useful for debugging)
ingestr service run

# Start as background daemon
ingestr service start

# Stop the daemon
ingestr service stop

# Check status
ingestr service status

# Restart
ingestr service restart

# Convert existing files once and exit
ingestr service run --once
```

### Service Options

```bash
--watch-dir <PATH>    Directory to watch for documents (default: ~/Documents)
--output-dir <PATH>   Directory for converted Markdown files (default: ~/markdown)
--once                Process existing files and exit
```

### Configuration

```bash
# Show effective configuration
ingestr config show

# Show config file path
ingestr config path

# Reset to defaults
ingestr config reset
```

### Shell Completions

```bash
# Bash
ingestr completions bash > ~/.local/share/bash-completion/completions/ingestr

# Zsh
ingestr completions zsh > ~/.zfunc/_ingestr

# Fish
ingestr completions fish > ~/.config/fish/completions/ingestr.fish
```

## Configuration

Configuration is loaded from (in order of increasing priority):

1. Default values
2. Global config: `$XDG_CONFIG_HOME/ingestr/config.toml` (or `~/.config/ingestr/config.toml`)
3. Local config: `./config.toml`
4. Environment variables: `INGESTR_CLI__<SECTION>__<KEY>`
5. CLI-specified config: `--config <path>`
6. Command-line arguments

### Example config.toml

```toml
profile = "default"

[logging]
level = "info"
# file = "~/Library/Logs/ingestr.log"

[runtime]
# parallelism = 8
timeout = 60
fail_fast = true

[watcher]
watch_dir = "~/Documents"
debounce_ms = 750
skip_hidden = true

[output]
markdown_dir = "~/markdown"

[paths]
# data_dir = "$XDG_DATA_HOME/ingestr"
# state_dir = "$XDG_STATE_HOME/ingestr"

[processors.ocr]
enabled = false
backend = "paddle"        # paddle (default) | tesseract | ocrs | surya | easyocr
paddle_model = "small"    # tiny | small | medium
languages = ["eng"]

# Semantic tier-router seam (System One / kev). Optional SPIKE: routes each
# document/page to one of a small closed set of ingestion tiers via a local
# System-One decision model served over HTTP (`POST /v1/systemone`).
# See docs/research/2026-ocr-vlm-semantic-router.md.
[routing]
mode = "heuristic"  # heuristic (default) | shadow | route
# router_url = "http://localhost:8009"  # System-One base URL; required for shadow/route
# model = "kev-latest"
# api_key = "sk-..."
```

See `ingestr-cli/examples/config.toml` for every option with comments.

### Semantic tier routing (SPIKE)

Ingestr can optionally route each document to one of a fixed set of ingestion
tiers (`native`/`cpu_ocr`/`gpu`/`vlm`/`skip`) using a local, trainable
System-One decision model (e.g. `jaredpalmer/kev`) served over HTTP. This is a
**spike / proof-of-concept seam**, not a production feature.

Configure it under `[routing]`:

```toml
[routing]
mode = "route"          # heuristic | shadow | route
router_url = "http://localhost:8009"
model = "kev-latest"
# api_key = "sk-..."
```

| mode        | Behavior                                                              |
|-------------|-----------------------------------------------------------------------|
| `heuristic` | (default) No router call; existing deterministic behavior unchanged.  |
| `shadow`    | Calls the router and logs the chosen tier + probabilities; does **not** change routing. |
| `route`     | Calls the router; when it selects `vlm` and VLM is enabled, prefers the VLM path, otherwise logs and falls back to the deterministic pipeline. |

In `shadow`/`route` mode, if the router is unreachable or returns an error, the
pipeline falls back to the `heuristic` behavior and never panics. Router
decisions are cached in-process by a content hash (sha256 of the file bytes) so
repeated/converted documents reuse a decision. See
`docs/research/2026-ocr-vlm-semantic-router.md` for the research and tier model
recommendations.

### Environment Variables

Override any config value using environment variables:

```bash
INGESTR_CLI__WATCHER__WATCH_DIR=~/MyDocs ingestr service run
INGESTR_CLI__PROCESSORS__OCR__ENABLED=true ingestr service run
```

## MCP Server

The `ingestr-mcp` binary is an MCP server (official Rust SDK `rmcp`, protocol
revision 2026-07-28, stdio transport) that lets AI assistants convert documents
on demand. It runs the `ingestr` CLI as a subprocess, so install both binaries.

### Setup

Add to your MCP client configuration (e.g., Claude):

```json
{
  "mcpServers": {
    "ingestr": {
      "command": "ingestr-mcp",
      "args": []
    }
  }
}
```

Or generate the config:

```bash
ingestr-mcp --show-config
```

### Available Tools

| Tool | Description |
|------|-------------|
| `convert_document` | Convert a file or http(s) URL to Markdown and return it; options `ocr`, `raw`, `max_chars`, `pages`, `section` |
| `convert_to_file` | Convert and write Markdown (plus extracted images) to an output path |
| `supported_formats` | List the file extensions ingestr can convert |
| `doctor` | Report which optional external tools are installed |

### MCP Server Options

```bash
--ingestr-bin <PATH>   Path to the ingestr CLI (default: $INGESTR_BIN, a sibling binary, then PATH)
--timeout <SECONDS>    Maximum seconds per conversion (default: 600)
--log-level <LEVEL>    Set log level (error, warn, info, debug, trace); logs go to stderr
--show-config          Print MCP client configuration JSON and exit
```

## Directory Structure

| Path | Description |
|------|-------------|
| `$XDG_CONFIG_HOME/ingestr/config.toml` | Configuration file |
| `$XDG_CACHE_HOME/ingestr/` | Conversion cache (content-hash keyed) |
| `~/.cache/huggingface/hub/` | PP-OCR and layout models (`$HF_HOME`; shared read-only cache via `$INGESTR_SHARED_HF_HOME`; `HF_HUB_OFFLINE=1` forbids downloads) |
| `$XDG_STATE_HOME/ingestr/service.pid` | Background service PID |
| `$XDG_STATE_HOME/ingestr/service.log` | Background service logs |

## Project Structure

```
ingestr/
  ingestr-cli/     # CLI application and watch service
  ingestr-core/    # Shared conversion library (format routing today; pipeline next)
  ingestr-mcp/     # MCP server for AI assistants
  vendor/          # Patched third-party crates (see vendor/*/PATCH.md)
```

## Development

```bash
# Full gate: fmt, clippy, drift check, tests
just check-all

# Format code
just fmt

# Run the service during development
just serve

# Run MCP server during development
just mcp
```

## License

See LICENSE file for details.