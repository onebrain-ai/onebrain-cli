#!/usr/bin/env python3
"""Guard against the README Quickstart version going stale on release.

The Quickstart shows the expected `onebrain --version` output:

    onebrain --version
    # → onebrain X.Y.Z

That `X.Y.Z` must equal the workspace `version` in `Cargo.toml`
(`[workspace.package]`). It has drifted every release because the version-bump
PR forgets the README, so this check makes the drift a hard CI failure instead
of a recurring manual miss.

Anchored deliberately: the version is read from the `# → onebrain <ver>` line
that follows the literal `onebrain --version` command in the Quickstart — NOT
from any incidental `3.4.x` mentioned elsewhere in the README (milestone lists,
changelog prose), so unrelated version strings can't satisfy or trip the guard.

Exits non-zero with both values + the fix when they disagree. Self-contained
(stdlib). Run: `python3 scripts/check-readme-version.py` from the repo root.
"""
import re
import sys

CARGO_TOML = "Cargo.toml"
README = "README.md"

# The `→` is U+2192 (RIGHTWARDS ARROW), as written in the README example.
QUICKSTART_RE = re.compile(r"#\s*→\s*onebrain\s+(\d+\.\d+\.\d+)")
VERSION_CMD = "onebrain --version"


def read(path):
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def workspace_version(path=CARGO_TOML):
    """The `version = "X.Y.Z"` under `[workspace.package]` — scoped to that
    section so a `version` in `[workspace.dependencies]` can't be picked up."""
    text = read(path)
    m = re.search(r"\[workspace\.package\](.*?)(?:\n\[|\Z)", text, re.S)
    section = m.group(1) if m else text
    vm = re.search(r'^\s*version\s*=\s*"([^"]+)"', section, re.M)
    return vm.group(1) if vm else None


def readme_quickstart_version(path=README):
    """The `X.Y.Z` from the `# → onebrain X.Y.Z` line that follows the literal
    `onebrain --version` command in the Quickstart. Returns None when absent."""
    lines = read(path).splitlines()
    for i, line in enumerate(lines):
        if line.strip() == VERSION_CMD:
            for nxt in lines[i + 1 : i + 4]:
                m = QUICKSTART_RE.search(nxt)
                if m:
                    return m.group(1)
    return None


def main():
    cargo = workspace_version()
    if cargo is None:
        print(
            "FAIL: could not read `version` from [workspace.package] in Cargo.toml",
            file=sys.stderr,
        )
        return 1

    readme = readme_quickstart_version()
    if readme is None:
        print(
            "FAIL: could not find the README Quickstart `# → onebrain <ver>` example "
            f"(expected on a line right after `{VERSION_CMD}`)",
            file=sys.stderr,
        )
        return 1

    if cargo != readme:
        print("FAIL: README Quickstart version is stale.", file=sys.stderr)
        print(f"  Cargo.toml [workspace.package] version = {cargo}", file=sys.stderr)
        print(f"  README.md  Quickstart '# → onebrain {readme}'", file=sys.stderr)
        print(
            f"  Fix: set the README Quickstart line to '# → onebrain {cargo}'.",
            file=sys.stderr,
        )
        return 1

    print(f"OK: README Quickstart '# → onebrain {readme}' matches Cargo.toml {cargo}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
