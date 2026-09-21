#!/usr/bin/env bash
#
# Drift check (tmpl-8adx): verify that the facts this repo documents
# (commands, crate/version references, JSON/TOML-only constraint) match the
# machine-checked configuration (the actual Cargo.toml manifests and PATH).
#
# Anything this script checks is "source of truth" in AGENTS.md — if it fails,
# either the code or the documentation drift must be fixed.

set -u
FAILED=0

echo "==> Required commands on PATH"
for cmd in cargo just trx python3; do
  if command -v "$cmd" >/dev/null 2>&1; then
    echo "  ok: $cmd -> $(command -v "$cmd")"
  else
    echo "  FAIL: required command not found on PATH: $cmd"
    FAILED=1
  fi
done

echo "==> No YAML application format (JSON/TOML only)"
if rg -l 'serde_yaml|serde-yaml|yaml' --glob 'Cargo.toml' . >/dev/null 2>&1; then
  echo "  FAIL: found 'serde_yaml'/'yaml' in a Cargo.toml"
  rg -n 'serde_yaml|serde-yaml' --glob 'Cargo.toml' . || true
  FAILED=1
else
  echo "  ok: no serde_yaml dependency in any manifest"
fi

echo "==> Workspace config crate: default-features=false, json/toml only"
python3 - "$FAILED" <<'PY'
import sys
from pathlib import Path

failed = int(sys.argv[1])
root = Path('.')
manifests = list(root.rglob('Cargo.toml'))
found_config = False
for m in manifests:
    text = m.read_text()
    if 'config' not in text:
        continue
    for line in text.splitlines():
        s = line.strip()
        if s.startswith('config') and ('version' in s or 'features' in s):
            found_config = True
            if 'default-features = false' not in s:
                print(f"  FAIL: {m}: config dep missing default-features = false")
                failed = 1
            if 'yaml' in s:
                print(f"  FAIL: {m}: config dep enables yaml")
                failed = 1
            if 'json' not in s or 'toml' not in s:
                print(f"  WARN: {m}: config features do not mention both json and toml: {s}")

if found_config and failed == 0:
    print("  ok: config crate is default-features=false with json/toml only")

sys.exit(failed)
PY
FAILED=$?

echo "==> Workspace resolves (cargo metadata)"
if cargo metadata --no-deps --format-version 1 >/dev/null 2>&1; then
  echo "  ok: cargo metadata resolves"
else
  echo "  FAIL: cargo metadata --no-deps does not resolve"
  FAILED=1
fi

echo
if [[ "$FAILED" -eq 0 ]]; then
  echo "drift-check: PASS"
else
  echo "drift-check: FAIL"
  exit 1
fi