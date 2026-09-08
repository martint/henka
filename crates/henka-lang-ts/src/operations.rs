//! The TypeScript/JavaScript operations, driven over LSP.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use henka_core::operation::{
    Operation, OperationCtx, OperationDescriptor, OperationKind, OperationOutcome,
    OperationRequest, Target, TargetKind,
};
use henka_core::{Error as CoreError, Language, LanguageRoute, Position, Result as CoreResult};
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

/// Extract a whole-file target.
fn file_target(req: &OperationRequest) -> CoreResult<&PathBuf> {
    match &req.target {
        Target::File { file } => Ok(file),
        _ => Err(CoreError::InvalidTarget(
            "this operation expects a file".into(),
        )),
    }
}

/// The extra lines of context a request asked for around each result location.
fn context_lines(req: &OperationRequest) -> usize {
    req.params
        .get("context_lines")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(0)
}

/// Where a query's results quote their source from: the working copy the
/// request is being answered against, with paths reported relative to the
/// project root.
fn source<'a>(session: &'a TsSession, req: &OperationRequest) -> henka_lsp::Source<'a> {
    henka_lsp::Source::new(
        session.root(),
        session.content_root(),
        context_lines(req),
    )
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

/// Search for symbols across the project by name query.
pub struct SymbolSearchOp;

#[async_trait]
impl Operation for SymbolSearchOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: "symbol-search".into(),
            title: "Symbol search".into(),
            description: "Search for symbols across the project by name query.".into(),
            kind: OperationKind::Query,
            languages: languages(),
            target: TargetKind::Project,
            params_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Partial or full symbol name to search for."
                    },
                    "context_lines": henka_lsp::context_lines_param(),
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of symbols to return.",
                        "default": henka_lsp::DEFAULT_SYMBOL_SEARCH_LIMIT
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
        let query = req
            .params
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::InvalidTarget("`query` is required".into()))?;
        let limit = req
            .params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(henka_lsp::DEFAULT_SYMBOL_SEARCH_LIMIT);

        let out = session
            .symbol_search(query, limit, context_lines(req))
            .await
            .map_err(backend)?;
        Ok(OperationOutcome::Query(out))
    }
}

/// A position-targeted query that resolves the symbol under the position to a
/// set of locations — the goto family. One struct serves each of them: they
/// differ only in the request issued and the key their list is published under,
/// and the work either side of the request is the same.
pub struct GotoQueryOp {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    /// The LSP request to issue (e.g. `textDocument/definition`).
    method: &'static str,
    /// The key the location list is published under (e.g. `definitions`).
    result_key: &'static str,
}

impl GotoQueryOp {
    /// Resolve the symbol at a position to where it is declared.
    pub fn definition() -> Self {
        Self {
            id: "go-to-definition",
            title: "Go to definition",
            description: "Resolve the symbol at the given position to where it is defined",
            method: "textDocument/definition",
            result_key: "definitions",
        }
    }

    /// From an interface, abstract method, or trait member, resolve the
    /// concrete implementations. Not a special case of find-usages: a call to
    /// a method and an override of it are different questions, and only the
    /// override says where the behavior lives.
    pub fn implementations() -> Self {
        Self {
            id: "find-implementations",
            title: "Find implementations",
            description: "Find the concrete implementations of the symbol at the given position",
            method: "textDocument/implementation",
            result_key: "implementations",
        }
    }
}

#[async_trait]
impl Operation for GotoQueryOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: self.id.into(),
            title: self.title.into(),
            description: self.description.into(),
            kind: OperationKind::Query,
            languages: languages(),
            target: TargetKind::Position,
            params_schema: json!({
                "type": "object",
                "properties": {
                    "context_lines": henka_lsp::context_lines_param()
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

        session.ensure_indexed().await.map_err(backend)?;
        let uri = session.ensure_open(file).await.map_err(backend)?;
        let result: Value = session
            .client()
            .request(
                self.method,
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.line, "character": position.character },
                }),
            )
            .await
            .map_err(backend)?;

        let out = henka_lsp::goto_to_query(result, &source(session, req), self.result_key)
            .map_err(backend)?;
        Ok(OperationOutcome::Query(out))
    }
}

/// Describe the symbol at a position: its resolved type or signature and its
/// documentation.
pub struct DescribeSymbolOp;

#[async_trait]
impl Operation for DescribeSymbolOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: "describe-symbol".into(),
            title: "Describe symbol".into(),
            description: "Report the type, signature, and documentation of the symbol at the \
                          given position"
                .into(),
            kind: OperationKind::Query,
            languages: languages(),
            target: TargetKind::Position,
            params_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn run(
        &self,
        ctx: &OperationCtx<'_>,
        req: &OperationRequest,
    ) -> CoreResult<OperationOutcome> {
        let session = ts(ctx)?;
        let (file, position) = position_target(req)?;

        session.ensure_indexed().await.map_err(backend)?;
        let uri = session.ensure_open(file).await.map_err(backend)?;
        let result: Value = session
            .client()
            .request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.line, "character": position.character },
                }),
            )
            .await
            .map_err(backend)?;

        let out = henka_lsp::hover_to_query(result).map_err(backend)?;
        Ok(OperationOutcome::Query(out))
    }
}

/// List the symbols declared in a file, nested as they are in the source.
pub struct FileOutlineOp;

#[async_trait]
impl Operation for FileOutlineOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: "file-outline".into(),
            title: "File outline".into(),
            description: "List the symbols declared in the given file, with their coordinates"
                .into(),
            kind: OperationKind::Query,
            languages: languages(),
            target: TargetKind::File,
            params_schema: json!({
                "type": "object",
                "properties": {
                    "context_lines": henka_lsp::context_lines_param()
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
        let file = file_target(req)?;

        session.ensure_indexed().await.map_err(backend)?;
        let uri = session.ensure_open(file).await.map_err(backend)?;
        let result: Value = session
            .client()
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await
            .map_err(backend)?;

        let out = henka_lsp::document_symbols_to_query(result, &source(session, req), &uri)
            .map_err(backend)?;
        Ok(OperationOutcome::Query(out))
    }
}

/// Resolve a position to the call hierarchy items it names, for the incoming-
/// and outgoing-calls queries to walk.
pub struct PrepareCallHierarchyOp;

#[async_trait]
impl Operation for PrepareCallHierarchyOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: "prepare-call-hierarchy".into(),
            title: "Prepare call hierarchy".into(),
            description: "Resolve the position to the call hierarchy items to walk with \
                          incoming-calls or outgoing-calls"
                .into(),
            kind: OperationKind::Query,
            languages: languages(),
            target: TargetKind::Position,
            params_schema: json!({
                "type": "object",
                "properties": {
                    "context_lines": henka_lsp::context_lines_param()
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

        session.ensure_indexed().await.map_err(backend)?;
        let uri = session.ensure_open(file).await.map_err(backend)?;
        let result: Value = session
            .client()
            .request(
                "textDocument/prepareCallHierarchy",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.line, "character": position.character },
                }),
            )
            .await
            .map_err(backend)?;

        let out = henka_lsp::call_hierarchy_items_to_query(result, &source(session, req))
            .map_err(backend)?;
        Ok(OperationOutcome::Query(out))
    }
}

/// The `item` parameter the call-hierarchy directions take, as a JSON Schema
/// property. It is the server's own handle on a declaration and is passed back
/// unchanged: servers attach private data to it and reject an item without it.
fn call_hierarchy_item_param() -> Value {
    json!({
        "type": "object",
        "description": "A call hierarchy item, taken verbatim from a prepare-call-hierarchy \
                        result's `item` field. Treat it as opaque and pass it back unmodified."
    })
}

/// Extract the required `item` parameter.
fn call_hierarchy_item(req: &OperationRequest) -> CoreResult<&Value> {
    req.params
        .get("item")
        .filter(|item| item.is_object())
        .ok_or_else(|| {
            CoreError::InvalidTarget(
                "`item` is required: run prepare-call-hierarchy first and pass one of its \
                 `item` values back unchanged"
                    .into(),
            )
        })
}

/// Ready the session for a call-hierarchy query: warm the index, and open the
/// file the item lives in so the server answers from the content the rest of
/// the session sees. A file outside the project (a dependency's source) simply
/// isn't opened.
async fn prepare_for_call_query(session: &TsSession, item: &Value) -> CoreResult<()> {
    session.ensure_indexed().await.map_err(backend)?;
    if let Some(uri) = item.get("uri").and_then(Value::as_str) {
        let _ = session.ensure_open(&henka_lsp::uri_to_path(uri)).await;
    }
    Ok(())
}

/// Find the callers of a call hierarchy item.
pub struct IncomingCallsOp;

#[async_trait]
impl Operation for IncomingCallsOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: "incoming-calls".into(),
            title: "Incoming calls".into(),
            description: "Find the callers of a call hierarchy item, with their call sites".into(),
            kind: OperationKind::Query,
            languages: languages(),
            target: TargetKind::Project,
            params_schema: json!({
                "type": "object",
                "required": ["item"],
                "properties": {
                    "item": call_hierarchy_item_param(),
                    "context_lines": henka_lsp::context_lines_param()
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
        let item = call_hierarchy_item(req)?;

        prepare_for_call_query(session, item).await?;
        let result: Value = session
            .client()
            .request("callHierarchy/incomingCalls", json!({ "item": item }))
            .await
            .map_err(backend)?;

        let out = henka_lsp::incoming_calls_to_query(result, &source(session, req)).map_err(backend)?;
        Ok(OperationOutcome::Query(out))
    }
    fn route(&self, params: &Value) -> LanguageRoute {
        henka_lsp::call_hierarchy_item_route(params)
    }

}

/// Find what a call hierarchy item calls.
pub struct OutgoingCallsOp;

#[async_trait]
impl Operation for OutgoingCallsOp {
    fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            id: "outgoing-calls".into(),
            title: "Outgoing calls".into(),
            description: "Find what a call hierarchy item calls, with the call sites inside it"
                .into(),
            kind: OperationKind::Query,
            languages: languages(),
            target: TargetKind::Project,
            params_schema: json!({
                "type": "object",
                "required": ["item"],
                "properties": {
                    "item": call_hierarchy_item_param(),
                    "context_lines": henka_lsp::context_lines_param()
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
        let item = call_hierarchy_item(req)?;

        prepare_for_call_query(session, item).await?;
        let result: Value = session
            .client()
            .request("callHierarchy/outgoingCalls", json!({ "item": item }))
            .await
            .map_err(backend)?;

        // Outgoing call sites are written in the queried item's own file.
        let called_from = item.get("uri").and_then(Value::as_str).unwrap_or_default();
        let out = henka_lsp::outgoing_calls_to_query(result, &source(session, req), called_from)
            .map_err(backend)?;
        Ok(OperationOutcome::Query(out))
    }
    fn route(&self, params: &Value) -> LanguageRoute {
        henka_lsp::call_hierarchy_item_route(params)
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
                    },
                    "context_lines": henka_lsp::context_lines_param()
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

        let usages = henka_lsp::locations_to_query(result, &source(session, req)).map_err(backend)?;
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
