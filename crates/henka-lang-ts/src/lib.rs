//! The TypeScript/JavaScript language provider: drives
//! typescript-language-server to give the server TS/JS semantics, and
//! contributes the TS/JS operations. One provider serves both languages.

pub mod error;
pub mod operations;
pub mod provider;
pub mod server;

pub use error::{Result, TsError};
pub use provider::{LANGUAGES, TsProvider};
pub use server::TsSession;
