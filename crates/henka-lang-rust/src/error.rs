//! Errors from the Rust language provider.

use thiserror::Error;

/// An error starting or driving the Rust backend.
#[derive(Debug, Error)]
pub enum RustError {
    /// No rust-analyzer binary could be located.
    #[error(
        "could not locate rust-analyzer (looked at: {0}); set HENKA_RUST_ANALYZER, put it on PATH, or run `cargo xtask rust-analyzer`"
    )]
    RustAnalyzerNotFound(String),

    /// The language server protocol failed.
    #[error(transparent)]
    Lsp(#[from] henka_lsp::LspError),

    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias for the Rust provider.
pub type Result<T> = std::result::Result<T, RustError>;
