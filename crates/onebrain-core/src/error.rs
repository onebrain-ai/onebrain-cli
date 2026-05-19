use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CoreError {
    #[error("vault.yml not found at {path}")]
    VaultYamlMissing {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("vault.yml has invalid syntax")]
    InvalidYaml(#[from] serde_yaml::Error),

    #[error("path is not a valid vault root: {path}")]
    NotAVault { path: PathBuf },
}

pub type Result<T> = std::result::Result<T, CoreError>;
