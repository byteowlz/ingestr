# ingestr

A background service that watches directories for documents, converts them to Markdown, and indexes them for full-text search. Includes an MCP server for integration with AI assistants.

## Features

- **Document Conversion**: Automatically converts documents (PDF, DOCX, XLSX, PPTX, HTML, etc.) to Markdown. PDFs use [LiteParse](https://github.com/run-llama/liteparse) (fast native extraction + page-aware OCR merge); other formats use [markitdown](https://crates.io/crates/markitdown).
- **Full-Text Search**: Indexes converted documents with [Tantivy](https://github.com/quickwit-oss/tantivy) for fast search
- **Background Service**: Runs as a daemon watching for file changes
- **MCP Server**: Exposes search functionality to AI assistants via the Model Context Protocol
- **XDG Compliant**: Respects `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_STATE_HOME`

## Installation

### From Source

```bash
# Clone the repository
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

1. **Initialize configuration:**

```bash
ingestr init
```

This creates a config file at `~/.config/ingestr/config.toml`.

2. **Start the service:**

```bash
# Run in foreground
ingestr service run

# Or run as background daemon
ingestr service start
```

3. **Search your documents:**

```bash
ingestr search "quarterly report"
```

## CLI Usage

```
ingestr <COMMAND>

Commands:
  service      Manage the background conversion and indexing service
  search       Query the search index
  convert      Convert documents to Markdown (file, directory, or URL)
  init         Create config directories and default files
  config       Inspect and manage configuration
  cache        Inspect or clear the conversion cache
  completions  Generate shell completions
```

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
--index-dir <PATH>    Directory for search index (default: $XDG_DATA_HOME/ingestr-cli/index)
--disable-index       Convert files without indexing
--once                Process existing files and exit
```

### Search

```bash
# Basic search
ingestr search "search terms"

# Limit results
ingestr search "budget" --limit 5

# Output as JSON
ingestr search "report" --json
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

[index]
enabled = true
# index_dir = "$XDG_DATA_HOME/ingestr/index"

[paths]
# data_dir = "$XDG_DATA_HOME/ingestr"
# state_dir = "$XDG_STATE_HOME/ingestr"
```

### Environment Variables

Override any config value using environment variables:

```bash
INGESTR_CLI__WATCHER__WATCH_DIR=~/MyDocs ingestr service run
INGESTR_CLI__INDEX__ENABLED=false ingestr service run
```

## MCP Server

The `ingestr-mcp` binary provides an MCP server for AI assistant integration.

### Setup

Add to your MCP client configuration (e.g., Claude):

```json
{
  "ingestr": {
    "command": "ingestr-mcp",
    "args": []
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
| `search` | Search the document index for relevant content |
| `open_source` | Open a source document (requires confirmation) |

### MCP Server Options

```bash
--config <PATH>      Override config file path
--index-dir <PATH>   Override index directory
--log-level <LEVEL>  Set log level (error, warn, info, debug, trace)
--show-config        Print MCP client configuration JSON and exit
```

The MCP server automatically starts the ingestr service if not already running.

## Directory Structure

| Path | Description |
|------|-------------|
| `$XDG_CONFIG_HOME/ingestr/config.toml` | Configuration file |
| `$XDG_DATA_HOME/ingestr/index/` | Tantivy search index |
| `$XDG_STATE_HOME/ingestr/service.pid` | Background service PID |
| `$XDG_STATE_HOME/ingestr/service.log` | Background service logs |

## Project Structure

```
ingestr/
  ingestr-cli/     # CLI application and background service
  ingestr-core/    # Shared search index library
  ingestr-mcp/     # MCP server for AI assistants
```

## Development

```bash
# Check all crates
just check

# Format code
just fmt

# Run tests
just test

# Run the service during development
just serve

# Run MCP server during development
just mcp
```

## License

See LICENSE file for details.
