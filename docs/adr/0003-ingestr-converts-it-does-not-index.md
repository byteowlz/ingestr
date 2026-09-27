# ingestr converts documents; it does not index or search them

ingestr shipped a tantivy full-text index and a `search` command/MCP tool
alongside conversion. In oqto, the intended host, search already exists (SQLite
FTS over the workspace ledger) and every consumer of converted Markdown owns its
own retrieval, so a second full-text engine inside the converter duplicated that
role and brought tantivy's single-writer constraint into a per-user, concurrent
setting. We removed the index and search entirely: ingestr's contract is bytes
in, Markdown plus extracted assets out. Search over the output belongs to the
consumer.

Status: accepted (supersedes the search/index parts of the original design)

## Considered options

- **Keep the index as an optional feature**: keeps `ingestr search` for the
  standalone case, but the code path, the writer-locking questions, and the
  dependency stay for everyone, and the MCP server keeps a search tool that
  competes with the host's. Rejected.
- **Remove index and search (chosen)**: `ingestr-core` becomes the conversion
  library only; the watch service converts on change and writes Markdown; the
  MCP server exposes conversion and reading of converted output; document
  search is done by whatever the host already uses (oqto's FTS, ripgrep, SQLite).

## Consequences

- `tantivy` is dropped from every crate; `ingestr search`, `--index-dir`,
  `[index]` config, and the MCP `search` tool are removed. This is a breaking
  CLI/config change.
- The watch service no longer has a shared-writer concurrency problem; the
  only shared state left is the read-only model cache and the conversion cache.
- The embedding path for oqto (ADR-0010 in oqto: embedded library, no per-user
  service) becomes straightforward: the runner calls `ingestr-core` to convert
  on upload and indexes the Markdown itself.
- Standalone users who relied on `ingestr search` should point their own
  search tool at the Markdown output directory.