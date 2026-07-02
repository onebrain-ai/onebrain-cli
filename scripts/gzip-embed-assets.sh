#!/usr/bin/env bash
# Gzip the embedded web UI's hashed `assets/` in place so `onebrain serve` can
# ship them with `Content-Encoding: gzip` (see crates/onebrain-cli/src/server/
# static.rs — it detects the gzip magic and adds the header; the browser inflates).
#
# Only `assets/` is touched — the root files the CLI reads itself (index.html,
# version.json, changelog.json, favicon) stay raw. Idempotent (skips files that
# are already gzip) and a no-op on a fresh checkout (webui/ holds only .gitkeep).
# Run from the repo root, before `cargo build`, after the dist is in place.
#
# CAUTION: this gzips in place. Do NOT then `onebrain serve --dir <this folder>`
# — the `--dir` path serves files raw (no Content-Encoding), so the browser would
# get gzip bytes it can't read. `--dir` is for a fresh (un-gzipped) webui dist.
set -euo pipefail

assets_dir="${1:-crates/onebrain-cli/webui/assets}"
if [ ! -d "$assets_dir" ]; then
  echo "gzip-embed-assets: no $assets_dir — nothing to gzip (empty embed?)"
  exit 0
fi

gzipped=0
while IFS= read -r -d '' f; do
  # Check the gzip magic bytes (1f 8b) FIRST, before `gzip -t`. Gating solely on
  # `gzip -t` conflates two cases: a raw file (no magic → re-gzip, correct) and a
  # *corrupt* gzip (has the magic but fails integrity → `gzip -t` also fails, so
  # it would get re-gzipped into a double-wrapped stream the server can't inflate).
  # Split them: has magic + valid → skip (idempotent); has magic + corrupt → fail
  # fast; no magic → gzip below.
  magic=$(head -c2 "$f" | od -An -tx1 | tr -d ' \n')
  if [ "$magic" = "1f8b" ]; then
    if gzip -t "$f" 2>/dev/null; then
      continue # already a valid gzip stream → idempotent skip
    fi
    echo "gzip-embed-assets: $f has the gzip magic but fails integrity (corrupt) — aborting" >&2
    exit 1
  fi
  # -9 max ratio, -n omit name/timestamp (reproducible builds). Separate
  # statements (not `&&`) so a gzip failure aborts under `set -e` rather than
  # silently skipping the file and miscounting.
  gzip -9 -n -c "$f" >"$f.gz"
  # If the in-place swap fails, remove the stray `.gz` so a re-run doesn't trip
  # over a leftover partial artifact, then abort.
  if ! mv "$f.gz" "$f"; then
    rm -f "$f.gz"
    exit 1
  fi
  gzipped=$((gzipped + 1))
done < <(find "$assets_dir" -type f -print0)

echo "gzip-embed-assets: gzipped $gzipped file(s) in $assets_dir"
