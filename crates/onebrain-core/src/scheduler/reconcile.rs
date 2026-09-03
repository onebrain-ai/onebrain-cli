//! Ownership-based reconciliation of installed scheduler artifacts (#410, #352).
//!
//! Labels (`com.onebrain.<label>`) are a GLOBAL namespace with no vault
//! component, so "not in onebrain.yml" is not evidence that an artifact is
//! ours to delete — the v3.4.21 ledger deleted another vault's live job on
//! exactly that inference and was reverted. Everything here decides from the
//! ARTIFACT: each renderer writes `ONEBRAIN_VAULT=<vault>` into what it
//! installs, the parsers below read it (or the legacy `--vault` argv) back,
//! and [`plan_reconcile`] only ever proposes deleting an artifact whose owner
//! is provably the current vault. A parse that proves nothing yields
//! [`Ownership::Unknown`], and Unknown is never deleted.

use std::path::{Component, Path, PathBuf};

/// Lexical path normalization (no symlink resolution, no disk touch):
/// drops `.` components and pops on `..`. Equivalent to Node's
/// `path.resolve` after the base is applied. Moved here from
/// `register_schedule.rs` so the planner and the CLI agree on one rule.
pub fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_strips_curdir_and_pops_parentdir() {
        assert_eq!(
            normalize_path(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn normalize_path_parent_dir_pops_prefix() {
        assert_eq!(normalize_path(Path::new("/a/b/..")), PathBuf::from("/a"));
    }

    #[test]
    fn normalize_path_plain_absolute_unchanged() {
        assert_eq!(normalize_path(Path::new("/x/y")), PathBuf::from("/x/y"));
    }
}
