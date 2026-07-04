//! Branch resolution from `vault.yml::update_channel`.
//!
//!   - `Some("next")` → `"next"` (opt-in pre-release channel, for future use)
//!   - anything else (including `None` and `Some("stable")`) → `"main"`
//!
//! `main` is the default because the `onebrain-ai/onebrain` plugin repo ships
//! only a `main` branch today — a `next` branch does not exist, so defaulting
//! absent/unknown channels to `next` would 404 the tarball fetch on
//! `plugin update` for any vault without an explicit `update_channel`.

/// Resolve the upstream branch to fetch the tarball from.
///
/// `update_channel === 'next'` ⇒ `next`; any other channel (or absence) ⇒
/// `main` (the only branch that currently exists upstream).
pub fn resolve_branch(update_channel: Option<&str>) -> &'static str {
    match update_channel {
        Some("next") => "next",
        _ => "main",
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
    fn none_resolves_to_main() {
        // Absent channel must default to `main` — `next` does not exist
        // upstream, so defaulting there would 404 the tarball fetch.
        assert_eq!(resolve_branch(None), "main");
    }

    #[test]
    fn next_resolves_to_next() {
        assert_eq!(resolve_branch(Some("next")), "next");
    }

    #[test]
    fn unknown_channel_resolves_to_main() {
        assert_eq!(resolve_branch(Some("beta")), "main");
        assert_eq!(resolve_branch(Some("")), "main");
    }
}
