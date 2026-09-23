# ingestr

A glossary for the domain language used by this project. Add terms only after
their meaning has been resolved. `CONTEXT.md` is a glossary, not a spec,
implementation guide, or scratchpad — include only project-specific domain
concepts, not general programming terms.

## Language

**Document**:
A source file ingestr ingests (PDF, DOCX, XLSX, PPTX, HTML, image, text).
Converted by a pipeline into Markdown for indexing. The unit of ingestion.
_Avoid_: file (ambiguous), input (ambiguous)

**Converted Document**:
The Markdown output of processing one source Document, along with metadata
(title, source/output paths, conversion timestamp). The persisted Markdown file
plus its frontmatter.
_Avoid_: output (ambiguous), result

**Conversion**:
The act of turning a Document into a Converted Document via a processor
pipeline. May involve text extraction, OCR, or VLM image understanding.
_Avoid_: processing, extract

**Ingest**:
The end-to-end flow of noticing a Document (via the watcher or an explicit
convert command), converting it, writing the Markdown, and indexing it for
search.
_Avoid_: import (ambiguous with other tools)

**Watch Directory**:
A directory the background service watches for new or changed Documents. A
Document landing here (or changing) triggers a conversion-and-index pass.
_Avoid_: input dir, source dir

**Output / Markdown Directory**:
Where Converted Documents are written as `.md` files.
_Avoid_: out dir

**Index**:
The tantivy-backed full-text index over converted Documents, queried by the
search command and the MCP server. One index may be writable (service) or
read-only (search).
_Avoid_: database, index dir (that is the on-disk location)

**Processor Pipeline**:
An ordered list of processors (`markitdown`, `ocr_fallback`, `vlm_images`, …)
tried in sequence for a Document. The first producing non-empty Markdown wins.
_Avoid_: pipeline

**Processor**:
A step in the pipeline that produces Markdown from a Document (MarkItDown
conversion, OCR, or VLM image description).
_Avoid_: converter (used loosely)

**Backend (OCR)**:
A concrete OCR engine selected via `--ocr-backend` / `[processors.ocr].backend`
(`tesseract`, `ocrs`, `surya`, `easyocr`). Independent of the pipeline order.
_Avoid_: engine (ambiguous with index engine)

**Profile**:
A named configuration overlay within the config file (`[profiles.<name>]`),
selectable at runtime. Different profiles can watch different directories or use
different processing settings.
_Avoid_: mode, environment

**Service / Daemon**:
The background process (`service run`, `service start`) that watches a Watch
Directory and ingests Documents continuously. Distinguished from the
one-shot `convert` command.
_Avoid_: daemon-only, watch loop

**Service Settings**:
The resolved runtime configuration for one service run, built from config +
CLI overrides (watch/output/index dirs, watcher, index, LLM, processors). Same
type used by both one-shot convert and the long-running service.
_Avoid_: config (the persisted AppConfig), runtime config

**Search**:
The full-text query over the Index, exposed by the `search` command and the
MCP server. Returns scored hits with source/output paths.
_Avoid_: query (the specific search string)

**MCP Server**:
The `ingestr-mcp` binary exposing search (and related) capabilities to AI
assistants over the Model Context Protocol.
_Avoid_: mcp (acronym used in names), api

**VLM (Vision Language Model)**:
An OpenAI-compatible vision-capable model endpoint used to describe images,
diagram pages, or presentation slides when text extraction yields too little.
Configured under `[processors.vlm]` / `[llm]`.
_Avoid_: vision, image processor

**LLM Configuration**:
The shared provider connection (`[llm]`) supplying endpoint/model/API key for
VLM processing. VLM falls back to it when its own fields are empty.
_Avoid_: provider

**Frontmatter**:
The `---`-delimited metadata block prepended to a Converted Document when
`--meta` is set. Serialized as JSON (a valid YAML subset) for standard frontmatter
compatibility; no YAML dependency.
_Avoid_: metadata block

**Config File**:
The TOML configuration at `~/.config/ingestr/config.toml`. Written on first run;
overridable via CLI flags and environment.
_Avoid_: settings file (ambiguous)

**Cache**:
The per-user on-disk cache of downloaded OCR models (XDG cache dir), keyed by
URL, used to avoid re-downloading large model weights.
_Avoid_: model cache (that is the specific sub-location)

**Debounce**:
The delay (ms) the watcher waits after a filesystem event before ingesting, to
coalesce bursts of writes.
_Avoid_: delay (ambiguous)

**Post-processing / Cleanup**:
A set of deterministic cleanup steps on raw converted Markdown (dedupe repeated
lines, strip standalone page numbers, fix broken column wraps, collapse blank
lines, extract a table of contents). Skippable with `--raw`.
_Avoid_: cleaning, normalize

**TOC**:
The extracted table of contents from converted Markdown, represented as
[`TocEntry`] sections. Used by the `--toc` view.
_Avoid_: outline

**Convert Result**:
The structured output of a one-shot conversion (`ConvertResult`), including
source/output paths, frontmatter, and stats; used for `--json` output.
_Avoid_: response

**Page-aware Routing**:
The process of classifying each PDF page (text/vector vs scanned) and routing
text pages to native Markdown extraction while sending only scanned pages to
OCR. Keeps scanned pages from being silently dropped in mixed PDFs and avoids
OCR-ing text pages. Handled internally by LiteParse (`is_complex` → `needs_ocr`);
configured under `[processors.ocr]` (`page_dpi`, `ocr_server_url`).
_Avoid_: routing (ambiguous with file-type routing)

**LiteParse**:
Apache-2.0 Rust parser used as ingestr's Tier-0 document engine for PDF and
office formats. Extracts native text/vector pages via PDFium (no model),
classifies pages (`is_complex`), OCRs/merges only scanned or text-sparse pages,
and converts PPTX/DOCX/XLSX via LibreOffice while extracting embedded images.
Supports an `ocr_server_url` HTTP OCR seam and `oar-ocr` GPU features.
_Avoid_: parser (generic), llama/liteparse name confusion

**Office formats**:
PPTX/DOCX/XLSX/PPT/ODP/KEY/DOC/ODT/XLS/ODS converted via LiteParse (LibreOffice
→ text extraction + embedded-image extraction) rather than markitdown.
_Avoid_: office docs (ambiguous)