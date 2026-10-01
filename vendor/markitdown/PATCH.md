# Vendored patch: markitdown 0.1.11

Upstream: https://github.com/uhobnil/markitdown-rs (crates.io `markitdown`,
MIT). This directory is the published 0.1.11 source, applied via
`[patch.crates-io]` in the workspace `Cargo.toml`.

## Why

The upstream library prints debug output with `println!` from library code
(`src/excel.rs`, `src/image.rs`, `src/pptx.rs`, `src/llm.rs`). Those lines go
to the calling process's stdout, which breaks ingestr's machine-mode contract
(`ingestr convert file.xlsx --json` produced invalid JSON) and pollutes plain
stdout output.

## Delta from upstream

- Every `println!(` in the four files above is replaced with `log::debug!(`.
- The never-read `TableStat.index` field and its `count` counter in `src/pptx.rs`
  removed (dead-code
  warning on every build).
- `log = "0.4"` added as a dependency (`[dependencies.log]`).
- Registry metadata files (`.cargo-ok`, `.cargo_vcs_info.json`,
  `Cargo.toml.orig`), the crate's own `Cargo.lock`, and its `tests/` fixture
  directory (2.4 MB, not used by ingestr) removed. No other source changes.

## Refreshing

When bumping to a newer upstream release, re-copy the crate from
`~/.cargo/registry/src/*/markitdown-<version>` and re-apply the replacement:

```bash
python3 - <<'PY'
import re
for f in ["src/excel.rs","src/image.rs","src/pptx.rs","src/llm.rs"]:
    s = open(f).read()
    open(f, "w").write(re.sub(r'\bprintln!\(', 'log::debug!(', s))
PY
```

Then verify `rg -n 'println!' src --glob '!main.rs'` prints nothing, and
consider dropping this patch once the fix is upstream.
## Spreadsheets: .xls/.ods and every sheet (ingestr-6vsx)

- `src/excel.rs`: open with calamine `open_workbook_auto` (xlsx, xlsm, xlsb,
  xls, ods) instead of `Xlsx` only, render every non-empty sheet under a
  `## <sheet>` heading (no heading for single-sheet workbooks), and escape `|`
  in cells.
- `src/lib.rs` `detect_file_type`: when content sniffing reports a legacy OLE
  type (`msi`/`doc`/`xls`/`ppt`) and the file name has one of those
  extensions, use the name. `infer` reports `msi` for .xls files without a
  class ID.
