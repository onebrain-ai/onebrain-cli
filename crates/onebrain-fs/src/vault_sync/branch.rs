//! Branch resolution from `vault.yml::update_channel`.
//!
//! Port of Bun's `resolveBranch` (vault-sync.ts §"Branch resolution"):
//!   - `Some("stable")` → `"main"`
//!   - anything else (including `None`)  → `"next"`

/// Resolve the upstream branch to fetch the tarball from.
///
/// `update_channel === 'stable'` ⇒ `main`; any other channel (or absence) ⇒ `next`.
/// Matches the Bun source character-for-character.
pub fn resolve_branch(update_channel: Option<&str>) -> &'static str {
    match update_channel {
        Some("stable") => "main",
        _ => "next",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_resolves_to_main() {
        assert_eq!(resolve_branch(Some("stable")), "main");
    }

    #[test]
    fn none_resolves_to_next() {
        assert_eq!(resolve_branch(None), "next");
    }

    #[test]
    fn next_resolves_to_next() {
        assert_eq!(resolve_branch(Some("next")), "next");
    }

    #[test]
    fn unknown_channel_resolves_to_next() {
        assert_eq!(resolve_branch(Some("beta")), "next");
        assert_eq!(resolve_branch(Some("")), "next");
    }
}
