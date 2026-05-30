//! Errors from the Java language provider.

use std::path::PathBuf;

use thiserror::Error;

/// An error starting or driving the Java backend.
#[derive(Debug, Error)]
pub enum JavaError {
    /// No jdtls distribution could be located.
    #[error(
        "could not locate a jdtls distribution (looked at: {0}); set JDTLS_HOME or run scripts/fetch-jdtls.sh"
    )]
    JdtlsNotFound(String),

    /// A jdtls install was found but is missing expected files.
    #[error("jdtls install at {0} is incomplete: {1}")]
    JdtlsIncomplete(PathBuf, String),

    /// No Java runtime could be found.
    #[error("no Java runtime found; set JAVA_HOME or put `java` on PATH")]
    JavaNotFound,

    /// The language server protocol failed.
    #[error(transparent)]
    Lsp(#[from] refactor_lsp::LspError),

    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias for the Java provider.
pub type Result<T> = std::result::Result<T, JavaError>;
