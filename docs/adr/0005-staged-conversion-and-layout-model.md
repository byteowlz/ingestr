# ADR-0005: Staged conversion, with a layout model in the slow stage

Status: accepted (2026-09-29)

## Context

oqto converts every upload. Native PDF text takes milliseconds per page; OCR
and layout analysis take about a second per page on CPU. Users should not wait
for the slow part before they can see and search what is cheap to get.

Separately, ingestr loses vector charts and diagrams (only their axis labels
survive, sometimes as a fake table) and cannot find figures or tables on
scanned pages. A layout model fixes both. A spike on 14 pages (3 charts,
7 tables, 1 picture) compared the PaddlePaddle layout models on CPU:

| Model | Size | ms/page | Charts | Tables |
|---|---|---|---|---|
| PP-DocLayout-S | 5 MB | ~75 | 3/3 | 2/7 |
| PP-DocLayout-M | 23 MB | ~280 | 2/3 | 4/7 |
| PP-DocLayout_plus-L | 125 MB | ~900 | 3/3 | 7/7 |
| PP-DocLayoutV3 | 125 MB | ~1200 | 3/3 | 7/7 |

## Decision

- **Two stages.** Stage 1 parses native text with OCR and layout off and hands
  the document to the caller at once. Stage 2 runs only when there is more to
  get (the OCR gate fired, or layout is enabled and a page qualifies): it
  re-parses with OCR and adds layout results, then returns the final
  document. Re-parsing native text costs milliseconds, so stage 2 stays a
  plain second parse rather than a patch of stage 1.
- **Layout model: PP-DocLayout_plus-L** from `PaddlePaddle/PP-DocLayout_plus-L_onnx`
  on Hugging Face, SHA-256 pinned (ADR-0004 rules). The smaller models miss or
  mislabel too many tables to be trusted.
- **Only selected pages** go through layout: scanned pages (per the OCR gate)
  and pages with a vector figure cluster that is not densely covered by text
  (dense ones are ruled tables). Plain text pages cost nothing. Measured:
  DeepSeek-R1 1/22 pages, FACTUR-X 17/85 pages.
- **Opt-in first** (`--layout`, `[processors.layout] enabled`), until measured
  on real oqto uploads.
- First use: crop charts/figures/images to PNG and link them in reading
  order. Table regions (false-table removal, SLANet+/VLM reconstruction) are a
  follow-up on the same detections.

## Consequences

- A 125 MB model joins the PP-OCR models in the shared HF cache.
- Library hosts get a preview callback; the CLI and MCP keep returning one
  final document.
- Stage 2 replaces stage 1 wholesale, so consumers index the final text again
  rather than merging.
