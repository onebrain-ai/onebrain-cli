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
  # Already a gzip stream? (idempotent re-runs / partial builds) → skip.
  if gzip -t "$f" 2>/dev/null; then
    continue
  fi
  # -9 max ratio, -n omit name/timestamp (reproducible builds). Separate
  # statements (not `&&`) so a gzip failure aborts under `set -e` rather than
  # silently skipping the file and miscounting.
  gzip -9 -n -c "$f" >"$f.gz"
  mv "$f.gz" "$f"
  gzipped=$((gzipped + 1))
done < <(find "$assets_dir" -type f -print0)

echo "gzip-embed-assets: gzipped $gzipped file(s) in $assets_dir"
