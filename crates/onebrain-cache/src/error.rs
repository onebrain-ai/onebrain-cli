use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("cache I/O error at {path}")]
    CacheIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Core(#[from] onebrain_core::CoreError),
}

pub type Result<T> = std::result::Result<T, CacheError>;
