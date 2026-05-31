//! Errors from the TypeScript/JavaScript language provider.

use thiserror::Error;

/// An error starting or driving the TypeScript/JavaScript backend.
#[derive(Debug, Error)]
pub enum TsError {
    /// No typescript-language-server could be located.
    #[error(
        "could not locate typescript-language-server (looked at: {0}); set HENKA_TYPESCRIPT_LANGUAGE_SERVER, put it on PATH, or run `cargo xtask typescript`"
    )]
    ServerNotFound(String),

    /// The language server protocol failed.
    #[error(transparent)]
    Lsp(#[from] henka_lsp::LspError),

    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias for the TypeScript/JavaScript provider.
pub type Result<T> = std::result::Result<T, TsError>;
