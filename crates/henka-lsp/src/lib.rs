//! A minimal asynchronous LSP/JSON-RPC client for driving language servers
//! (such as Eclipse JDT LS) over a child process's stdio.

pub mod client;
pub mod convert;
pub mod error;
pub mod framing;
pub mod session;

pub use client::LspClient;
pub use convert::{
    DEFAULT_SYMBOL_SEARCH_LIMIT, MAX_CONTEXT_LINES, Source, context_lines_param, goto_to_query,
    locations_to_query, symbols_to_query, to_core_workspace_edit, uri_to_path,
};
pub use error::{LspError, Result};
pub use session::{LspSession, path_to_file_uri};
