//! The Rust operations: rename and find-usages, driven over LSP.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use henka_core::operation::{
    Operation, OperationCtx, OperationDescriptor, OperationKind, OperationOutcome,
    OperationRequest, Target, TargetKind,
};
use henka_core::{Error as CoreError, Language, Position, Result as CoreResult};
use serde_json::{Value, json};

use crate::analyzer::RaSession;

/// Downcast the operation's session to a rust-analyzer session.
fn ra<'a>(ctx: &'a OperationCtx<'_>) -> CoreResult<&'a RaSession> {
    ctx.session
        .as_any()
        .downcast_ref::<RaSession>()
        .ok_or_else(|| CoreError::Backend("expected a Rust (rust-analyzer) session".into()))
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
            languages: vec![Language::Rust],
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
        let session = ra(ctx)?;
        let (file, position) = position_target(req)?;
        let new_name = req
            .params
            .get("new_name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::InvalidTarget("`new_name` is required".into()))?;

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

        let edit = henka_lsp::to_core_workspace_edit(result).map_err(backend)?;
        Ok(OperationOutcome::Edit(edit))
    }
}

/// An operation backed by a rust-analyzer assist: request code actions for a
/// target, pick the one of a given kind (and, when several share a kind,
/// matching a title keyword), and use its inline or resolved edit.
///
/// rust-analyzer groups its refactors under broad kinds (`refactor.extract`,
/// `refactor.inline`) and distinguishes them by title, so a keyword selects the
/// specific assist (e.g. "variable" vs "function").
pub struct CodeActionOp {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    /// The LSP code action kind to request (e.g. `refactor.extract`).
    action_kind: &'static str,
    /// A lowercase keyword the chosen action's title must contain, to pick one
    /// assist when a kind covers several.
    title_keyword: Option<&'static str>,
    target: TargetKind,
}

impl CodeActionOp {
    const fn new(
        id: &'static str,
        title: &'static str,
        description: &'static str,
        action_kind: &'static str,
        title_keyword: Option<&'static str>,
        target: TargetKind,
    ) -> Self {
        Self {
            id,
            title,
            description,
            action_kind,
            title_keyword,
            target,
        }
    }

    /// The extract/inline refactorings contributed for Rust.
    pub fn rust_set() -> Vec<Arc<dyn Operation>> {
        vec![
            Arc::new(Self::new(
                "extract-variable",
                "Extract to variable",
                "Extract the selected expression into a new local variable",
                "refactor.extract",
                Some("variable"),
                TargetKind::Selection,
            )),
            Arc::new(Self::new(
                "extract-constant",
                "Extract to constant",
                "Extract the selected expression into a new constant",
                "refactor.extract",
                Some("constant"),
                TargetKind::Selection,
            )),
            Arc::new(Self::new(
                "extract-function",
                "Extract to function",
                "Extract the selected statements into a new function",
                "refactor.extract",
                Some("function"),
                TargetKind::Selection,
            )),
            Arc::new(Self::new(
                "inline",
                "Inline",
                "Inline the local variable at the position",
                "refactor.inline",
                None,
                TargetKind::Position,
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
            languages: vec![Language::Rust],
            target: self.target,
            params_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn run(
        &self,
        ctx: &OperationCtx<'_>,
        req: &OperationRequest,
    ) -> CoreResult<OperationOutcome> {
        let session = ra(ctx)?;
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

        // Choose the action of the requested kind whose title matches the
        // keyword (when set).
        let chosen = actions
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|a| a.get("kind").and_then(Value::as_str) == Some(self.action_kind))
            .find(|a| match self.title_keyword {
                Some(keyword) => a
                    .get("title")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.to_lowercase().contains(keyword)),
                None => true,
            })
            .cloned()
            .ok_or_else(|| {
                CoreError::OperationNotAvailable(format!(
                    "{} is not available at this location",
                    self.id
                ))
            })?;

        // The edit is usually resolved lazily; resolve the action if absent.
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

        let edit = henka_lsp::to_core_workspace_edit(edit_value).map_err(backend)?;
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
            languages: vec![Language::Rust],
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
        let session = ra(ctx)?;
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

        let usages = henka_lsp::locations_to_query(result, session.root()).map_err(backend)?;
        Ok(OperationOutcome::Query(usages))
    }
}
