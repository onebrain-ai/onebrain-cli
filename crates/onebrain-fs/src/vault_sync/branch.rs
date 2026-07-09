//! Branch resolution from `vault.yml::update_channel`.
//!
//!   - `Some("next")` → `"next"` (opt-in pre-release channel, for future use)
//!   - anything else (including `None` and `Some("stable")`) → `"main"`
//!
//! `main` is the default because the `onebrain-ai/onebrain` plugin repo ships
//! only a `main` branch today — a `next` branch does not exist, so defaulting
//! absent/unknown channels to `next` would 404 the tarball fetch on
//! `plugin update` for any vault without an explicit `update_channel`.

/// The `update_channel` values the CLI understands — the single source of
/// truth for scaffold comments and doctor validation. Anything else resolves
/// to `main` at fetch time (see [`resolve_branch`]) but is flagged by
/// `onebrain doctor` as an invalid value.
pub const VALID_UPDATE_CHANNELS: &[&str] = &["stable", "next"];

/// The `update_channel` value `init` scaffolds and `doctor --fix` resets to.
pub const DEFAULT_UPDATE_CHANNEL: &str = "stable";

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
