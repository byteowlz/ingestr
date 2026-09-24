# PP-OCR as the default OCR engine, and the LiteParse/markitdown format split

For a large mixed pile the scanned pages are the throughput wall and the OCR
default must be fast, CPU-only and permissively licensed, so we made PP-OCR
(PaddleOCR family, via LiteParse's bundled `oar-ocr` ONNX runtime) the default
OCR backend for both PDF pages and standalone images, and we settled the engine
split per format on measured output quality: PDF and PPTX/DOCX on LiteParse,
spreadsheets and everything else on markitdown.

Status: accepted

## Considered options

- **Keep `ocrs` as the OCR default**: pure Rust, but ~28 s/page on our test
  scans and weaker on tables; PP-OCR measured ~8 s/page on the same page with
  better recall. Rejected as default, kept as an optional backend.
- **Tesseract (LiteParse built-in) as default**: zero download, but lower
  accuracy than PP-OCR on the benchmarks in `docs/research/`. Kept as the
  fallback when the Paddle engine cannot be built.
- **PP-OCR via `oar-ocr` (chosen)**: Apache-2.0, ~70M parameters, models
  auto-downloaded and SHA-256 verified into `$OAR_HOME` (default `~/.oar`),
  selectable size (`paddle_model = tiny|small|medium`, default `small`). One
  engine instance is shared per process.
- **Everything through LiteParse**: rejected for spreadsheets after an A/B
  (`--engine`) showed markitdown emits proper Markdown tables with the header
  row for every sheet, while the LiteParse (LibreOffice → PDF) path drops the
  header and flattens small sheets. markitdown's PPTX converter is broken, so
  presentations stay on LiteParse.

## Consequences

- Scanned pages are OCR'd several times faster by default, with no external
  tool; a first run needs network access to fetch ~50 MB of models.
- Adds a native ONNX runtime (`ort`) to the build. `oar-ocr 0.8` only builds
  against `ort 2.0.0-rc.12`, and Cargo treats rc pre-releases as compatible,
  so the version is pinned in `Cargo.lock` and enforced by
  `scripts/drift-check.sh`. Revisit when LiteParse moves to `oar-ocr 0.9`.
- Spreadsheets no longer require LibreOffice.
- OCR is gated per document: office formats are never OCR'd (they always have
  a native text layer after LibreOffice), and for PDFs a cheap `is_complex`
  pre-pass enables OCR only for pages that are scanned, text-less, garbled, or
  sparse-with-images. On a mixed pile this removed ~90% of OCR work; the
  `ocr_sparse_pages` setting restores recall-first behaviour.
- `--engine liteparse|markitdown` exists to re-run this A/B on new inputs; the
  engine is part of the conversion cache key.
- In a multi-user deployment the `~/.oar` model cache is per user unless
  `OAR_HOME` points at a shared read-only location.