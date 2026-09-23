# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`ingestr doctor`**: reports which external tools are installed (LibreOffice, Poppler, Tesseract, ImageMagick, Python) and what each enables, with install instructions for any that are missing.
- **Clear LibreOffice dependency**: office formats (PPTX/DOCX/XLSX) require LibreOffice; the converter now fails with an actionable message (pointing at `ingestr doctor`) instead of a cryptic error. PDFs need no external tool (PDFium is bundled).
- **Office formats through LiteParse**: PPTX/DOCX/XLSX (and PPT/ODP/KEY/DOC/ODT/XLS/ODS) now convert via LiteParse instead of markitdown (whose PPTX path was broken). For a PPTX this extracts both per-slide text and embedded images; with `--output` the images are written as files alongside the Markdown, so a deck comes back as text + image components. PDFs still use LiteParse.
- **Top-notch `convert` CLI**: rich help with worked examples, `--fail-fast` (stop at the first conversion error), TTY-gated color/progress (honoring `NO_COLOR`/`--no-color`), machine-mode `--json` contract (stdout always parseable, `ok`/`found`/`plans` envelopes, per-file results and errors), clean display-only error output, and a non-zero exit code when any file fails. `ingestr convert .` now converts every document in the cwd one-shot; add `--recursive` for subdirs.
- **Bug fix**: a relative input like `ingestr convert .` no longer silently skips every file (the hidden-file filter treated the `.` current-dir component as hidden).
- **LiteParse as the Tier-0 PDF parser**: `ingestr` now uses [LiteParse](https://github.com/run-llama/liteparse) (Apache-2.0) for PDF conversion. It classifies each page, extracts text/vector pages natively (PDFium, ~2-5ms/page, no model), and OCRs/merges only scanned or text-sparse pages — so mixed PDFs no longer drop their scanned pages. This replaced the earlier hand-rolled `pdf-inspector` routing (removed) and beats both `pdf-inspector` and markitdown on ParseBench/olmOCR/opendataloader benchmarks. Adds `liteparse` + `tokio` deps; absorbed the old `jinja`-style route into the parser.
- **`[processors.ocr]` `ocr_server_url`**: optional local OCR HTTP server URL (LiteParse OCR API) that delegates PDF OCR to an external engine (e.g. a PaddleOCR-VL server) instead of LiteParse's built-in Tesseract.

### Changed

- **Conformance to byteowlz standards**: Added `CONTEXT.md`, `docs/adr/` (template + README), `clippy.toml`, `.ast-grep/rules/`, and `scripts/drift-check.sh`; rewrote `AGENTS.md` with source-of-truth, strict-lint, and JSON/TOML-only guidance; added workspace lints and the full `just check-all` gate.
- **Removed `--yaml` output**: The `--yaml` flag and `serde_yaml` dependency were removed to enforce the project's JSON/TOML-only rule (machine output is JSON). Use `--json` instead.
- **Frontmatter serialized as JSON**: Markdown frontmatter (via `--meta`) is now emitted as JSON rather than YAML. JSON is a valid YAML subset, so standard `---` frontmatter consumers still parse it.
- **VLM API key passed as a Bearer header**: The API key is now sent to the vision model endpoint via an `Authorization: Bearer` header instead of setting process-global `OPENAI_*` environment variables (which are `unsafe` in Rust 2024 and were not consumed as auth).

## [0.3.5] - 2026-08-27

### Fixed

- Prevented conversion crashes when `markitdown` panics on malformed/misdetected inputs (for example, `ParseError(NoFeedRoot)` in RSS parsing while converting non-RSS files). `ingestr` now catches converter panics, logs a warning, and continues with fallback handling instead of aborting the process.
- Updated `markitdown` from `0.1.10` to `0.1.11` for upstream converter fixes.

## [0.3.1] - 2026-03-25

### Added

- **Native Rust OCR backend (`ocrs`)**: Added `--ocr-backend ocrs` to run OCR using the Rust-native `ocrs` engine. Default `ocrs` models are auto-downloaded on first use and cached in the XDG cache directory (`$XDG_CACHE_HOME/ocrs` or `~/.cache/ocrs`).
- **OCRS PDF support**: `ocrs` backend now handles PDFs by rendering pages via `pdftoppm` and running OCR page-by-page, similar to the VLM PDF path.
- **Detailed OCR progress display**: Added verbose OCR progress on stderr, including model preparation/loading, page-by-page PDF OCR progress with ETA, and model download progress (bytes/percent) when models are fetched.

### Changed

- **OCR default backend**: `ocrs` is now the default OCR backend for CLI and config defaults. You can still override with `--ocr-backend tesseract|surya|easyocr`.

### Fixed

- **Encrypted PDF handling**: Fixed panic when converting encrypted/password-protected PDFs. The tool now detects encrypted PDFs and returns a clear error message suggesting to use `--vlm` or `--ocr` flags instead of crashing with `PdfError(Decryption(InvalidKeyLength))`.
- **Cleaner CLI output for problematic PDFs**: Suppressed noisy `lopdf` "corrupt deflate stream" warnings in normal output so users see actionable errors instead of repetitive parser warnings.

## [0.3.0] - 2026-03-12

### Added

- **Streaming VLM output**: `--vlm` now streams pages to a markdown file as each page completes, instead of buffering everything. Default output is `<stem>.md` in the current directory. Agents can read partial results immediately.
- **VLM progress display**: Progress with ETA shown on stderr during VLM processing (e.g., `[3/14] Page 3 done (12.1s avg, ~133s remaining)`).
- **Parallel VLM processing**: `-j N` / `--jobs N` runs N concurrent VLM requests. Pages are still written to file in order.
- **`--vlm-model` flag**: Specify a vision model on the command line without changing config (e.g., `--vlm-model qwen/qwen3-vl-8b`).

### Changed

- VLM config simplified: removed `backend`, `provider` fields. VLM now works exclusively via OpenAI-compatible APIs (Ollama, LM Studio, vLLM, etc.).
- VLM config falls back to main `[llm]` config for `base_url` and `model` when not explicitly set.
- Removed `X-Provider` header from VLM API calls.
- VLM HTTP client now has a 120s timeout (vision models are slow).
- Fixed `--force` alias conflict that caused panics on some subcommands.

## [0.2.0] - 2026-03-11

### Added

- **Clean output by default**: Conversion output is now pure markdown with no YAML frontmatter. Use `--meta` to include metadata when needed.
- **Post-processing cleaning pipeline**: Automatically strips page numbers, repeated headers/footers, fixes broken PDF line-wraps, and collapses excessive blank lines. Use `--raw` to skip cleaning.
- **Token budget control**: `--max-tokens N` and `--max-chars N` truncate output at section boundaries with a notice showing remaining content. Combine with `--offset N` for paginated reading.
- **Table of contents extraction**: `--toc` shows document structure with section numbers, estimated token counts, and markers for tables/code blocks. Supports `--json` output.
- **Section-level retrieval**: `-s / --section` extracts a specific section by number (e.g., `2.1`) or heading name (case-insensitive substring match).
- **Page-level access**: `--pages "1-3,7"` converts only specific pages from PDFs (requires page break markers in converted output).
- **Native URL support**: Pass a URL as input to fetch and convert in one step (e.g., `ingestr https://example.com/report.pdf`).
- **Clipboard support**: `--clipboard` reads text, HTML, or images from the system clipboard for conversion.
- **VLM for PDFs**: `--vlm` now works on PDFs -- each page is rendered to an image via `pdftoppm` and sent through the vision model page-by-page.
- **VLM for presentations**: `--vlm` now works on PPTX/PPT/ODP/KEY -- slides are converted to PDF via LibreOffice, then rendered to images and processed slide-by-slide through the vision model.
- **VLM uses any OpenAI-compatible API**: Works with Ollama, LM Studio, vLLM, or any server implementing `/v1/chat/completions` with vision support. Falls back to the main `[llm]` config, or override with `[processors.vlm] llm_url` and `model`.
- **Conversion cache**: Results are cached by file hash and conversion flags. Repeated conversions of the same file are instant. Manage with `ingestr cache stats` and `ingestr cache clear`. Use `--no-cache` to bypass.
- **Default subcommand**: `ingestr file.pdf` now works without typing `convert` -- the subcommand is inferred when the first argument is a file path or URL.
- **Cache management commands**: `ingestr cache stats` and `ingestr cache clear` for inspecting and managing the conversion cache.
- 21 new unit tests covering cleaning, TOC extraction, section retrieval, truncation, page ranges, URL detection, and page filtering.

### Changed

- JSON output format for `convert` now returns `{"source", "tokens", "content"}` instead of embedding frontmatter inside the markdown string.
- `ConvertCommand.input` changed from `PathBuf` to `String` to support URLs as input.

## [0.1.0] - 2025-12-22

### Added

- Initial release.
- Background service that watches directories for documents and converts them to Markdown using markitdown.
- Full-text search indexing with Tantivy.
- `ingestr service run/start/stop/restart/status` for daemon management.
- `ingestr search` for querying the search index.
- `ingestr convert` for ad-hoc single file and batch directory conversion.
- Stdin support for piped input with format auto-detection.
- VLM processor for image understanding via EAVS.
- OCR processor with tesseract, surya, and easyocr backends.
- Configurable processor pipeline with file type routing.
- LLM-based image description generation.
- XDG-compliant configuration, data, and state directories.
- MCP server (`ingestr-mcp`) exposing search functionality to AI assistants.
- Shell completion generation.
- JSON and YAML output formats.
- Parallel batch processing with `--parallel N`.
