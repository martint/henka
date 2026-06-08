//! The Java operations: rename and find-usages, driven over LSP.

use std::path::PathBuf;

use async_trait::async_trait;
use henka_core::operation::{
    Operation, OperationCtx, OperationDescriptor, OperationKind, OperationOutcome,
    OperationRequest, Target, TargetKind,
};
use henka_core::{Error as CoreError, Language, Position, Result as CoreResult};
use serde_json::{Value, json};

use henka_lsp::convert as lsp;

use crate::jdtls::JdtlsSession;

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
    /// A substring the chosen action's title must contain (case-insensitive).
    /// Disambiguates when several distinct refactorings share one kind — e.g.
    /// `refactor.inline` hosts both "Inline Method" and "Make Static", and only
    /// the former is what `inline` means.
    prefer_title: Option<&'static str>,
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
            prefer_title: None,
            target,
        }
    }

    /// Require the chosen action's title to contain `substring`, so a kind that
    /// hosts more than one refactoring resolves to the intended one.
    fn preferring(mut self, substring: &'static str) -> Self {
        self.prefer_title = Some(substring);
        self
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
            std::sync::Arc::new(
                Self::new(
                    "inline",
                    "Inline",
                    "Inline the local variable, constant, or method at the position",
                    "refactor.inline",
                    TargetKind::Position,
                )
                // `refactor.inline` also carries "Make Static" at a method;
                // keep only the actual inline refactoring.
                .preferring("Inline"),
            ),
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

        // Choose the action of the requested kind. When a kind hosts several
        // refactorings, narrow to the one whose title matches this op (so we
        // never apply a neighbour like "Make Static" for an inline); then
        // prefer a single-target variant over a "replace all occurrences" one.
        let prefer = self.prefer_title.map(str::to_ascii_lowercase);
        let title_of = |a: &Value| {
            a.get("title")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
        };
        let chosen = actions
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|a| a.get("kind").and_then(Value::as_str) == Some(self.action_kind))
            .filter(|a| match &prefer {
                Some(want) => title_of(a).is_some_and(|t| t.contains(want.as_str())),
                None => true,
            })
            .min_by_key(|a| u8::from(title_of(a).is_some_and(|t| t.contains("occurrence"))))
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

/// Execute a jdtls delegate command (from our bundle) and return its result.
async fn execute_command(
    session: &JdtlsSession,
    command: &str,
    arguments: Value,
) -> CoreResult<Value> {
    session
        .client()
        .request(
            "workspace/executeCommand",
            json!({ "command": command, "arguments": arguments }),
        )
        .await
        .map_err(backend)
}

/// A `CodeActionParams` for a position (zero-width range) and refactor kind.
fn code_action_params(uri: &str, position: Position, only: &str) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": position.line, "character": position.character },
            "end": { "line": position.line, "character": position.character }
        },
        "context": { "diagnostics": [], "only": [only] }
    })
}

/// Change a method's signature: rename it, change its return type/visibility,
/// and reorder/add/remove/retype its parameters, updating all call sites.
///
/// Backed by the delegate-command bundle: `getChangeSignatureInfo` reports the
/// current signature, then `getRefactorEdit` computes the edit from the desired
/// one. Parameters omitted from the request default to the current values.
pub struct ChangeSignatureOp;

#[async_trait]
impl Operation for ChangeSignatureOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: "change-signature".into(),
            title: "Change signature".into(),
            description: "Change a method's name, return type, visibility, or parameters and \
                          update all call sites"
                .into(),
            kind: OperationKind::Edit,
            languages: vec![Language::Java],
            target: TargetKind::Position,
            params_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "New method name (defaults to current)." },
                    "return_type": { "type": "string", "description": "New return type (defaults to current)." },
                    "modifier": { "type": "string", "description": "New visibility, e.g. public/protected/private (defaults to current)." },
                    "delegate": { "type": "boolean", "default": false, "description": "Keep the original method as a delegate." },
                    "parameters": {
                        "type": "array",
                        "description": "The full new parameter list, in order. Omit to keep the current parameters.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string" },
                                "name": { "type": "string" },
                                "default_value": { "type": "string", "description": "Initializer for a newly added parameter." },
                                "original_index": { "type": "integer", "description": "Index of this parameter in the original signature, or -1 if new." }
                            },
                            "required": ["type", "name", "original_index"]
                        }
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
        session.ensure_indexed().await.map_err(backend)?;
        let uri = session.ensure_open(file).await.map_err(backend)?;

        let context = code_action_params(&uri, position, "refactor.change.signature");

        // Step 1: current signature.
        let info = execute_command(
            session,
            "henka.mcp.getChangeSignatureInfo",
            json!([context]),
        )
        .await?;
        if let Some(err) = info.get("errorMessage").and_then(Value::as_str) {
            return Err(CoreError::Backend(format!(
                "change-signature unavailable here: {err}"
            )));
        }
        let method_identifier = info
            .get("methodIdentifier")
            .cloned()
            .ok_or_else(|| CoreError::InvalidTarget("no method at this position".into()))?;

        // Step 2: build the desired signature, defaulting to the current one.
        let p = &req.params;
        let name = p
            .get("name")
            .cloned()
            .or_else(|| info.get("methodName").cloned())
            .unwrap_or(Value::Null);
        let modifier = p
            .get("modifier")
            .cloned()
            .or_else(|| info.get("modifier").cloned())
            .unwrap_or(Value::Null);
        let return_type = p
            .get("return_type")
            .cloned()
            .or_else(|| info.get("returnType").cloned())
            .unwrap_or(Value::Null);
        let parameters = match p.get("parameters") {
            Some(provided) => normalize_parameters(provided),
            None => info.get("parameters").cloned().unwrap_or(json!([])),
        };
        let exceptions = info.get("exceptions").cloned().unwrap_or(json!([]));
        let delegate = p.get("delegate").and_then(Value::as_bool).unwrap_or(false);

        let command_arguments = json!([
            method_identifier,
            delegate,
            name,
            modifier,
            return_type,
            parameters,
            exceptions,
            false, // preview
        ]);
        let refactor_params = json!({
            "command": "changeSignature",
            "context": context,
            "commandArguments": command_arguments,
        });

        let result = execute_command(
            session,
            "henka.mcp.getRefactorEdit",
            json!([refactor_params]),
        )
        .await?;
        if let Some(err) = result.get("errorMessage").and_then(Value::as_str) {
            return Err(CoreError::Backend(format!(
                "change-signature failed: {err}"
            )));
        }
        let edit = result.get("edit").cloned().unwrap_or(Value::Null);
        Ok(OperationOutcome::Edit(
            lsp::to_core_workspace_edit(edit).map_err(backend)?,
        ))
    }
}

/// Translate request parameters (snake_case) into the server's MethodParameter
/// shape (camelCase): `{type, name, defaultValue, originalIndex}`.
fn normalize_parameters(provided: &Value) -> Value {
    let items = provided.as_array().cloned().unwrap_or_default();
    let mapped: Vec<Value> = items
        .into_iter()
        .map(|item| {
            json!({
                "type": item.get("type").cloned().unwrap_or(Value::Null),
                "name": item.get("name").cloned().unwrap_or(Value::Null),
                "defaultValue": item.get("default_value").cloned().unwrap_or(Value::Null),
                "originalIndex": item.get("original_index").and_then(Value::as_i64).unwrap_or(-1),
            })
        })
        .collect();
    Value::Array(mapped)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_parameters_maps_to_server_shape() {
        let provided = json!([
            { "type": "int", "name": "b", "original_index": 1 },
            { "type": "String", "name": "extra", "default_value": "\"x\"", "original_index": -1 }
        ]);
        let mapped = normalize_parameters(&provided);
        assert_eq!(
            mapped,
            json!([
                { "type": "int", "name": "b", "defaultValue": null, "originalIndex": 1 },
                { "type": "String", "name": "extra", "defaultValue": "\"x\"", "originalIndex": -1 }
            ])
        );
    }

    #[test]
    fn normalize_parameters_defaults_missing_original_index() {
        let mapped = normalize_parameters(&json!([{ "type": "int", "name": "n" }]));
        assert_eq!(mapped[0]["originalIndex"], json!(-1));
        assert_eq!(mapped[0]["defaultValue"], Value::Null);
    }

    #[test]
    fn change_signature_descriptor_contract() {
        let d = ChangeSignatureOp.descriptor();
        assert_eq!(d.id, "change-signature");
        assert_eq!(d.kind, OperationKind::Edit);
        assert_eq!(d.target, TargetKind::Position);
        assert_eq!(d.languages, vec![Language::Java]);
        // The parameter list is part of the schema.
        assert!(d.params_schema["properties"].get("parameters").is_some());
    }

    #[test]
    fn java_code_action_set_contract() {
        let ops = CodeActionOp::java_set();
        let by_id: std::collections::BTreeMap<String, OperationDescriptor> = ops
            .iter()
            .map(|o| (o.descriptor().id.clone(), o.descriptor()))
            .collect();

        for id in [
            "extract-variable",
            "extract-constant",
            "extract-field",
            "extract-method",
            "inline",
            "organize-imports",
        ] {
            let d = by_id.get(id).unwrap_or_else(|| panic!("missing op `{id}`"));
            assert_eq!(d.kind, OperationKind::Edit);
        }
        assert_eq!(by_id["extract-variable"].target, TargetKind::Selection);
        assert_eq!(by_id["inline"].target, TargetKind::Position);
        assert_eq!(by_id["organize-imports"].target, TargetKind::File);
    }
}
