//! The MCP server handler: tenancy tools, the dynamic operation catalog, and
//! the skill resource.
//!
//! Tools are dispatched manually (rather than via the `#[tool]` macros) because
//! the operation catalog is dynamic — driven by the registered language
//! providers. The tenancy and discovery tools are the static core of that same
//! dispatch; every catalog operation is surfaced as its own tool.

use std::sync::Arc;

use henka_core::operation::{OperationCtx, OperationOutcome, OperationRequest};
use henka_core::{
    EditApplier, Error as CoreError, OperationRegistry, Project, ProjectRegistry, ProviderRegistry,
};
use rmcp::model::{
    Annotated, CallToolRequestParams, CallToolResult, Content, Implementation,
    InitializeRequestParams, InitializeResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, RawResource, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio::sync::RwLock;

use crate::ops;

/// The skill resource: a written refactoring workflow, served over MCP and
/// embedded at build time so the server stays a single self-contained binary.
const SKILL_URI: &str = "skill://henka/refactoring";
const SKILL_BODY: &str = include_str!("../skills/refactoring.md");

/// A JSON object, matching rmcp's tool-schema representation.
type JsonObject = Map<String, Value>;

/// The MCP server handler.
#[derive(Clone)]
pub struct HenkaMcp {
    registry: Arc<RwLock<ProjectRegistry>>,
    providers: Arc<ProviderRegistry>,
    operations: Arc<OperationRegistry>,
}

impl HenkaMcp {
    /// Build a handler over the given project registry and language providers.
    /// The operation catalog is assembled from the providers' contributions.
    pub fn new(registry: ProjectRegistry, providers: ProviderRegistry) -> Self {
        let mut operations = OperationRegistry::new();
        operations.extend(providers.operations());
        Self {
            registry: Arc::new(RwLock::new(registry)),
            providers: Arc::new(providers),
            operations: Arc::new(operations),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool parameter types (tenancy / discovery)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct RegisterProjectParams {
    /// Filesystem path to the project's source tree (a directory).
    root: String,
    /// Optional explicit project id (a slug of lowercase letters, digits and
    /// dashes). Derived from the directory name when omitted.
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ProjectIdParams {
    /// The id of a registered project.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListOperationsParams {
    /// The id of a registered project to list operations for.
    project: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NoParams {}

// ---------------------------------------------------------------------------
// Tool catalog
// ---------------------------------------------------------------------------

impl HenkaMcp {
    /// The full tool list: tenancy + discovery tools, plus one tool per
    /// operation in the catalog.
    fn tools(&self) -> Vec<Tool> {
        let mut tools = vec![
            Tool::new(
                "register_project",
                "Register a local source tree as a project so it can be operated on. Detects the \
                 project's languages and persists the registration across restarts.",
                schema::<RegisterProjectParams>(),
            ),
            Tool::new(
                "unregister_project",
                "Forget a registered project. Never deletes source.",
                schema::<ProjectIdParams>(),
            ),
            Tool::new(
                "list_projects",
                "List all registered projects with their roots and detected languages.",
                schema::<NoParams>(),
            ),
            Tool::new(
                "project_status",
                "Report a registered project's root and detected languages.",
                schema::<ProjectIdParams>(),
            ),
            Tool::new(
                "list_operations",
                "List the operations (refactorings, structural replace, semantic queries) \
                 available for a project, each with its kind, target, and parameters.",
                schema::<ListOperationsParams>(),
            ),
        ];
        tools.extend(
            self.operations
                .descriptors()
                .iter()
                .map(ops::operation_tool),
        );
        tools
    }

    async fn handle_call(
        &self,
        request: CallToolRequestParams,
    ) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            "register_project" => {
                let p: RegisterProjectParams = parse_args(request.arguments)?;
                let project = {
                    let mut reg = self.registry.write().await;
                    reg.register(p.id, p.root).map_err(into_mcp)?
                };
                ok_json(&project)
            }
            "unregister_project" => {
                let p: ProjectIdParams = parse_args(request.arguments)?;
                let project = {
                    let mut reg = self.registry.write().await;
                    reg.unregister(&p.id).map_err(into_mcp)?
                };
                ok_json(&project)
            }
            "list_projects" => {
                let _: NoParams = parse_args(request.arguments)?;
                let reg = self.registry.read().await;
                let projects: Vec<&Project> = reg.list().collect();
                ok_json(&projects)
            }
            "project_status" => {
                let p: ProjectIdParams = parse_args(request.arguments)?;
                let reg = self.registry.read().await;
                let project = reg.get(&p.id).map_err(into_mcp)?;
                ok_json(project)
            }
            "list_operations" => {
                let p: ListOperationsParams = parse_args(request.arguments)?;
                let reg = self.registry.read().await;
                let project = reg.get(&p.project).map_err(into_mcp)?;
                ok_json(&self.operations.descriptors_for(project))
            }
            name => self.dispatch_operation(name, request.arguments).await,
        }
    }

    /// Run a catalog operation: resolve the project and operation, build the
    /// request, run it, and either return the query result or preview/apply the
    /// edit.
    async fn dispatch_operation(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args = arguments.unwrap_or_default();
        let project_id = ops::project_id(&args)?;

        let project: Project = {
            let reg = self.registry.read().await;
            reg.get(&project_id).map_err(into_mcp)?.clone()
        };

        let operation = self
            .operations
            .resolve(name, &project.languages)
            .map_err(|_| {
                McpError::invalid_params(
                    format!("unknown tool or operation `{name}` for project `{project_id}`"),
                    None,
                )
            })?;
        let descriptor = operation.descriptor();

        let target = ops::parse_target(&args, descriptor.target)?;
        let params = ops::operation_params(&args);

        let language = project
            .languages
            .iter()
            .copied()
            .find(|&l| descriptor.applies_to(l))
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("operation `{name}` does not apply to this project's languages"),
                    None,
                )
            })?;
        let provider = self.providers.get(language).ok_or_else(|| {
            McpError::internal_error(format!("no provider registered for `{language}`"), None)
        })?;
        let session = provider.session(&project).await.map_err(into_mcp)?;

        let ctx = OperationCtx {
            project: &project,
            session,
        };
        let req = OperationRequest { target, params };
        let outcome = operation.run(&ctx, &req).await.map_err(into_mcp)?;

        match outcome {
            OperationOutcome::Query(value) => ok_json(&value),
            OperationOutcome::Edit(edit) => {
                if ops::dry_run(&args) {
                    let files = EditApplier::preview(&edit, &project.root).map_err(into_mcp)?;
                    ok_json(&json!({ "dry_run": true, "files": files }))
                } else {
                    let applied = EditApplier::apply(&edit, &project.root).map_err(into_mcp)?;
                    // Keep the session's view current so later operations in
                    // this session see the applied changes.
                    ctx.session.sync_changed(&applied.changed_files).await;
                    ok_json(&json!({ "dry_run": false, "applied": applied }))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

impl ServerHandler for HenkaMcp {
    fn get_info(&self) -> ServerInfo {
        // rmcp's model structs are #[non_exhaustive], so build via default and
        // assign the fields we set.
        let mut implementation = Implementation::from_build_env();
        implementation.name = env!("CARGO_PKG_NAME").into();
        implementation.version = env!("CARGO_PKG_VERSION").into();

        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::V_2024_11_05;
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        info.server_info = implementation;
        info.instructions = Some(
            "Multi-tenant server for semantics-aware code operations. One server hosts many \
             projects; pass the project `id` on every operation. Register a source tree with \
             `register_project`, then `list_operations` to see what a project supports: \
             refactorings and structural replace (edits), plus semantic queries like \
             find-usages and go-to-definition. Prefer a semantic query over text search. \
             Edit operations default to a preview (a diff); pass `dry_run: false` to apply. \
             Read the resource `skill://henka/refactoring` for the full workflow."
                .into(),
        );
        info
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        let mut result = self.get_info();
        result.protocol_version = request.protocol_version;
        Ok(result)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tools(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_call(request).await
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let raw = RawResource {
            uri: SKILL_URI.into(),
            name: "refactoring".into(),
            title: Some("Code refactoring workflow".into()),
            description: Some(
                "How to use this server: register a project, discover its operations, prefer \
                 semantic queries over text search, and preview edits with dry_run before \
                 applying."
                    .into(),
            ),
            mime_type: Some("text/markdown".into()),
            size: Some(SKILL_BODY.len() as u32),
            icons: None,
            meta: None,
        };
        Ok(ListResourcesResult {
            resources: vec![Annotated::new(raw, None)],
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        match request.uri.as_str() {
            SKILL_URI => Ok(ReadResourceResult::new(vec![
                ResourceContents::TextResourceContents {
                    uri: SKILL_URI.into(),
                    mime_type: Some("text/markdown".into()),
                    text: SKILL_BODY.into(),
                    meta: None,
                },
            ])),
            other => Err(McpError::resource_not_found(
                "resource not found",
                Some(json!({ "uri": other })),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a tool input schema from a `JsonSchema` type.
fn schema<T: JsonSchema>() -> Arc<JsonObject> {
    let schema = schemars::schema_for!(T);
    match serde_json::to_value(schema) {
        Ok(Value::Object(map)) => Arc::new(map),
        _ => Arc::new(JsonObject::new()),
    }
}

/// Deserialize tool arguments into the expected parameter type.
fn parse_args<T: DeserializeOwned>(args: Option<JsonObject>) -> Result<T, McpError> {
    let value = Value::Object(args.unwrap_or_default());
    serde_json::from_value(value)
        .map_err(|e| McpError::invalid_params(format!("invalid arguments: {e}"), None))
}

/// Return a successful tool result carrying pretty-printed JSON.
fn ok_json<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

/// Map a core error to the closest MCP error.
fn into_mcp(err: CoreError) -> McpError {
    match err {
        CoreError::ProjectNotFound(_)
        | CoreError::ProjectAlreadyExists(_)
        | CoreError::InvalidProjectId(_)
        | CoreError::PathNotFound(_)
        | CoreError::NotADirectory(_)
        | CoreError::NoLanguageDetected(_)
        | CoreError::OperationNotAvailable(_)
        | CoreError::InvalidTarget(_)
        | CoreError::PositionOutOfRange { .. }
        | CoreError::OverlappingEdits(_) => McpError::invalid_params(err.to_string(), None),
        CoreError::Backend(_)
        | CoreError::ConfigRead { .. }
        | CoreError::ConfigWrite(_)
        | CoreError::Io(_) => McpError::internal_error(err.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::path::Path;

    use async_trait::async_trait;
    use henka_core::operation::{
        Operation, OperationCtx, OperationDescriptor, OperationKind, OperationOutcome,
        OperationRequest, Target, TargetKind,
    };
    use henka_core::{
        FileEdit, Language, LanguageProvider, LanguageSession, PositionEncoding, Range, Result,
        TextEdit, WorkspaceEdit,
    };

    use super::*;

    /// Inserts `text` at the target position.
    struct InsertOp;

    #[async_trait]
    impl Operation for InsertOp {
        fn descriptor(&self) -> OperationDescriptor {
            OperationDescriptor {
                id: "insert-text".into(),
                title: "Insert text".into(),
                description: "Insert text at a position".into(),
                kind: OperationKind::Edit,
                languages: vec![Language::Java],
                target: TargetKind::Position,
                params_schema: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                }),
            }
        }

        async fn run(
            &self,
            _ctx: &OperationCtx<'_>,
            req: &OperationRequest,
        ) -> Result<OperationOutcome> {
            let text = req
                .params
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let (file, position) = match &req.target {
                Target::Position { file, position } => (file.clone(), *position),
                _ => return Err(CoreError::InvalidTarget("expected a position".into())),
            };
            Ok(OperationOutcome::Edit(WorkspaceEdit {
                encoding: PositionEncoding::Utf16,
                files: vec![FileEdit {
                    path: file,
                    edits: vec![TextEdit {
                        range: Range::new(position, position),
                        new_text: text,
                    }],
                }],
                file_ops: Vec::new(),
            }))
        }
    }

    /// Echoes the target back as a query result.
    struct EchoQuery;

    #[async_trait]
    impl Operation for EchoQuery {
        fn descriptor(&self) -> OperationDescriptor {
            OperationDescriptor {
                id: "echo".into(),
                title: "Echo".into(),
                description: "Echo the target".into(),
                kind: OperationKind::Query,
                languages: vec![Language::Java],
                target: TargetKind::Position,
                params_schema: json!({ "type": "object", "properties": {} }),
            }
        }

        async fn run(
            &self,
            _ctx: &OperationCtx<'_>,
            req: &OperationRequest,
        ) -> Result<OperationOutcome> {
            let line = match &req.target {
                Target::Position { position, .. } => position.line,
                _ => 0,
            };
            Ok(OperationOutcome::Query(json!({ "line": line })))
        }
    }

    struct MockSession;
    impl LanguageSession for MockSession {
        fn language(&self) -> Language {
            Language::Java
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct MockProvider;
    #[async_trait]
    impl LanguageProvider for MockProvider {
        fn language(&self) -> Language {
            Language::Java
        }
        fn operations(&self) -> Vec<Arc<dyn Operation>> {
            vec![Arc::new(InsertOp), Arc::new(EchoQuery)]
        }
        async fn session(&self, _project: &Project) -> Result<Arc<dyn LanguageSession>> {
            Ok(Arc::new(MockSession))
        }
    }

    /// Build a handler over a fresh Java project containing `Main.java`.
    fn handler_with_project(dir: &Path) -> (HenkaMcp, std::path::PathBuf) {
        let cfg = dir.join("projects.toml");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pom.xml"), "<project/>").unwrap();
        std::fs::write(root.join("Main.java"), "hello\n").unwrap();

        let mut registry = ProjectRegistry::load(&cfg).unwrap();
        registry.register(Some("p".into()), &root).unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(MockProvider));

        (HenkaMcp::new(registry, providers), root)
    }

    fn args(value: Value) -> Option<JsonObject> {
        value.as_object().cloned()
    }

    fn call(name: &str, arguments: Option<JsonObject>) -> CallToolRequestParams {
        let mut request = CallToolRequestParams::new(name.to_string());
        request.arguments = arguments;
        request
    }

    #[tokio::test]
    async fn catalog_lists_operation_tools() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, _) = handler_with_project(dir.path());
        let names: Vec<String> = mcp.tools().iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"insert-text".to_string()));
        assert!(names.contains(&"echo".to_string()));
        assert!(names.contains(&"list_operations".to_string()));
    }

    #[tokio::test]
    async fn edit_previews_then_applies() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, root) = handler_with_project(dir.path());
        let main = root.join("Main.java");

        // Preview (default dry_run): file must be untouched.
        let preview = mcp
            .handle_call(call(
                "insert-text",
                args(json!({ "project": "p", "file": "Main.java", "line": 0, "character": 0, "text": "X" })),
            ))
            .await
            .unwrap();
        assert_ne!(preview.is_error, Some(true));
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "hello\n");

        // Apply.
        let applied = mcp
            .handle_call(call(
                "insert-text",
                args(json!({ "project": "p", "file": "Main.java", "line": 0, "character": 0, "text": "X", "dry_run": false })),
            ))
            .await
            .unwrap();
        assert_ne!(applied.is_error, Some(true));
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "Xhello\n");
    }

    #[tokio::test]
    async fn query_runs_without_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, _) = handler_with_project(dir.path());
        let res = mcp
            .handle_call(call(
                "echo",
                args(json!({ "project": "p", "file": "Main.java", "line": 3, "character": 0 })),
            ))
            .await
            .unwrap();
        assert_ne!(res.is_error, Some(true));
    }

    #[tokio::test]
    async fn unknown_operation_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, _) = handler_with_project(dir.path());
        let err = mcp
            .handle_call(call("nonexistent", args(json!({ "project": "p" }))))
            .await
            .unwrap_err();
        assert!(err.message.contains("unknown tool or operation"));
    }
}
