use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("cache directory unreadable at {path}")]
    CacheDirIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("session token resolution failed · all fallbacks exhausted")]
    TokenResolutionExhausted,

    #[error(transparent)]
    Core(#[from] onebrain_core::CoreError),
}

pub type Result<T> = std::result::Result<T, CacheError>;
