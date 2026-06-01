//! Core, language-agnostic building blocks for the Henka server.
//!
//! This crate holds the multi-tenant [`ProjectRegistry`], the [`Language`]
//! model, and (in later phases) the pluggable refactoring framework and
//! workspace-edit application. It has no knowledge of MCP or of any particular
//! language backend.

pub mod edit;
pub mod error;
pub mod language;
pub mod operation;
pub mod project;
pub mod provider;
pub mod registry;
pub mod vcs;

pub use edit::{
    AppliedEdit, EditApplier, FileDiff, FileEdit, FileOperation, Position, PositionEncoding, Range,
    TextEdit, WorkspaceEdit,
};
pub use error::{Error, Result};
pub use language::{Language, detect_languages};
pub use operation::{
    Operation, OperationCtx, OperationDescriptor, OperationKind, OperationOutcome,
    OperationRegistry, OperationRequest, Target, TargetKind,
};
pub use project::{Project, validate_project_id};
pub use provider::{LanguageProvider, LanguageSession, ProviderRegistry, RequestGuard};
pub use registry::{ProjectRegistry, data_root, default_config_path};
pub use vcs::{RepoId, Revision, Vcs, detect_revision, repo_identity, working_copy_delta};
