set positional-arguments

default: help

# List available tasks
help:
    just --list

# Format all crates
fmt:
    cargo fmt

# Workspace check
check:
    cargo check

# Check individual crates
check-cli:
    cargo check -p ingestr-cli

check-mcp:
    cargo check -p ingestr-mcp

check-core:
    cargo check -p ingestr-core

# Run the markdown watcher/indexer; pass additional flags after `--`
serve *args:
    cargo run -p ingestr-cli -- serve {{args}}

# Search the index via CLI
search query limit='10':
    cargo run -p ingestr-cli -- search "{{query}}" --limit {{limit}}

# Start the MCP server; pass additional flags after `--`
mcp *args:
    cargo run -p ingestr-mcp -- {{args}}

# Run tests (workspace)
test:
    cargo test

# Install binaries locally
install-cli:
    cargo install --path ingestr-cli

install-mcp:
    cargo install --path ingestr-mcp

install-all:
    cargo install --path ingestr-cli
    cargo install --path ingestr-mcp
