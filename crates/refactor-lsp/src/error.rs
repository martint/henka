//! LSP client errors.

use thiserror::Error;

/// An error talking to a language server.
#[derive(Debug, Error)]
pub enum LspError {
    /// The child process could not be spawned or its pipes were unavailable.
    #[error("failed to start language server: {0}")]
    Spawn(String),

    /// Malformed protocol framing.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The server returned a JSON-RPC error response.
    #[error("language server error {code}: {message}")]
    Response {
        /// JSON-RPC error code.
        code: i64,
        /// Human-readable message.
        message: String,
    },

    /// The connection closed before a response arrived.
    #[error("language server connection closed")]
    Closed,

    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A (de)serialization error.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Result alias for the LSP client.
pub type Result<T> = std::result::Result<T, LspError>;
