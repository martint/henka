//! The Java operations: rename and find-usages, driven over LSP.

use std::path::PathBuf;

use async_trait::async_trait;
use refactor_core::operation::{
    Operation, OperationCtx, OperationDescriptor, OperationKind, OperationOutcome,
    OperationRequest, Target, TargetKind,
};
use refactor_core::{Error as CoreError, Language, Position, Result as CoreResult};
use serde_json::{Value, json};

use crate::jdtls::JdtlsSession;
use crate::lsp;

/// Downcast the operation's session to a jdtls session.
fn jdtls<'a>(ctx: &'a OperationCtx<'_>) -> CoreResult<&'a JdtlsSession> {
    ctx.session
        .as_any()
        .downcast_ref::<JdtlsSession>()
        .ok_or_else(|| CoreError::Backend("expected a Java (jdtls) session".into()))
}

/// Extract a position target.
fn position_target(req: &OperationRequest) -> CoreResult<(&PathBuf, Position)> {
    match &req.target {
        Target::Position { file, position } => Ok((file, *position)),
        _ => Err(CoreError::InvalidTarget(
            "this operation expects a position (file, line, character)".into(),
        )),
    }
}

/// Map a backend error into the core error type.
fn backend(e: impl std::fmt::Display) -> CoreError {
    CoreError::Backend(e.to_string())
}

/// Rename the symbol at a position and update all references.
pub struct RenameOp;

#[async_trait]
impl Operation for RenameOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: "rename".into(),
            title: "Rename symbol".into(),
            description: "Rename the symbol at the given position and update every reference"
                .into(),
            kind: OperationKind::Edit,
            languages: vec![Language::Java],
            target: TargetKind::Position,
            params_schema: json!({
                "type": "object",
                "properties": {
                    "new_name": { "type": "string", "description": "The new name for the symbol." }
                },
                "required": ["new_name"],
            }),
        }
    }

    async fn run(
        &self,
        ctx: &OperationCtx<'_>,
        req: &OperationRequest,
    ) -> CoreResult<OperationOutcome> {
        let session = jdtls(ctx)?;
        let (file, position) = position_target(req)?;
        let new_name = req
            .params
            .get("new_name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::InvalidTarget("`new_name` is required".into()))?;

        // Ensure the project is resolved so the rename covers every usage.
        session.ensure_indexed().await.map_err(backend)?;
        let uri = session.ensure_open(file).await.map_err(backend)?;

        let result: Value = session
            .client()
            .request(
                "textDocument/rename",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.line, "character": position.character },
                    "newName": new_name,
                }),
            )
            .await
            .map_err(backend)?;

        let edit = lsp::to_core_workspace_edit(result).map_err(backend)?;
        Ok(OperationOutcome::Edit(edit))
    }
}

/// An operation backed by a jdtls code action: request the code actions for a
/// target, pick the one of a given kind, and use its (inline or resolved) edit.
///
/// This covers the extract refactorings, inline, and organize-imports. Without
/// the "advanced" client capabilities, jdtls computes these itself and returns
/// the edit on the action (using a default name for extracted symbols).
pub struct CodeActionOp {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    /// The LSP code action kind to select (e.g. `refactor.extract.variable`).
    action_kind: &'static str,
    target: TargetKind,
}

impl CodeActionOp {
    const fn new(
        id: &'static str,
        title: &'static str,
        description: &'static str,
        action_kind: &'static str,
        target: TargetKind,
    ) -> Self {
        Self {
            id,
            title,
            description,
            action_kind,
            target,
        }
    }

    /// The extract refactorings and friends contributed for Java.
    pub fn java_set() -> Vec<std::sync::Arc<dyn Operation>> {
        vec![
            std::sync::Arc::new(Self::new(
                "extract-variable",
                "Extract to local variable",
                "Extract the selected expression into a new local variable",
                "refactor.extract.variable",
                TargetKind::Selection,
            )),
            std::sync::Arc::new(Self::new(
                "extract-constant",
                "Extract to constant",
                "Extract the selected expression into a new constant",
                "refactor.extract.constant",
                TargetKind::Selection,
            )),
            std::sync::Arc::new(Self::new(
                "extract-field",
                "Extract to field",
                "Extract the selected expression into a new field",
                "refactor.extract.field",
                TargetKind::Selection,
            )),
            std::sync::Arc::new(Self::new(
                "extract-method",
                "Extract to method",
                "Extract the selected statements into a new method",
                "refactor.extract.function",
                TargetKind::Selection,
            )),
            std::sync::Arc::new(Self::new(
                "inline",
                "Inline",
                "Inline the local variable or constant at the position",
                "refactor.inline",
                TargetKind::Position,
            )),
            std::sync::Arc::new(Self::new(
                "organize-imports",
                "Organize imports",
                "Sort and prune the file's imports",
                "source.organizeImports",
                TargetKind::File,
            )),
        ]
    }

    /// The codeAction range for a request, derived from its target.
    fn range(&self, req: &OperationRequest) -> CoreResult<Value> {
        let point = |p: Position| json!({ "line": p.line, "character": p.character });
        match &req.target {
            Target::Selection { range, .. } => {
                Ok(json!({ "start": point(range.start), "end": point(range.end) }))
            }
            Target::Position { position, .. } => {
                Ok(json!({ "start": point(*position), "end": point(*position) }))
            }
            Target::File { .. } => Ok(json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 0 }
            })),
            Target::Project => Err(CoreError::InvalidTarget(
                "this operation needs a file, selection, or position".into(),
            )),
        }
    }
}

#[async_trait]
impl Operation for CodeActionOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: self.id.into(),
            title: self.title.into(),
            description: self.description.into(),
            kind: OperationKind::Edit,
            languages: vec![Language::Java],
            target: self.target,
            params_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn run(
        &self,
        ctx: &OperationCtx<'_>,
        req: &OperationRequest,
    ) -> CoreResult<OperationOutcome> {
        let session = jdtls(ctx)?;
        let file = req
            .target
            .file()
            .ok_or_else(|| CoreError::InvalidTarget("a file is required".into()))?;
        let range = self.range(req)?;

        session.ensure_indexed().await.map_err(backend)?;
        let uri = session.ensure_open(file).await.map_err(backend)?;

        let actions: Value = session
            .client()
            .request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": uri },
                    "range": range,
                    "context": { "diagnostics": [], "only": [self.action_kind] },
                }),
            )
            .await
            .map_err(backend)?;

        // Choose the action of the requested kind, preferring a single-target
        // variant over a "replace all occurrences" one.
        let chosen = actions
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|a| a.get("kind").and_then(Value::as_str) == Some(self.action_kind))
            .min_by_key(|a| {
                let multi = a
                    .get("title")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.contains("occurrence"));
                u8::from(multi)
            })
            .cloned()
            .ok_or_else(|| {
                CoreError::OperationNotAvailable(format!(
                    "{} is not available at this location",
                    self.id
                ))
            })?;

        // The edit is usually inline; otherwise resolve the action for it.
        let edit_value = match chosen.get("edit") {
            Some(edit) => edit.clone(),
            None => {
                let resolved: Value = session
                    .client()
                    .request("codeAction/resolve", chosen)
                    .await
                    .map_err(backend)?;
                resolved.get("edit").cloned().unwrap_or(Value::Null)
            }
        };

        let edit = lsp::to_core_workspace_edit(edit_value).map_err(backend)?;
        Ok(OperationOutcome::Edit(edit))
    }
}

/// Find every reference to the symbol at a position.
pub struct FindUsagesOp;

#[async_trait]
impl Operation for FindUsagesOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: "find-usages".into(),
            title: "Find usages".into(),
            description: "Find every reference to the symbol at the given position".into(),
            kind: OperationKind::Query,
            languages: vec![Language::Java],
            target: TargetKind::Position,
            params_schema: json!({
                "type": "object",
                "properties": {
                    "include_declaration": {
                        "type": "boolean",
                        "default": true,
                        "description": "Whether to include the symbol's own declaration."
                    }
                }
            }),
        }
    }

    async fn run(
        &self,
        ctx: &OperationCtx<'_>,
        req: &OperationRequest,
    ) -> CoreResult<OperationOutcome> {
        let session = jdtls(ctx)?;
        let (file, position) = position_target(req)?;
        let include_declaration = req
            .params
            .get("include_declaration")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        session.ensure_indexed().await.map_err(backend)?;
        let uri = session.ensure_open(file).await.map_err(backend)?;
        let result: Value = session
            .client()
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.line, "character": position.character },
                    "context": { "includeDeclaration": include_declaration },
                }),
            )
            .await
            .map_err(backend)?;

        let usages = lsp::locations_to_query(result, session.root()).map_err(backend)?;
        Ok(OperationOutcome::Query(usages))
    }
}
