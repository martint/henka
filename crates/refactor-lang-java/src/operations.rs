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
