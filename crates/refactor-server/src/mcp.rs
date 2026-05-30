//! The MCP server handler: tenancy tools and the skill resource.
//!
//! Tools are dispatched manually (rather than via the `#[tool]` macros) because
//! the refactoring catalog is dynamic — driven by the registered language
//! providers in later phases. The tenancy tools here are the static core of
//! that same dispatch.

use std::sync::Arc;

use refactor_core::{Error as CoreError, Project, ProjectRegistry};
use rmcp::model::{
    Annotated, CallToolRequestParam, CallToolResult, Content, Implementation,
    InitializeRequestParam, InitializeResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParam, ProtocolVersion, RawResource, ReadResourceRequestParam,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tokio::sync::RwLock;

/// The skill resource: a written refactoring workflow, served over MCP and
/// embedded at build time so the server stays a single self-contained binary.
const SKILL_URI: &str = "skill://refactor/refactoring";
const SKILL_BODY: &str = include_str!("../skills/refactoring.md");

/// A JSON object, matching rmcp's tool-schema representation.
type JsonObject = Map<String, Value>;

/// The MCP server handler.
#[derive(Clone)]
pub struct RefactorMcp {
    registry: Arc<RwLock<ProjectRegistry>>,
}

impl RefactorMcp {
    /// Build a handler over the given registry.
    pub fn new(registry: ProjectRegistry) -> Self {
        Self {
            registry: Arc::new(RwLock::new(registry)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool parameter types
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
struct NoParams {}

// ---------------------------------------------------------------------------
// Tool catalog
// ---------------------------------------------------------------------------

impl RefactorMcp {
    /// The tenancy tools always present on the server. The refactoring catalog
    /// (added in later phases) is appended to this list.
    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool::new(
                "register_project",
                "Register a local source tree as a project so it can be refactored. Detects the \
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
        ]
    }

    async fn handle_call(&self, request: CallToolRequestParam) -> Result<CallToolResult, McpError> {
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
            other => Err(McpError::invalid_params(
                format!("unknown tool: `{other}`"),
                None,
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

impl ServerHandler for RefactorMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: env!("CARGO_PKG_NAME").into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Implementation::from_build_env()
            },
            instructions: Some(
                "Multi-tenant code refactoring server. One server hosts many projects; pass the \
                 project `id` on every refactoring call. Register a source tree with \
                 `register_project`, see what is registered with `list_projects`, and discover \
                 the refactorings available for a project with `list_refactorings`. Every \
                 refactoring supports a `dry_run` flag that returns a diff without touching disk. \
                 Before refactoring, read the resource `skill://refactor/refactoring` for the \
                 full workflow."
                    .into(),
            ),
        }
    }

    async fn initialize(
        &self,
        request: InitializeRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        Ok(InitializeResult {
            protocol_version: request.protocol_version,
            capabilities: self.get_info().capabilities,
            server_info: self.get_info().server_info,
            instructions: self.get_info().instructions,
        })
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tools(),
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.handle_call(request).await
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParam>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let raw = RawResource {
            uri: SKILL_URI.into(),
            name: "refactoring".into(),
            title: Some("Code refactoring workflow".into()),
            description: Some(
                "How to use this server: register a project, discover its refactorings, preview \
                 with dry_run, then apply."
                    .into(),
            ),
            mime_type: Some("text/markdown".into()),
            size: Some(SKILL_BODY.len() as u32),
            icons: None,
        };
        Ok(ListResourcesResult {
            resources: vec![Annotated::new(raw, None)],
            next_cursor: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        match request.uri.as_str() {
            SKILL_URI => Ok(ReadResourceResult {
                contents: vec![ResourceContents::TextResourceContents {
                    uri: SKILL_URI.into(),
                    mime_type: Some("text/markdown".into()),
                    text: SKILL_BODY.into(),
                    meta: None,
                }],
            }),
            other => Err(McpError::resource_not_found(
                "resource not found",
                Some(serde_json::json!({ "uri": other })),
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
        | CoreError::NoLanguageDetected(_) => McpError::invalid_params(err.to_string(), None),
        CoreError::ConfigRead { .. } | CoreError::ConfigWrite(_) | CoreError::Io(_) => {
            McpError::internal_error(err.to_string(), None)
        }
    }
}
