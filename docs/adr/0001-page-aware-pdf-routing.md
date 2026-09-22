# Page-aware PDF routing via LiteParse

Mixed-content PDFs (some text pages, some scanned pages) previously hit either
markitdown (native text only, silently dropping scanned pages) or
whole-document OCR (OCR-ing every page, including native text ones). We adopted
**LiteParse** (`run-llama/liteparse`, Apache-2.0) as the Tier-0 PDF parser: it
classifies each page (`is_complex` → `needs_ocr` + reasons), extracts native
Markdown for text/vector pages (PDFium, ~2-5ms/page, no model), and OCRs and
merges only the scanned/text-sparse pages itself. This superseded the earlier
hand-rolled pdf-inspector routing.

Status: accepted

## Considered options

- **Whole-document OCR**: simple but wastes compute on text pages. Rejected.
- **markitdown then whole-OCR fallback**: a mixed PDF returns the text pages
  (non-empty), so the OCR fallback never runs and scanned pages are lost.
  Rejected.
- **Hand-rolled per-page routing via `pdf-inspector`**: worked, but `liteparse`
  beats it on every benchmark (ParseBench 0.364 vs 0.283; olmOCR 39.6 vs 33.7;
  opendataloader 0.886 vs 0.842) and also beats markitdown (ParseBench 0.364 vs
  0.185). Superseded — we removed the pdf-inspector dependency and the routing
  code.
- **LiteParse (chosen)**: single engine for native extraction + per-page
  classification + selective OCR + OCR merge, with an `ocr_server_url` HTTP seam
  (Phase 2: PaddleOCR-VL) and `oar-ocr` GPU features (Phase 3). Built-in
  Tesseract OCR. PDFium is auto-downloaded at build (musl supported) and loaded
  at runtime.

## Consequences

- Scanned pages in mixed PDFs are no longer dropped; text pages are not OCR'd.
- PDF parsing quality and speed improve substantially (table, heading and
  multi-column reconstruction; 187-page German dissertation in ~2.7s).
- Adds `liteparse` + `tokio` deps. `pdf-inspector` is removed.
- `[processors.ocr]` gains `ocr_server_url` (optional OCR HTTP server) and
  `page_dpi`; `page_routing` was removed as liteparse routes internally.
- A future GPU tier plugs in via `ocr_server_url` or the `oar-ocr` feature,
  without changing the parser.
- Ingest's own OCR backends (ocrs/surya/easyocr via `run_ocr`) remain for the
  non-PDF image path and as a fallback.