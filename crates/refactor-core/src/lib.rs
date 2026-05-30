//! Core, language-agnostic building blocks for the Refactor server.
//!
//! This crate holds the multi-tenant [`ProjectRegistry`], the [`Language`]
//! model, and (in later phases) the pluggable refactoring framework and
//! workspace-edit application. It has no knowledge of MCP or of any particular
//! language backend.

pub mod error;
pub mod language;
pub mod project;
pub mod registry;

pub use error::{Error, Result};
pub use language::{Language, detect_languages};
pub use project::{Project, validate_project_id};
pub use registry::{ProjectRegistry, default_config_path};
