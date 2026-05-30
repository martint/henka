//! A minimal asynchronous LSP/JSON-RPC client for driving language servers
//! (such as Eclipse JDT LS) over a child process's stdio.

pub mod client;
pub mod convert;
pub mod error;
pub mod framing;
pub mod session;

pub use client::LspClient;
pub use convert::{locations_to_query, to_core_workspace_edit, uri_to_path};
pub use error::{LspError, Result};
pub use session::{LspSession, path_to_file_uri};
