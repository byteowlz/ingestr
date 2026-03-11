# Issues

## Open

### [ingestr-50xe] Section-level retrieval: --section flag (P1, feature)
Return only a specific section by heading number or name. Works with --toc output. E.g. --section '2.1' returns only that subsection. Dramatically reduces token usage for large documents.

### [ingestr-09a8] Structure extraction: --toc flag (P1, feature)
Extract document structure (headings, page count, tables) without full content. Show heading hierarchy with estimated token counts per section. Enables agents to scan structure first, then request specific sections.

### [ingestr-qke0] Token budget control: --max-tokens / --max-chars (P1, feature)
Truncate output to a token/character budget. Intelligent truncation at section boundaries, not mid-sentence. Append truncation notice with remaining token count so agent can request next chunk. Support --offset for pagination.

### [ingestr-jz66] Post-processing cleaning pipeline (P1, feature)
After markitdown converts, run a cleaning pass: collapse excessive whitespace, strip repeated headers/footers (detected by repetition across pages), remove page numbers, fix broken line wraps from PDF column layouts. On by default with --raw to skip.

### [ingestr-ejk1] Strip frontmatter by default, add --meta flag (P1, feature)
Default output should be pure markdown with no YAML frontmatter. Add --meta flag to include metadata when explicitly requested. When --json is used, metadata goes in JSON fields, not embedded in markdown string.

### [ingestr-5a7e] Make convert the default subcommand (P2, feature)
If the first argument is a file path or URL, assume convert. No need to type 'ingestr convert file.pdf' when 'ingestr file.pdf' is unambiguous. Every other subcommand is a known keyword.

### [ingestr-vfz7] Conversion cache (P2, feature)
Cache converted output keyed by file hash + conversion options. Second conversion is instant. Cache lives in XDG_CACHE_HOME/ingestr/. --no-cache to bypass. Add cache clear and cache stats subcommands.

### [ingestr-2vda] Native URL support (P2, feature)
If input looks like a URL, fetch and convert in one step. Handles HTML pages and PDF links. Follows redirects. Combines with all other flags (--max-tokens, --toc, etc).

### [ingestr-980z] Page-level access: --pages flag (P2, feature)
For PDFs: convert specific pages only. --pages 1-3,7 converts only those pages. Agents often know which page they need from a prior TOC scan.

### [ingestr-ffc5] Table-aware conversion (P2, feature)
Detect tables in source and ensure they survive as proper markdown tables. For XLSX, use sheet name as heading. For --json output, include a structured tables array with headers and rows.

### [ingestr-a6nx] Clipboard support: --clipboard flag (P3, feature)
Read from system clipboard. Handles text, HTML, and images (via VLM). Detects content type automatically.

