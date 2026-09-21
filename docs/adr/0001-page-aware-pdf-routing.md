# Page-aware PDF routing for OCR

Mixed-content PDFs (some text pages, some scanned pages) were previously handled
either by markitdown (native text only, silently dropping scanned pages) or by
whole-document OCR (OCR-ing every page, including native text ones). We decided
to classify each PDF page and route text/vector pages to fast native Markdown
extraction while sending only scanned pages to the selected OCR backend, using
the `pdf-inspector` crate for per-page detection.

Status: accepted

## Considered options

- **Whole-document OCR (current behavior)**: simple but wastes compute on text
  pages and is slow. Rejected for the Tier-0 CPU path.
- **markitdown then whole-OCR fallback (current behavior)**: for a mixed PDF
  markitdown returns the text pages (non-empty), so the OCR fallback never runs
  and scanned pages are silently lost. Rejected.
- **Per-page routing via `pdf-inspector` (chosen)**: `extract_pages_markdown`
  returns per-page Markdown plus a `needs_ocr` flag from PDF internals (text
  operators, images) with no model load (~10-50ms/page). Only the flagged pages
  are rendered (pdftoppm) and OCR'd; the rest stay on the native path.

## Consequences

- Scanned pages in mixed PDFs are no longer dropped; text pages are no longer
  OCR'd, so CPU cost scales with the number of truly scanned pages.
- Adds a `pdf-inspector` dependency (MIT, pure Rust, no OCR/render features
  enabled — poppler `pdftoppm` remains the rasterizer).
- `[processors.ocr]` gains `page_routing` (default `true`) and `page_dpi`
  (default `300`); setting `page_routing = false` restores whole-document OCR.
- Routing is only active when OCR is enabled for a PDF; otherwise the markitdown
  path is unchanged.
- A future GPU tier can replace the per-page OCR backend while keeping the
  classification and merge logic unchanged.