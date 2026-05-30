//! A minimal asynchronous LSP/JSON-RPC client for driving language servers
//! (such as Eclipse JDT LS) over a child process's stdio.

pub mod client;
pub mod error;
pub mod framing;

pub use client::LspClient;
pub use error::{LspError, Result};
