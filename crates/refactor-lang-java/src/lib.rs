//! The Java language provider: drives Eclipse JDT LS (jdtls) to give the server
//! Java semantics, and (in later phases) contributes Java operations.

pub mod error;
pub mod jdtls;
pub mod provider;

pub use error::{JavaError, Result};
pub use jdtls::{JdtlsInstall, JdtlsSession};
pub use provider::JavaProvider;
