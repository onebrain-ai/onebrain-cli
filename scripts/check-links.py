#!/usr/bin/env python3
"""Validate that doc links + reference-doc source paths resolve to real files.

Two checks (both must pass), run over the repo's markdown:

1. Every relative markdown link `[text](path)` resolves to an existing file/dir.
   External URLs, `mailto:`, and pure `#anchors` are skipped; an optional link
   title (`[t](path "title")`) and `<...>`-wrapped destinations are handled.
2. Every crate-relative `src/…rs` path mentioned in `docs/reference/onebrain-<crate>.md`
   exists in that crate (`src/main.rs` in `onebrain-cli.md` means
   `crates/onebrain-cli/src/main.rs`).

To avoid false positives that would block a legitimate PR, both checks ignore
content inside fenced code blocks (``` / ~~~), and the link check also ignores
inline `code` spans — a `[t](p)` written as a *markdown example* is not a live link.

Known gaps (rare/malformed input only): reference-style links `[t][ref]` aren't
followed and the reference-crate map below is hardcoded (false-negatives — never
block a PR); 4-space *indented* code blocks aren't recognized, and destinations
containing a literal `(`/`)` or an unclosed `<` aren't parsed (could false-flag
such a link — none exist in this repo). Fenced blocks of any fence length ARE
handled.

Exits non-zero and lists every failure if anything is missing. Self-contained (stdlib).
Run: `python3 scripts/check-links.py` from the repo root.
"""
import glob
import os
import re
import sys

MD_FILES = sorted(
    f
    for f in glob.glob("docs/**/*.md", recursive=True) + ["README.md", "CONTRIBUTING.md"]
    if os.path.isfile(f)
)

# Reference doc -> the crate whose `src/…` paths it describes. Hardcoded: add a
# row when a new crate gains a reference doc (see "Known gaps" above).
REFERENCE_CRATES = {
    "docs/reference/onebrain-cli.md": "crates/onebrain-cli",
    "docs/reference/onebrain-fs.md": "crates/onebrain-fs",
    "docs/reference/onebrain-core.md": "crates/onebrain-core",
    "docs/reference/onebrain-cache.md": "crates/onebrain-cache",
    "docs/reference/onebrain-search.md": "crates/onebrain-search",
}

LINK_RE = re.compile(r"\[[^\]]*\]\(([^)]+)\)")
SRC_RE = re.compile(r"`(src/[A-Za-z0-9_./\-]+\.rs)`")
# a run of N backticks, then the nearest matching run of N — stays on one line so
# a stray unmatched backtick can't swallow a real link on a later line.
INLINE_CODE_RE = re.compile(r"(`+)[^\n]*?\1")
SKIP_PREFIXES = ("http://", "https://", "mailto:", "tel:", "#")


def read(path):
    # errors="replace" so a stray non-UTF-8 byte can't crash CI with a traceback.
    with open(path, encoding="utf-8", errors="replace") as fh:
        return fh.read()


def without_fenced_blocks(text):
    """Drop lines inside ``` / ~~~ fenced code blocks (examples, not real refs).

    Tracks the opening fence's char and run length, so a longer outer fence
    (```` ````) is not closed early by a shorter inner one (``` ```) — the
    canonical way to *document* a fenced block. A closing fence is a line of
    only fence chars, at least as long as the opener.
    """
    out, fence_char, fence_len = [], None, 0
    for line in text.splitlines():
        s = line.lstrip()
        if fence_char is None:
            if s[:3] in ("```", "~~~"):
                fence_char = s[0]
                fence_len = len(s) - len(s.lstrip(fence_char))
                continue
            out.append(line)
        else:
            run = len(s) - len(s.lstrip(fence_char))
            if run >= fence_len and not s[run:].strip():
                fence_char, fence_len = None, 0
            # lines inside the fence (including the closer) are dropped
    return "\n".join(out)


def link_destination(raw):
    """Extract the path from a markdown link destination, dropping any title.

    `path` / `path "title"` / `path 'title'` -> `path`; `<a b>` -> `a b`.
    A bare destination can't contain a space unless <bracketed>, so the URL is
    the first whitespace-delimited token.
    """
    raw = raw.strip()
    if raw.startswith("<") and ">" in raw:
        return raw[1 : raw.index(">")]
    parts = raw.split()
    return parts[0] if parts else ""


def check_markdown_links():
    broken = []
    for f in MD_FILES:
        base = os.path.dirname(f)
        # ignore fenced blocks + inline code: a link shown as an example is literal.
        text = INLINE_CODE_RE.sub("", without_fenced_blocks(read(f)))
        for m in LINK_RE.finditer(text):
            dest = link_destination(m.group(1))
            if not dest or dest.startswith(SKIP_PREFIXES):
                continue
            path = dest.split("#", 1)[0].split("?", 1)[0]
            if not path:
                continue
            if not os.path.exists(os.path.normpath(os.path.join(base, path))):
                broken.append(f"{f}: link `{m.group(1).strip()}` -> missing")
    return broken


def check_reference_src_paths():
    broken = []
    for doc, crate in REFERENCE_CRATES.items():
        if not os.path.isfile(doc):
            broken.append(f"{doc}: reference doc is missing (expected for {crate})")
            continue
        # keep inline code (the `src/…rs` refs ARE inline code) but skip fences.
        for m in SRC_RE.finditer(without_fenced_blocks(read(doc))):
            rel = m.group(1)
            if not os.path.exists(os.path.join(crate, rel)):
                broken.append(f"{doc}: `{rel}` -> missing in {crate}/")
    return broken


def main():
    broken = check_markdown_links() + check_reference_src_paths()
    if broken:
        print(f"FAIL: {len(broken)} broken doc reference(s):", file=sys.stderr)
        for b in broken:
            print(f"  - {b}", file=sys.stderr)
        return 1
    print(f"OK: {len(MD_FILES)} markdown files scanned; all links + source paths resolve.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
