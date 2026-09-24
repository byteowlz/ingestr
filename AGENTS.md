# AGENTS.md

Short guidance for coding agents in this Rust workspace. Machine-checked
config is the source of truth; this file only carries the enforceable rules.

## Source of truth

- Crate list, deps, lints → root `Cargo.toml` (`cargo metadata --no-deps`).
- Task commands → `just` (run `just` to list).
- Issues → `trx`. Drift guard → `scripts/drift-check.sh`.
If they disagree with anything here, the command is right.

## Domain & architecture

- Read `CONTEXT.md` before domain work (ingestr = watch/convert/index documents
  to Markdown for full-text search). Add ADRs in `docs/adr/` only for
  hard-to-reverse trade-offs.
- Layout: `ingestr-core` is the library dependency root; `ingestr-cli`/
  `ingestr-mcp` depend on it but never on each other.
- Never publish to a public registry without explicit approval.

## Lints

`[workspace.lints.clippy]` is the source of truth. `unsafe_code` and
`unwrap`/`expect`/`panic`/`todo`/`dbg_macro` are denied; propagate with `?` /
`anyhow::Result` / `.context(...)`.

## Formats

Prefer **JSON/TOML** for config and machine output. Don't add `serde_yaml` as a
new dependency; leave existing YAML alone. Markdown frontmatter is serialized
as JSON (a valid YAML subset).

## Issue tracking

Use `trx`, never markdown TODOs. Commit `.trx/` changes with the code.

## House rules

- Do exactly what's asked; no unsolicited files.
- Keep README updates concise, emoji-free.
- Never commit secrets. `Cargo.lock` is committed (bump with manifests).
- Run `just check-all` (fmt + clippy + drift + tests) before committing.