//! Source-preserving image reduction with verified transactional replacement.

pub mod cli;
pub mod discovery;
pub mod inspection;
pub mod plan;
pub mod processing;
pub mod runner;

use std::path::PathBuf;

use thiserror::Error;

/// Run-wide argument, filesystem, and JSON-report errors with their available causes.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    InvalidArgument(String),
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot serialize JSON report: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result type for run-wide operations.
pub type Result<T> = std::result::Result<T, Error>;
