//! Error type shared across the core.

use std::path::PathBuf;

use thiserror::Error;

/// Errors produced by the refactoring core.
#[derive(Debug, Error)]
pub enum Error {
    /// No project is registered under the given id.
    #[error("project not found: `{0}`")]
    ProjectNotFound(String),

    /// A project is already registered under the given id.
    #[error("project already registered: `{0}`")]
    ProjectAlreadyExists(String),

    /// A project id is not a valid slug.
    #[error(
        "invalid project id `{0}`: use lowercase letters, digits and dashes (e.g. `my-service`)"
    )]
    InvalidProjectId(String),

    /// The given root path does not exist.
    #[error("path does not exist: {0}")]
    PathNotFound(PathBuf),

    /// The given root path is not a directory.
    #[error("path is not a directory: {0}")]
    NotADirectory(PathBuf),

    /// No supported language could be detected under the root.
    #[error("no supported language detected under {0}")]
    NoLanguageDetected(PathBuf),

    /// The persisted registry file could not be parsed.
    #[error("could not read registry config at {path}: {source}")]
    ConfigRead {
        /// Path of the config file.
        path: PathBuf,
        /// Underlying parse error.
        source: toml::de::Error,
    },

    /// The registry file could not be serialized.
    #[error("could not serialize registry config: {0}")]
    ConfigWrite(#[from] toml::ser::Error),

    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias for the core.
pub type Result<T> = std::result::Result<T, Error>;
