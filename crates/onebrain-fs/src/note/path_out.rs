//! Forward-slash path rendering for `note`-verb OUTPUT.
//!
//! Every `note` verb emits vault-relative paths in its result struct (JSON +
//! text). A raw [`PathBuf`](std::path::PathBuf) serializes/displays with the
//! OS-native separator — `/` on unix, `\` on Windows. That is WRONG for a
//! vault tool: Obsidian uses `/` on every platform, wikilinks use `/`, and JSON
//! consumers expect a stable `/`. These helpers render paths with `/` on ALL
//! platforms by joining the path COMPONENTS with `/` (rather than string-
//! replacing `\`), which is correct by construction on unix and Windows alike.
//!
//! INTERNAL path handling (filesystem ops, walking, `strip_prefix`) stays on
//! [`Path`](std::path::Path)/`PathBuf`; only the values that get serialized or
//! displayed pass through here.

use std::path::Path;

/// Render a path as a forward-slash string for output, OS-independent.
/// Obsidian + JSON consumers use `/` on every platform.
pub(super) fn to_slash(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Strip the vault root and render forward-slash (for walk-derived abs paths).
pub(super) fn rel_slash(vault_root: &Path, abs: &Path) -> String {
    to_slash(abs.strip_prefix(vault_root).unwrap_or(abs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn to_slash_keeps_forward_slash_input() {
        // On unix the input parses into `/`-separated components; the join must
        // round-trip it unchanged.
        assert_eq!(to_slash(Path::new("a/b/c.md")), "a/b/c.md");
    }

    #[test]
    fn to_slash_single_component() {
        assert_eq!(to_slash(Path::new("note.md")), "note.md");
    }

    #[test]
    fn to_slash_joins_components_with_forward_slash() {
        // Build the path component-by-component (no separator literal), so the
        // output separator is decided solely by our `/`-join — proving the
        // result is `/` regardless of `MAIN_SEPARATOR`. On Windows the same
        // component join yields `03-knowledge/alpha.md`, never `\`.
        let p: PathBuf = ["03-knowledge", "ml", "alpha.md"].iter().collect();
        assert_eq!(to_slash(&p), "03-knowledge/ml/alpha.md");
    }

    #[test]
    fn rel_slash_strips_vault_root() {
        let root = Path::new("/vault");
        let abs = Path::new("/vault/03-knowledge/alpha.md");
        assert_eq!(rel_slash(root, abs), "03-knowledge/alpha.md");
    }
}
