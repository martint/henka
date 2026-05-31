//! The TypeScript/JavaScript operations, driven over LSP.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use henka_core::operation::{
    Operation, OperationCtx, OperationDescriptor, OperationKind, OperationOutcome,
    OperationRequest, Target, TargetKind,
};
use henka_core::{Error as CoreError, Language, Position, Result as CoreResult};
use serde_json::{Value, json};

use crate::server::TsSession;

/// The languages this backend's operations apply to.
fn languages() -> Vec<Language> {
    vec![Language::TypeScript, Language::JavaScript]
}

/// Downcast the operation's session to a typescript-language-server session.
fn ts<'a>(ctx: &'a OperationCtx<'_>) -> CoreResult<&'a TsSession> {
    ctx.session
        .as_any()
        .downcast_ref::<TsSession>()
        .ok_or_else(|| CoreError::Backend("expected a TypeScript/JavaScript session".into()))
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
            languages: languages(),
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
        let session = ts(ctx)?;
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
            languages: languages(),
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
        let session = ts(ctx)?;
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

/// An operation backed by a typescript-language-server code action: request the
/// code actions for a target, pick the one of a given kind, and use its inline
/// or resolved edit. Covers the extract refactorings and organize-imports.
pub struct CodeActionOp {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    /// The LSP code action kind to select (e.g. `refactor.extract.function`).
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

    /// The code-action operations contributed for TypeScript/JavaScript.
    pub fn ts_set() -> Vec<Arc<dyn Operation>> {
        vec![
            Arc::new(Self::new(
                "extract-function",
                "Extract to function",
                "Extract the selection into a new function",
                "refactor.extract.function",
                TargetKind::Selection,
            )),
            Arc::new(Self::new(
                "extract-constant",
                "Extract to constant",
                "Extract the selected expression into a new constant",
                "refactor.extract.constant",
                TargetKind::Selection,
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
            languages: languages(),
            target: self.target,
            params_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn run(
        &self,
        ctx: &OperationCtx<'_>,
        req: &OperationRequest,
    ) -> CoreResult<OperationOutcome> {
        let session = ts(ctx)?;
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

        // Choose the action whose kind matches (exactly, or as a more specific
        // sub-kind of the requested one).
        let chosen = actions
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .find(|a| {
                a.get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|k| k == self.action_kind || k.starts_with(self.action_kind))
            })
            .cloned()
            .ok_or_else(|| {
                CoreError::OperationNotAvailable(format!(
                    "{} is not available at this location",
                    self.id
                ))
            })?;

        // Resolve the action if it carries neither an inline edit nor a command.
        let chosen = if chosen.get("edit").is_some() || chosen.get("command").is_some() {
            chosen
        } else {
            session
                .client()
                .request("codeAction/resolve", chosen)
                .await
                .map_err(backend)?
        };

        let edit = if let Some(edit) = chosen.get("edit").filter(|e| !e.is_null()) {
            henka_lsp::to_core_workspace_edit(edit.clone()).map_err(backend)?
        } else if let Some(command) = chosen.get("command") {
            // typescript-language-server returns refactors as a command that,
            // when executed, sends the edit back via `workspace/applyEdit`.
            execute_capturing_edit(session, command).await?
        } else {
            henka_core::WorkspaceEdit::empty()
        };
        Ok(OperationOutcome::Edit(edit))
    }
}

/// Execute a server `command` (e.g. `_typescript.applyRefactoring`) and capture
/// the edit the server pushes back via a `workspace/applyEdit` request, rather
/// than letting it apply to disk.
async fn execute_capturing_edit(
    session: &TsSession,
    command: &Value,
) -> CoreResult<henka_core::WorkspaceEdit> {
    let name = command
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Backend("code action command has no name".into()))?;
    let arguments = command.get("arguments").cloned().unwrap_or(json!([]));

    // Subscribe before executing so the applyEdit request isn't missed.
    let mut events = session.client().subscribe();
    let _: Value = session
        .client()
        .request(
            "workspace/executeCommand",
            json!({ "command": name, "arguments": arguments }),
        )
        .await
        .map_err(backend)?;

    // The applyEdit request was broadcast while the command ran; drain for it.
    let deadline = tokio::time::Duration::from_secs(5);
    let captured = tokio::time::timeout(deadline, async {
        loop {
            match events.recv().await {
                Ok((method, params)) if method == "workspace/applyEdit" => {
                    return params.get("edit").cloned();
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten();

    match captured {
        Some(edit) => henka_lsp::to_core_workspace_edit(edit).map_err(backend),
        None => Ok(henka_core::WorkspaceEdit::empty()),
    }
}
