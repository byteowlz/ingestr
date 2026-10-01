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
convert command), converting it, and writing the Markdown plus extracted
assets. Search over the output is the consumer's job (ADR-0003).
_Avoid_: import (ambiguous with other tools)

**Watch Directory**:
A directory the background service watches for new or changed Documents. A
Document landing here (or changing) triggers a conversion-and-index pass.
_Avoid_: input dir, source dir

**Output / Markdown Directory**:
Where Converted Documents are written as `.md` files.
_Avoid_: out dir

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
CLI overrides (watch/output dirs, watcher, LLM, processors). Same
type used by both one-shot convert and the long-running service.
_Avoid_: config (the persisted AppConfig), runtime config

**MCP Server**:
The `ingestr-mcp` binary exposing conversion (`convert_document`,
`convert_to_file`, `supported_formats`, `doctor`) to AI assistants over the
Model Context Protocol, built on the official `rmcp` SDK and served over stdio.
It runs the `ingestr` CLI as a subprocess. It does not search.
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
PPTX/DOCX/PPT/ODP/KEY/DOC/ODT/ODS converted via LiteParse (LibreOffice → text
extraction + embedded-image extraction). Requires LibreOffice (`soffice`).
Spreadsheets (XLSX/XLS) are converted by markitdown instead (better tables, no
LibreOffice). PDFs need no external tool.
_Avoid_: office docs (ambiguous)

**Paddle OCR**:
The default OCR Backend (`paddle`): PP-OCR models from the PaddleOCR family run
on a bundled ONNX runtime (via LiteParse's `oar-ocr`), CPU-only. Used for
scanned PDF pages and standalone images; models come from Hugging Face, sha256-pinned (ADR-0004).
_Avoid_: PaddleOCR-VL (that is the separate GPU-tier vision model)

**Staged Conversion**:
A PDF conversion delivered in two stages (ADR-0005). Stage 1 is the native
text, available in milliseconds and handed to a library host's preview
callback. Stage 2 runs only when OCR or Layout Analysis has more to add, and
its result replaces stage 1 wholesale.
_Avoid_: incremental conversion, partial result

**Layout Analysis**:
Running the layout model (PP-DocLayout_plus-L) on the PDF pages that can hold
figures the native parse misses (scanned pages, vector figure clusters with
little text), cropping charts / diagrams / pictures to `fig_pN_K.png` and
linking them above their captions (`--layout`, ADR-0005).
_Avoid_: layout parsing (LiteParse's reading-order pass is separate)

**Auto OCR**:
The default: OCR runs only on pages without usable text (scans, blank or
garbled pages), and only those pages are OCR'd; the others keep their native
text. Thorough OCR (`--ocr`) also reads text inside pictures and OCRs the
whole document.
_Avoid_: forced OCR, OCR fallback

**Locked PDF**:
A PDF that needs a password to open (a user password). ingestr opens it with
the first working candidate password and refuses it otherwise. PDFs that are
encrypted only to restrict printing or copying are not locked and convert as
they are.
_Avoid_: encrypted PDF (most encrypted PDFs are not locked)

**Resume Cache**:
The content-hash conversion Cache as used by batch conversion: a re-run skips
files whose content and conversion flags were already converted (`skipped` in
the stats) and re-creates missing outputs from the cache. Failures are not
cached.
_Avoid_: incremental mode

**Engine**:
The converter selected for a Document: `auto` (LiteParse for PDF/office,
markitdown otherwise), or forced via `--engine liteparse|markitdown`.
_Avoid_: backend (that is the OCR engine choice)

**Doctor**:
The `ingestr doctor` command reports which external tools are installed
(LibreOffice, Poppler, Tesseract, ImageMagick, Python) and what each enables.
_Avoid_: deps, requirements

**Semantic Tier Router**:
An optional SPIKE seam (`[routing]`) that routes each Document to one of a
small closed set of ingestion **Tiers** (`native`, `cpu_ocr`, `gpu`, `vlm`,
`skip`) using a local System-One decision model (e.g.
`jaredpalmer/kev`) served over HTTP (`POST /v1/systemone`). It runs in
`heuristic` (default, no router call), `shadow` (log only), or `route` (act on
the tier selection) mode. Distinct from **Page-aware Routing** (LiteParse's
within-PDF `is_complex` classification). _Avoid_: router (ambiguous with file
type routing), semantic router

**Tier**:
One of the closed set of ingestion tiers a Document can be routed to by the
**Semantic Tier Router**: `native` (clean text/vector), `cpu_ocr` (scanned or
text-sparse), `gpu` (dense/noisy tables, charts, handwriting), `vlm` (image,
diagram, screenshot), `skip` (non-document or low value). _Avoid_: level, rank