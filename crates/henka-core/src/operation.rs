//! The pluggable operation model.
//!
//! An [`Operation`] is a single named action on code — a refactoring, a
//! structural replace, or a semantic query. Operations are contributed per
//! language by a [`LanguageProvider`](crate::provider::LanguageProvider) and
//! collected into an [`OperationRegistry`]. They are *not* methods on a fixed
//! interface: adding an operation is adding a plugin.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::edit::{Position, Range, WorkspaceEdit};
use crate::error::{Error, Result};
use crate::language::Language;
use crate::project::Project;
use crate::provider::LanguageSession;

/// Whether an operation edits code or only reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    /// Produces a [`WorkspaceEdit`]; supports preview (`dry_run`).
    Edit,
    /// Produces a structured, read-only result.
    Query,
}

/// The kind of location an operation acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    /// A single point in a file (e.g. an identifier).
    Position,
    /// A range in a file (e.g. an expression or statements).
    Selection,
    /// A whole file.
    File,
    /// The project as a whole, with no specific location.
    Project,
}

/// A resolved target: where an operation should act.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Target {
    /// A point in a file.
    Position {
        /// File path (relative to project root unless absolute).
        file: PathBuf,
        /// The point.
        position: Position,
    },
    /// A range in a file.
    Selection {
        /// File path (relative to project root unless absolute).
        file: PathBuf,
        /// The range.
        range: Range,
    },
    /// A whole file.
    File {
        /// File path (relative to project root unless absolute).
        file: PathBuf,
    },
    /// The whole project.
    Project,
}

impl Target {
    /// The file this target refers to, if any.
    pub fn file(&self) -> Option<&PathBuf> {
        match self {
            Target::Position { file, .. }
            | Target::Selection { file, .. }
            | Target::File { file } => Some(file),
            Target::Project => None,
        }
    }
}

/// What an operation declares about itself: identity, applicability, and the
/// shape of its inputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationDescriptor {
    /// Stable identifier, also the MCP tool name (e.g. `rename`, `find-usages`).
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// One-line description of what the operation does.
    pub description: String,
    /// Whether it edits or only reads.
    pub kind: OperationKind,
    /// Languages the operation applies to.
    pub languages: Vec<Language>,
    /// The target shape it expects.
    pub target: TargetKind,
    /// JSON Schema (an object schema) for the operation-specific parameters,
    /// beyond the common target/`dry_run` envelope.
    pub params_schema: Value,
}

impl OperationDescriptor {
    /// Whether the operation applies to the given language.
    pub fn applies_to(&self, language: Language) -> bool {
        self.languages.contains(&language)
    }
}

/// The result of running an operation.
#[derive(Debug, Clone)]
pub enum OperationOutcome {
    /// An edit to be previewed or applied.
    Edit(WorkspaceEdit),
    /// A structured, read-only result.
    Query(Value),
}

/// A request to run an operation: where to act, and with what parameters.
#[derive(Debug, Clone)]
pub struct OperationRequest {
    /// The resolved target.
    pub target: Target,
    /// Operation-specific parameters (validated against `params_schema`).
    pub params: Value,
}

/// Everything an operation needs to run: the project and its analysis session.
pub struct OperationCtx<'a> {
    /// The project being operated on.
    pub project: &'a Project,
    /// The language session that provides semantic services.
    pub session: Arc<dyn LanguageSession>,
}

/// A single named action on code, contributed by a language provider.
#[async_trait]
pub trait Operation: Send + Sync {
    /// Describe this operation (identity, kind, target, parameter schema).
    fn descriptor(&self) -> OperationDescriptor;

    /// Run the operation against `ctx` with `req`.
    async fn run(&self, ctx: &OperationCtx<'_>, req: &OperationRequest)
    -> Result<OperationOutcome>;
}

/// A registered operation paired with its (cached) descriptor.
struct Registered {
    descriptor: OperationDescriptor,
    operation: Arc<dyn Operation>,
}

/// The catalog of operations available across all registered providers.
#[derive(Default)]
pub struct OperationRegistry {
    operations: Vec<Registered>,
}

impl OperationRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add operations (typically a provider's contributions).
    pub fn extend(&mut self, ops: impl IntoIterator<Item = Arc<dyn Operation>>) {
        for operation in ops {
            let descriptor = operation.descriptor();
            self.operations.push(Registered {
                descriptor,
                operation,
            });
        }
    }

    /// All operation descriptors, ordered by id.
    pub fn descriptors(&self) -> Vec<OperationDescriptor> {
        let mut out: Vec<OperationDescriptor> = self
            .operations
            .iter()
            .map(|r| r.descriptor.clone())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }

    /// Descriptors applicable to a project (any of its languages), by id.
    pub fn descriptors_for(&self, project: &Project) -> Vec<OperationDescriptor> {
        let mut out: Vec<OperationDescriptor> = self
            .operations
            .iter()
            .filter(|r| {
                project
                    .languages
                    .iter()
                    .any(|&l| r.descriptor.applies_to(l))
            })
            .map(|r| r.descriptor.clone())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }

    /// Resolve the operation with `id` applicable to one of `languages`.
    ///
    /// Fails if no registered operation matches, which is reported to clients
    /// as the operation being unavailable for the project.
    pub fn resolve(&self, id: &str, languages: &[Language]) -> Result<Arc<dyn Operation>> {
        self.operations
            .iter()
            .find(|r| {
                r.descriptor.id == id && languages.iter().any(|&l| r.descriptor.applies_to(l))
            })
            .map(|r| Arc::clone(&r.operation))
            .ok_or_else(|| Error::OperationNotAvailable(id.to_string()))
    }
}
