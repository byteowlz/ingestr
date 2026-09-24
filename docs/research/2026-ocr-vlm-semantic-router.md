# ingestr — OCR/VLM tier models & "jev-like" semantic router (research)

Scope: pick a small **all-rounder set** per tier (not a model zoo), and figure
out how a local open-source "jev-like" router could route documents/pages to
the right tier. Research only — no source changes.

## How ingestr tiers work today (grounding)

- **Tier-0 = LiteParse** (`run-llama/liteparse`, Apache-2.0, v2.14.7) for PDF +
  office formats. It:
  - extracts native text/vector pages via **PDFium** (no model);
  - classifies each page (`is_complex` → `needs_ocr` + reasons);
  - OCRs and merges **only** scanned/text-sparse pages;
  - rebuilds layout (tables, columns, headings) itself from word boxes.
  OCR backend built-in = **Tesseract** (`tesseract` feature). Optional
  **`ocr_server_url`** HTTP seam (Phase-2 target: PaddleOCR-VL) and **`oar-ocr`**
  GPU feature (Phase-3: cuda/tensorrt/directml/coreml/webgpu/openvino).
  `oar-ocr` (crates.io, Apache-2.0) is an ONNX OCR engine from **GreatV** (the
  RapidOCR/PaddleOCR-ONNX ecosystem) — i.e. the PaddleOCR (PP-OCR) model family.
- ingestr's own `OcrBackend` (`tesseract|ocrs|surya|easyocr`) handles the
  non-PDF image path and fallback (`ingestr-cli/src/main.rs`). Surya/EasyOCR
  shell out to Python.
- VLM tier = `--vlm` / `[processors.vlm]`, a plain **OpenAI-compatible vision
  API call** to any endpoint (eval order: `--vlm-model` > `[processors.vlm].model`
  > `[llm].model`; URL `[processors.vlm].llm_url` > `[llm].base_url` >
  `localhost:11434`). This already lets any local VLM slot in as the "easy tier".

> Implication: the CPU/GPU OCR seam already exists. The natural, lowest-friction
> choice is the **PaddleOCR family** — it is exactly what liteparse's native
> `oar-ocr` path (CPU/GPU ONNX) and the `ocr_server_url` target run.

---

# PART A — Model recommendations per tier

## A1. CPU OCR all-rounder (no GPU)

**Recommended default: PP-OCRv5 (mobile), run via RapidOCR / liteparse's
`oar-ocr` ONNX path.**

| Model | Publisher | License | Params | Hardware | Strengths | Weaknesses |
|---|---|---|---|---|---|---|
| **PP-OCRv5** | Baidu / PaddlePaddle | **Apache-2.0** | ~70M total (det+rec, mobile) | CPU only, tiny (~few hundred MB) | Fast (>370 chars/s on Xeon), 48+ langs via RapidOCR, printed text + multilingual, good det boxes/columns for liteparse's own layout rebuild, fully local | Text+boxes only — no structural table/column markdown by itself; weak on handwriting |
| Surya (OCR 2) | datalab-to (VikParuchuri) | **Apache-2.0 code / OpenRAIL-M-modified weights** (free for research/personal/startups <$5M; commercial license for broader use) | 650M | CPU/GPU; heavier RAM than PP-OCRv5 | 83.3% olmOCR-bench (best <3B), handles tables + layout + reading order + decent handwriting | Weights license not MIT/Apache — **licensing caveat for commercial**; heavier |
| GOT-OCR 2.0 | USTC / StepFun | Apache-2.0 | ~580M | CPU/GPU | End-to-end VL OCR, text+tables+formulas, single-pass | Slower on CPU; content-limited vs PP-OCRv5 for plain text throughput |
| dots.ocr | Xiaohongshu (rednote-hilab) | Apache-2.0 | 3B | GPU preferred | Good tables (OmniDocBench 90.77) | Too slow/heavy for a CPU default |

**Why PP-OCRv5 wins here:** it is Apache-2.0 (fully permissive commercial),
tiny, fast on CPU, multilingual, and it pairs *perfectly* with liteparse — liteparse
already does the structural reconstruction (tables/columns/reading order) from
word boxes, so the OCR engine only needs cheap text+boxes. That is exactly the
PaddleOCR-ONNX / `oar-ocr` shape liteparse is built around.

**Fallback for harder-but-still-CPU:** **Surya** (better tables + handwriting),
but accept the OpenRAIL-M weight-license restriction if commercial.

## A2. GPU OCR / doc-layout (the "hard 10–20%": dense/noisy tables, multi-column,
charts, old scans)

**Recommended default: PaddleOCR-VL-1.5 (or -1.6), served via liteparse
`ocr_server_url` or as a VLM.**

| Model | Publisher | License | Params | OmniDocBench v1.5 Overall / latency | Strengths | Weaknesses |
|---|---|---|---|---|---|---|
| **PaddleOCR-VL-1.5 / -1.6** | Baidu/PaddlePaddle | **Apache-2.0** | 0.9B | **94.93 / ~0.038s** ; -1.6 = **96.34 / ~0.033s** | Best specialized OCR-VLM; text+tables+formulas+charts+seals, 109 langs; outputs Markdown directly; runs CPU too (GPU better); SOTA at 0.9B | VLM needs a serving stack (vLLM/llama.cpp) |
| GLM-OCR | Zhipu (zai-org) | **MIT** | 0.9B | 95.22 / ~0.044s | MIT (most permissive), near-top score | Same VLM serving cost |
| Granite-Docling | IBM | Apache-2.0 | 258M | (docling-eval focus) | Ultra-compact end-to-end conversion, strong structure | Smaller scale; Latin-script-optimised |
| DeepSeek-OCR-2 | DeepSeek | Apache-2.0 | 3B | 90.25 / ~0.050s | Reads complex/mixed layouts, strong reading order | Heavier (3B), lower OmniDocBench than PaddleOCR-VL |
| dots.ocr | Xiaohongshu | Apache-2.0 | 3B | 90.77 / ~0.048s | Strong tables | Heavier, lower overall |

**Why PaddleOCR-VL here:** top OmniDocBench v1.5 score at 0.9B, Apache-2.0,
multilingual, emits Markdown (tables/charts/formulas) in one pass. It is the
"hard-tier" complement to PP-OCRv5 and its natural home is the existing
`ocr_server_url` seam (or `--vlm`). **GLM-OCR (MIT)** is the permissive
near-peer alternative if you prefer MIT or Baidu-vendor-neutral.

## A3. VLM — "easy tier" (describe diagrams, charts, screenshots, handwriting)

**Recommended default local endpoint: Qwen3-VL-8B-Instruct** via Ollama/vLLM
(an OpenAI-compatible endpoint → plug straight into `--vlm`).

| Model | License | Sizes | Hardware | Fit |
|---|---|---|---|---|
| **Qwen3-VL** | **Apache-2.0** | 2B/4B/8B/30B-A3B/32B/235B-A22B | 8B ≈ 8–16GB VRAM; 4B for ~8GB | Best local all-rounder: OCR (OCRBench ~896 @8B), diagrams, charts, screenshots, handwriting, agentic |
| GLM-4V / GLM-OCR | MIT | 4V/4.6V | mid | Permissive, strong vision |
| InternVL3 / InternVL3.5 | MIT | 8B/78B/241B | mid/large | Strong general image understanding |
| LLaVA-NeXT / MiniCPM-V / Florence-2 | MIT (Florence-2) | varied | small/mid | Lighter, but less OCR/handwriting strength |

**Recommendation:** **Qwen3-VL-8B-Instruct** as the default `--vlm` model
(Apache-2.0, best balance of capability vs VRAM, runs locally via Ollama's
`/v1` OpenAI-compatible API at `localhost:11434`). Drop to **Qwen3-VL-4B** on
8GB cards; step up to **Qwen3-VL-30B-A3B** (MoE) if VRAM allows. Use GLM-4V as
an MIT permissive alternative.

## Recommended default per tier — summary table

| Tier | Engine | Recommended default | License | Hardware |
|---|---|---|---|---|
| Native text | LiteParse/PDFium | (already in place) | Apache-2.0 | none |
| CPU OCR | **PP-OCRv5** (RapidOCR / `oar-ocr`) | **PP-OCRv5 mobile** | Apache-2.0 | CPU only |
| CPU OCR fallback | Surya (OCR 2) | Surya 650M | Apache-2.0 code / OpenRAIL-M **weights** | CPU/GPU |
| GPU / hard tier | OCR-VLM | **PaddleOCR-VL-1.5** (or -1.6) — via `ocr_server_url` | Apache-2.0 | GPU (or CPU 0.9B) |
| GPU alt | GLM-OCR | GLM-OCR | **MIT** | GPU |
| VLM (easy tier) | Local VLM | **Qwen3-VL-8B-Instruct** (Ollama `--vlm`) | Apache-2.0 | GPU/Apple Silicon |

---

# PART B — "jev / kev" leverage

## B1. What `jev` actually is (and whether there's a local open-source "kev")

**`jev` is a proprietary hosted API, not an open-source model.**
- `hive/hive/jev_backend.py`: endpoint **`https://api.typesafe.ai/v1/systemone`**,
  model `jev-latest` / `jev-1.13.0` (TypeSafe), Bearer API key. No weights, no
  server you can run.
- It answers a **state + question set** → returns a *tool choice* plus three
  scalar gates: `safe_to_execute`, `requires_reasoning`, `needs_llm`. The
  question set is versioned as **`hive-routing-v1`** (`hive/semantic_schema.py`).
- Hive consumes it via `SemanticRoutingPolicy` + `CascadeRoutingPolicy`
  (`semantic_factory.py`), with modes `off|shadow|cascade|compare`.
- **`docs/benchmarks/jev-calibration.md`** is the key lesson: the default
  thresholds are **unusable** against live jev (`safe_to_execute` max 0.41,
  `requires_reasoning` mean 0.41, `needs_llm` mean 0.37 → every decision would
  escalate). Tool *choice* is the informative signal; the three scalar gates are
  a "caution" signal, not a permission. The doc explicitly recommends deploying
  **shadow** mode first, building a coverage/fidelity curve, and **not** treating
  thresholds as validated.

**There is no "kev" in byteowlz.** I grepped the whole workspace, the cersei
wiki (`external-repos/cersei/wiki/`), and all external-repos: every `kev` hit is
**CISA KEV** (Known Exploited Vulnerabilities, in `tirith`) or an IETF language
subtag. No semantic-router called "kev" exists. The **local open-source
alternative Hive itself anticipates is "d-Jeff"** — `hive/djeff_backend.py` is a
backend-agnostic **placeholder** with the identical interface to `JevBackend`;
it raises `SemanticBackendError` until a real d-Jeff checkpoint is configured.
So: there is **no ready local jev clone** in the repos today; `d-Jeff` is the
intended seam but unimplemented.

## B2. Open-source local semantic-routing / model-routing options

| Option | License | How it works | Pros | Cons |
|---|---|---|---|---|
| **semantic-router** (aurelio-labs) | **MIT** | Embedding + example-route phrases, classify via vector similarity | Tiny, fast, no LLM call, permissive | Needs per-route example phrases (curation); Python |
| **RouteLLM** (LMSYS) | Apache-2.0 | Trained/eval router that sends "easy" queries to cheap models, "hard" to strong | Proven, benchmarked | Focused on LLM cost, not doc tiers |
| Small embedding + linear/SVM classifier | (embedding model license) | Embed a feature/snippet vector → logistic regression over few classes | Most compact, fully local, deterministic | Needs labelled data |
| Local LLM routing | varies | A small local LLM reads the state and outputs a tier label | Flexible | Slow, adds model + latency |
| Hash / lexical rules | n/a | Extension + text-density + page-class rules | Zero model, instant | No semantic nuance |

**Best fit for ingestr:** either **`semantic-router`-style embedding route
classification**, or simpler still, a **small embedding + shallow classifier**
over page/doc features. Both are comically cheap vs a 7B LLM router.

## B3. Concrete proposal: a local "jev-like" router for ingestr

**Goal:** route each document (or PDF page) to one of a **small closed set of
tiers** — not a model zoo. The tier set is already fixed:

```
T0 native text   (liteparse PDFium / markitdown)      → default
T1 CPU OCR       (PP-OCRv5 via RapidOCR/oar-ocr)      → scanned/text-sparse
T2 GPU model     (PaddleOCR-VL via ocr_server_url)    → hard 10-20%
T3 VLM           (Qwen3-VL via --vlm)                 → images/diagrams/charts
T4 skip          (non-doc / too low value)
```

**Routing input (what the router sees)** — all cheap, most already produced:
- per-page `is_complex` / `needs_ocr` + reasons from liteparse (already there);
- file **extension** (pdf/docx/xlsx/pptx/html/image/txt);
- **text density** (chars/page) and page count;
- **layout stats**: table-cell density, column count, bbox scatter, math/diagram
  heuristics;
- a **content stub** (filename + first-page snippet) — only when a semantic
  component is wanted.

**Routing signal (what it decides):** essentially a *confidence that the native
text tier suffices*. If native extraction is already high-density/clean → `T0`.
If `is_complex`/low density → `T1` (CPU OCR). If the page is table/handwriting/
chart heavy or very noisy → `T2` (GPU model). If it's an image/PPT/diagram page
(or any page the text/OCR tiers yield too little) → `T3` (VLM). `T4` to skip
garbage.

**Why a router beats static config:** static `backend="ocrs"` or
`ocr_server_url` is binary and forces the same tier for every doc. A router lets
*the cheap tier absorb the easy 80%* and only spend GPU/VLM on the hard 10–20%,
auto-tuned per page — exactly what liteparse already does *within* a PDF. The
amount of routing signal is small and already computed; a router just combines
it.

**But keep it simple:** a full embedding/LLM router is `YAGNI` here. Because the
decision is a **closed, low-cardinality split**, recommend:

1. **Primary = deterministic heuristic** over existing signals (liteparse
   `is_complex` + extension + text density + layout stats). This already
   covers most of it and is what ingestr does today for pages.
2. **Optional semantic layer (jev/d-Jeff-like)** only for the *ambiguous band*
   where the heuristic is low-confidence — e.g. a doc that's neither clearly
   text nor clearly scanned, an image whose content type is unclear, or a PPT
   that might warrant VLM. A small embedding+classifier or `semantic-router`
   instance classifies that band into the few tiers. Run it in **shadow-like**
   mode first (log, don't act) to build a coverage/fidelity curve, mirroring
   hive's jev-calibration lesson.

This keeps the router to a **tiny, closed decision** and avoids a model zoo.

## B4. Trade-offs

- **Latency:** deterministic heuristic = sub-ms. Embedding+classifier ≈ 1–5ms
  with a cached small encoder. LLM-based router = 100s of ms + GPU. For a
  watch-daemon converting many docs, the cheap options win.
- **Model footprint:** a small embedding encoder (~40–100MB) or
  `semantic-router` (MIT) is negligible; a 7B router is not. Prefer tiny.
- **Per-user cache (multi-user oqto):** cache the router *decision* keyed by a
  content/document hash (not per-user path) so repeated/converted docs and
  shared docs reuse decisions across users. Embeddings can be cached too; the
  is_complex/layout stats are already cached. Avoid re-embedding on every
  watch event.
- **License:** use MIT/Apache building blocks (`semantic-router` = MIT;
  embeddings = Apache/MIT) to avoid commercial restrictions. **Avoid Surya
  weights for anything commercial** (OpenRAIL-M). Avoid the hosted `jev` API
  for anything needing local/offline or per-user cost control.
- **Does a deterministic heuristic suffice?** Largely **yes.** liteparse already
  classifies pages → the hard routing (native vs scanned) is solved. The main
  gaps a router could add value on are: whole-file triage for images/PPTs
  (does it need VLM?), and picking T2 GPU vs T1 CPU for the genuinely hard
  scans. Even there, a small classifier over existing stats is enough. **A true
  semantic/embedding router should be a thin, optional escalation — not the
  backbone.**

---

## Bottom-line recommendation

- **CPU OCR default:** **PP-OCRv5** (Apache-2.0) via RapidOCR/liteparse
  `oar-ocr`. Cheap, permissive, and the native fit.
- **GPU/hard default:** **PaddleOCR-VL-1.5/-1.6** (Apache-2.0, 0.9B) via
  `ocr_server_url`; **GLM-OCR** (MIT) as the permissive alternative.
- **VLM default:** **Qwen3-VL-8B-Instruct** (Apache-2.0) via Ollama as the
  `--vlm` OpenAI-compatible endpoint.
- **Routing:** keep liteparse's deterministic `is_complex` routing as the
  backbone; add an **optional tiny local semantic router** (embedding+classifier
  or `semantic-router`, MIT) only for the ambiguous band, run shadow-first.
  There is no "kev" to adopt — the intended local replacement is Hive's
  **d-Jeff**, which is still an unimplemented placeholder.
---

# UPDATE — "kev" is real: jaredpalmer/kev (open-source Jev-alike)

The earlier B1 conclusion ("no kev exists") was wrong — the user pointed to
**https://github.com/jaredpalmer/kev**.

## What Kev is

**Kev** is a family of small decision models built on **Qwen3.5**, based on
"Jev's Architecture Unmasked", that you can train and run yourself.

- **Sizes:** 0.8B / 4B / 9B (Qwen3.5 base). Start with 4B; 9B for accuracy; 0.8B
  for smallest footprint.
- **License:** **Apache-2.0** (weights + code).
- **API:** **matches TypeSafe's System One** (`/v1/systemone`), so it's a drop-in
  local replacement for the hosted `jev` semantic router. The TypeSafe Python
  SDK works pointed at a local `kev` server.
- **Question types:** `noul` (yes/no), `choice` (multiple-choice), `score`
  (rating). Questions share the input text but can't read each other.
- **Runs locally:** CUDA, ROCm, Apple Silicon (MLX). 4B/9B fit a 32 GB Mac.
  Python 3.12/3.13 + `uv`; serve via `uv run ... python -m kev.serve --run
  jaredpalmer/kev-4b --port 8009`.
- **Calibration:** probabilities calibrated by default (temperature fitted in
  model card); returns probabilities per option, not just a single label.
- **Benchmarks (dev/test, new sources):** Kev-0.8B 0.652/0.684, Kev-4B
  0.797/0.837, Kev-9B **0.822/0.852**; hosted Jev 0.857 (not a controlled
  comparison). Brier: Kev-9B 0.286/0.237.

## How Kev maps to ingestr routing

Kev's `choice` question is a perfect fit for the closed tier set. Feed a compact
document/page **state**, ask **"which tier?"** with the tiers as `criteria`, and
use the returned probabilities (calibrated) to route:

```
state: "scanned German tax page, 2 columns, 3 tables, no text layer, ext=pdf"
questions:
  tier: { "type": "choice",
          "instructions": "Which ingestion tier should this page use?",
          "criteria": { "native": "clean text/vector", "cpu_ocr": "scanned/text-sparse",
                        "gpu": "dense tables/charts/handwriting", "vlm": "image/diagram/screenshot",
                        "skip": "non-document/garbage" } }
→ { "answers": { "tier": { "choice": "gpu", "probabilities": {...} } } }
```

## Integration approach for ingestr

Kev is served as an **HTTP System-One endpoint**, exactly like two seams ingestr
already has: the **VLM** (`--vlm`, OpenAI-compatible `/v1/chat/completions`) and
the **liteparse `ocr_server_url`** (HTTP OCR). So:

- Add a **`[routing]` config** with a `coremodel_url` / `router_url` (System-One
  endpoint) + model name + optional API key, and a `mode`: `heuristic` (default)
  | `shadow` | `route`. 
- **Heuristic (default):** deterministic `is_complex` + extension + text density
  (fast, no model). This stays the backbone.
- **Route/shadow:** POST the per-page/per-doc state to Kev, get calibrated tier
  probabilities, and either act on them (`route`) or just log the decision
  (`shadow`) to build a coverage/fidelity curve before enabling — mirroring
  hive's `jev-calibration.md` shadow-first lesson.
- **Cache** the decision by **content/document hash** (not per-user) so shared or
  re-converted docs reuse decisions in a multi-user oqto deployment.

## Recommendation

Use **Kev-4B** (or 0.8B) as the optional semantic tier-router behind a
`heuristic` default. Pay attention to GPU/VRAM footprint and latency: a decision
model is slower than a deterministic heuristic, so keep it on the **ambiguous
band** only (not every page), keyed by content hash. Kev is Apache-2.0, so no
commercial license concern.
