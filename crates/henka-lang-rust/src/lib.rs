//! The Rust language provider: drives rust-analyzer to give the server Rust
//! semantics, and contributes the Rust operations.

pub mod analyzer;
pub mod error;
pub mod operations;
pub mod provider;

pub use analyzer::RaSession;
pub use error::{Result, RustError};
pub use provider::RustProvider;
