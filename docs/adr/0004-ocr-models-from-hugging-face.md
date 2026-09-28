# ADR-0004: PP-OCR models come from Hugging Face, pinned by SHA-256

Status: accepted (2026-09-28)

## Context

ADR-0002 made PP-OCRv6 the default OCR backend through liteparse's
`oar-ocr-auto-download` feature. That feature downloads the ONNX models on
first use from a personal ModelScope mirror (`greatv/oar-ocr`) into a per-user
`~/.oar`. The files are SHA-256 verified, so integrity was never the problem.
The problems were:

- **Source**: a third-party mirror we do not control, instead of the model
  author. Hard to justify in an institutional audit and a single point of
  availability failure.
- **Multi-user hosts** (oqto, ADR-0009 there): every user downloads their own
  copy, and there is no way to seed models once per host or run offline.
- **Reuse**: nym needs the same models; `~/.oar` is a cache no other tool uses.

PaddlePaddle publishes the same ONNX exports on Hugging Face
(`PaddlePaddle/PP-OCRv6_{tiny,small,medium}_{det,rec}_onnx`, `inference.onnx`,
Apache-2.0). They are byte-identical to what oar pins. The recognition
dictionaries are not separate files there; they live in `inference.yml` and,
rebuilt from it, are byte-identical to oar's pinned dictionaries.

## Decision

- Build liteparse with `oar-ocr` only; auto-download stays off (enforced by
  `scripts/drift-check.sh`).
- `ingestr-core::models` resolves each model through the standard Hugging Face
  cache with the official `hf-hub` client, in this order:
  1. `$INGESTR_SHARED_HF_HOME/hub`, a read-only cache seeded once per host;
  2. the user's cache (`$HF_HOME/hub`, default `~/.cache/huggingface/hub`);
  3. download from Hugging Face into (2), unless `HF_HUB_OFFLINE=1`.
- Every file is checked against a SHA-256 pinned in code (oar's digests). A
  mismatch or a missing model while offline is a hard, explained error.
- The two dictionaries are embedded with `include_str!`.
- The engine is built with `OarOcrEngine::from_models(det, rec, dict)`.
- No separate crate: nym copies the module (a file list plus hashes) until the
  two copies actually diverge.

## Consequences

- Hosts can pre-seed models with `HF_HOME=<shared> huggingface-cli download ...`
  and set `INGESTR_SHARED_HF_HOME`; users never download.
- Models sit next to other Hugging Face models and are visible to standard
  tooling; `ingestr doctor` reports where they are.
- Updating a model means changing a pinned hash in code, deliberately.
- Existing `~/.oar` directories are no longer used and can be deleted.
