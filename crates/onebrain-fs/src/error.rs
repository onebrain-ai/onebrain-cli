use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FsError {
    #[error(transparent)]
    Core(#[from] onebrain_core::CoreError),

    #[error("vault walk failed at {path}")]
    WalkFailed {
        path: PathBuf,
        #[source]
        source: walkdir::Error,
    },
}

pub type Result<T> = std::result::Result<T, FsError>;
