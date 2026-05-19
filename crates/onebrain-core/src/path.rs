use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRoot(PathBuf);

impl VaultRoot {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, child: impl AsRef<Path>) -> PathBuf {
        self.0.join(child)
    }
}

/// Walk up from `start` looking for the nearest directory containing a
/// `vault.yml`. Returns `None` if none is found before the filesystem root.
pub fn find_vault_root(start: &Path) -> Option<VaultRoot> {
    let mut current = start.to_path_buf();
    loop {
        if current.join("vault.yml").is_file() {
            return Some(VaultRoot(current));
        }
        if !current.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn finds_vault_in_starting_dir() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "").unwrap();
        let result = find_vault_root(dir.path());
        assert_eq!(result.unwrap().as_path(), dir.path());
    }

    #[test]
    fn walks_up_from_subdirectory() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("vault.yml"), "").unwrap();
        let sub = dir.path().join("00-inbox").join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        let result = find_vault_root(&sub);
        assert_eq!(result.unwrap().as_path(), dir.path());
    }

    #[test]
    fn returns_none_when_no_vault_found() {
        let dir = tempdir().unwrap();
        assert!(find_vault_root(dir.path()).is_none());
    }
}
