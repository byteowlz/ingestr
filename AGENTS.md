# AGENTS.md

Guidance for coding agents working on this Rust workspace. This file is
deliberately short: it carries the enforceable boundaries and workflows, and
points at machine-checked configuration instead of duplicating any inventory.

## Source of truth

Static facts about this repo are machine-checked, **not** copied here:

- Crate list, dependency versions, lint settings → the workspace `Cargo.toml`.
  The authoritative enumeration is `cargo metadata --no-deps --format-version 1`.
- Task commands → `just` (run `just` to list).
- Issues → `trx` (see "Issue tracking" below).
- Drift guard → `scripts/drift-check.sh` (verifies commands exist and that
  documented crate/version references and the JSON/TOML-only constraint match
  the real manifests). Run it after touching any manifest or doc claim.

If `cargo metadata`, `just list`, or `scripts/drift-check.sh` disagree with
anything written below, the command is right and this file is wrong.

## Domain and architecture

- Read `CONTEXT.md` before domain work; keep it a glossary only.
- Read `docs/adr/` before architectural changes; add an ADR only for
  hard-to-reverse decisions with a real trade-off (`docs/adr/README.md`).
- Layout: `ingestr-core` is the only library crate and the dependency root;
  the two binaries (`ingestr-cli`, `ingestr-mcp`) depend on it but never on
  each other (`cargo metadata` is the map).
- Never publish to a public registry without explicit user approval.

## Strict lints

`[workspace.lints.clippy]` is strict. Key constraints:

- `unsafe_code = "forbid"`; `unwrap_used`, `expect_used`, `panic`, `todo`,
  `unimplemented`, `dbg_macro`, `exit` = deny — propagate with `?`,
  `anyhow::Result`, `.context("...")`.
- A small set of style/pedantic lints is `allow` to avoid churn on mature code
  (matching the posture of oqto). See the `INTENTIONALLY ALLOWED` block in
  `Cargo.toml`.
- Output macros (`print_*`) are allowed for CLIs.

## Workflow

- Domain terms: use `CONTEXT.md`.
- Add code: follow the crate's dominant pattern (CLI subcommands, MCP tools).
- Before committing anything significant: `just check` (fmt + clippy + test).
- Run `scripts/drift-check.sh` after touching manifests or versioned claims.

## Application formats: JSON and TOML only

- Configuration and machine output are **JSON or TOML only**. Never add a YAML
  output mode, a YAML dependency, or a YAML example. `serde_yaml` is banned.
- Root `config` crate runs with `default-features = false` and only the
  `json`/`toml` features — keep it that way (no `yaml` feature).
- Note: converted-document frontmatter is serialized as **JSON** inside a
  `---` block (JSON is a valid YAML subset) precisely to avoid a YAML
  dependency while staying compatible with standard frontmatter consumers.

## Configuration & storage

- XDG paths; expand `~` and env vars; ship a commented example under
  `examples/`, write a default on first run, override via the `config` crate.
- Env override prefix is derived from `APP_NAME` as
  `upper(PKG_NAME).replace('-', "_")`.

## Issue tracking (trx)

Use `trx` for all issue tracking — never markdown TODOs.

```bash
trx ready --json                                   # find unblocked work
trx create "Title" -t task -p 2 --json             # create (bug/feature/task/epic/chore)
trx update <id> --status in_progress --json        # claim
trx close <id> -r "reason" --json                  # complete with reason
```

Priorities: 0=critical, 1=high, 2=medium (default), 3=low, 4=backlog.
Issue state lives in `.trx/` (JSONL) — commit it with code changes.

## House rules

- Do exactly what the user asks — no unsolicited files.
- Keep README updates concise and emoji-free.
- Never commit secrets or sensitive paths; scrub logs.
- `Cargo.lock` is committed; bump manifests + lock together.