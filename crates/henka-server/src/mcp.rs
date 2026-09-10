//! The MCP server handler: tenancy tools, the dynamic operation catalog, and
//! the skill resource.
//!
//! Tools are dispatched manually (rather than via the `#[tool]` macros) because
//! the operation catalog is dynamic — driven by the registered language
//! providers. The tenancy and discovery tools are the static core of that same
//! dispatch; every catalog operation is surfaced as its own tool.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use henka_core::operation::{
    LanguageRoute, Operation, OperationCtx, OperationDescriptor, OperationKind, OperationOutcome,
    OperationRequest, TargetKind,
};
use henka_core::{
    ChangedFile, EditApplier, Error as CoreError, FileOperation, Language, LanguageProvider,
    LanguageSession, OperationRegistry, Project, ProjectRegistry, ProviderRegistry, RequestGuard,
    Target, WorkspaceEdit, detect_revision, repo_identity, working_copy_delta,
    working_copy_fingerprint,
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
use crate::pathmap::PathMap;

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
    /// Rewrites caller-supplied paths to the paths Henka sees (e.g. a host
    /// prefix mounted under a different path in a container).
    path_map: Arc<PathMap>,
    /// Directories whose immediate children are auto-registered as projects (a
    /// workspaces mount). Scanned once at startup and again on `list_projects`.
    auto_register_roots: Arc<Vec<PathBuf>>,
}

impl HenkaMcp {
    /// Build a handler over the given project registry and language providers.
    /// The operation catalog is assembled from the providers' contributions;
    /// the path map is taken from `HENKA_PATH_MAP`.
    pub fn new(registry: ProjectRegistry, providers: ProviderRegistry) -> Self {
        let mut operations = OperationRegistry::new();
        operations.extend(providers.operations());
        let path_map = PathMap::from_env();
        if !path_map.is_empty() {
            tracing::info!("translating caller paths via HENKA_PATH_MAP");
        }
        let auto_register_roots = auto_register_roots(&path_map);
        if !auto_register_roots.is_empty() {
            tracing::info!(roots = ?auto_register_roots, "auto-registering projects under workspace roots");
        }
        Self {
            registry: Arc::new(RwLock::new(registry)),
            providers: Arc::new(providers),
            operations: Arc::new(operations),
            path_map: Arc::new(path_map),
            auto_register_roots: Arc::new(auto_register_roots),
        }
    }

    /// Scan the configured workspace roots and register any unregistered child
    /// projects. Called once at startup and again on `list_projects`, so a
    /// worktree created after the server started is picked up without a
    /// restart. A no-op when no roots are configured.
    pub async fn warm_registry(&self) {
        if self.auto_register_roots.is_empty() {
            return;
        }
        let added = {
            let mut reg = self.registry.write().await;
            reg.auto_register(&self.auto_register_roots)
        };
        if !added.is_empty() {
            tracing::info!(count = added.len(), "auto-registered projects");
        }
    }
}

/// The directories whose immediate children Henka auto-registers as projects.
///
/// `HENKA_AUTO_REGISTER_ROOTS` (comma/semicolon/newline-separated absolute
/// paths) when set — set it empty to disable. Otherwise the path map's
/// container-side prefixes plus the conventional `/workspaces` mount. Only
/// existing directories are kept, so on a host without a workspaces mount this
/// is simply empty.
fn auto_register_roots(path_map: &PathMap) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = match std::env::var("HENKA_AUTO_REGISTER_ROOTS") {
        Ok(spec) => spec
            .split([',', ';', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
        Err(_) => {
            let mut defaults = path_map.container_prefixes();
            defaults.push(PathBuf::from("/workspaces"));
            defaults
        }
    };
    roots.retain(|p| p.is_dir());
    roots.sort();
    roots.dedup();
    roots
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
// Registration result
// ---------------------------------------------------------------------------

/// A successful registration, plus an explanation when the path map rewrote the
/// caller's root onto a mount Henka sees.
#[derive(Debug, serde::Serialize)]
struct RegisterResult<'a> {
    #[serde(flatten)]
    project: &'a Project,
    #[serde(skip_serializing_if = "Option::is_none")]
    translation: Option<PathTranslation>,
}

/// How a caller-supplied path was rewritten onto the location Henka reads, with
/// a note reassuring that the two name the same files.
#[derive(Debug, serde::Serialize)]
struct PathTranslation {
    /// The path the caller supplied.
    from: PathBuf,
    /// The path Henka resolved it to and now reads the project at.
    to: PathBuf,
    /// Plain-language reassurance for the caller.
    note: String,
}

impl PathTranslation {
    /// Describe a host→mount rewrite: the same tree, reachable under a different
    /// prefix inside Henka.
    fn mount(from: &Path, to: &Path) -> Self {
        let note = format!(
            "Henka reads this project at `{}`, the configured mount for your `{}`. \
             When the mount is a bind of your checkout it is the same tree on disk, \
             not a separate copy — coordinates you computed against your copy apply \
             directly. Don't judge that by the path alone (mounts differ between \
             containers): call project_status and confirm its `revision` and \
             `digest` match your checkout (`digest` is reproducible from your \
             changed files). If they differ, target edits with an explicit \
             `workspace` or guard positions with `expect`.",
            to.display(),
            from.display(),
        );
        Self {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            note,
        }
    }
}

// ---------------------------------------------------------------------------
// Project status
// ---------------------------------------------------------------------------

/// A project plus the version-control state Henka currently reads it at.
///
/// The VCS fields let a caller detect that Henka's checkout has drifted from the
/// working copy they are editing — e.g. "Henka is on `trunk`, I'm on my feature
/// branch" — before trusting position-targeted coordinates against it.
#[derive(Debug, serde::Serialize)]
struct ProjectStatus<'a> {
    #[serde(flatten)]
    project: &'a Project,
    /// The VCS state of the project root, or `null` when it is not a jj/git
    /// working copy.
    vcs: Option<VcsStatus>,
    /// The caller-side (host) path this in-container root corresponds to, when
    /// the project sits under a path-map mount. Lets a caller confirm that the
    /// `/workspaces/...` root Henka reports is its own working copy, not a
    /// separate checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    host_path: Option<PathBuf>,
}

/// The version-control state of a project root.
#[derive(Debug, serde::Serialize)]
struct VcsStatus {
    /// Which VCS reported the state (`jj` or `git`).
    vcs: String,
    /// The current revision: a jj change id or a git short commit hash.
    revision: String,
    /// The branch/bookmark name, if the revision carries one.
    branch: Option<String>,
    /// The shared repository root (its git common dir or jj repo dir). Sibling
    /// working copies of the same repo report the same value.
    repo_root: Option<PathBuf>,
    /// Whether the working copy has uncommitted changes against its base.
    dirty: bool,
    /// The files changed against the base (sorted), each with its git blob object
    /// id. Lets a caller compare the server's dirty set — and each file's content
    /// id — against its own (`jj diff --summary` / `git status`, then
    /// `git hash-object`) without reasoning about mount paths.
    changed_files: Vec<ChangedFile>,
    /// A combined content digest over `changed_files` (see
    /// [`WorkingCopyFingerprint`](henka_core::WorkingCopyFingerprint)), or `null`
    /// for a clean tree. Reproducible in the caller's own checkout, it proves the
    /// server sees the same uncommitted edits even under a different mount path.
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
}

/// Gather the project plus the VCS state of its root. Detecting the revision is
/// read-only; the dirty check snapshots the working copy (jj's normal
/// behavior), matching how operations already read the tree.
fn project_status<'a>(project: &'a Project, path_map: &PathMap) -> ProjectStatus<'a> {
    let vcs = detect_revision(&project.root).map(|rev| {
        let changed = working_copy_delta(&project.root);
        let fingerprint = working_copy_fingerprint(&project.root, &changed);
        VcsStatus {
            vcs: rev.vcs.to_string(),
            revision: rev.id,
            branch: rev.branch,
            repo_root: repo_identity(&project.root).map(|id| id.path),
            dirty: !fingerprint.files.is_empty(),
            changed_files: fingerprint.files,
            digest: fingerprint.digest,
        }
    });
    let host_path = path_map.reverse(&project.root);
    ProjectStatus {
        project,
        vcs,
        host_path,
    }
}

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
                "Report a registered project's root, detected languages, and the version-control \
                 state Henka reads it at: revision, branch, repo root, dirty, the changed-file set, \
                 and a content digest of the working copy. Compare `revision`/`digest`/`changed_files` \
                 against your own checkout (not the path — the server may mount it elsewhere) to \
                 confirm Henka sees the same tree, including uncommitted edits, before trusting \
                 position-targeted coordinates.",
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
                let requested = PathBuf::from(&p.root);
                let root = self.path_map.map(&requested);
                let rewritten = root != requested;
                let mut reg = self.registry.write().await;
                match reg.register(p.id, root) {
                    Ok(project) => {
                        // When the path map rewrote the caller's root, say so
                        // explicitly: the rewrite is the configured mount, and
                        // the result is the same tree (a bind mount), not a
                        // separate checkout — the misread that otherwise makes
                        // callers distrust the registration.
                        let translation = rewritten
                            .then(|| PathTranslation::mount(&requested, &project.root));
                        ok_json(&RegisterResult {
                            project: &project,
                            translation,
                        })
                    }
                    // A missing path usually means the caller named a path Henka
                    // cannot see (its own filesystem differs, e.g. a container
                    // mount). Help them find the path Henka does see.
                    Err(CoreError::PathNotFound(missing)) => {
                        Err(path_not_found_error(&missing, &suggest_mounts(&missing, &reg)))
                    }
                    Err(e) => Err(into_mcp(e)),
                }
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
                // Pick up worktrees created since the last scan before listing.
                self.warm_registry().await;
                let reg = self.registry.read().await;
                let projects: Vec<&Project> = reg.list().collect();
                ok_json(&projects)
            }
            "project_status" => {
                let p: ProjectIdParams = parse_args(request.arguments)?;
                let reg = self.registry.read().await;
                let project = reg.get(&p.id).map_err(into_mcp)?;
                ok_json(&project_status(project, &self.path_map))
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

    /// Run a catalog operation: resolve the project, choose the language(s) that
    /// will serve the request, run the operation on each, and either return the
    /// query result or preview/apply the edit.
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

        // The project's languages that contribute an operation under this id.
        let candidates = self.operations.languages_for(name, &project.languages);
        let Some(&any) = candidates.first() else {
            return Err(McpError::invalid_params(
                format!("unknown tool or operation `{name}` for project `{project_id}`"),
                None,
            ));
        };
        // Every registration under one id declares the same target shape and
        // reads the same parameters, so any one of them describes the request;
        // the operation that runs it is resolved per language below.
        let described_by = self.operations.resolve(name, any).map_err(into_mcp)?;
        let descriptor = described_by.descriptor();

        let mut target = ops::parse_target(&args, descriptor.target)?;
        remap_target_file(&self.path_map, &mut target);
        let params = ops::operation_params(&args);
        let languages = dispatch_languages(
            &candidates,
            &self.providers,
            &descriptor,
            &target,
            described_by.route(&params),
        )?;

        // Resolve and validate the working copy the edits should land in.
        let workspace = resolve_workspace(&args, &target, &project, &self.path_map);
        ensure_same_repo(&workspace, &project.root)?;

        // If the caller supplied an `expect` guard, verify the coordinate
        // resolves to the intended symbol in Henka's own copy before acting,
        // turning a silent mis-target into an actionable error.
        validate_expectation(&args, &target, &workspace)?;

        // Only a read-only, project-scoped query is served by several
        // languages; an edit resolves to exactly one, so the first edit outcome
        // is the only one.
        let mut queries = Vec::with_capacity(languages.len());
        let mut asked: Vec<Arc<dyn LanguageProvider>> = Vec::new();
        for language in languages {
            let provider = self.provider_for(language)?;
            // One provider can serve several of the project's languages — one
            // TypeScript server answers for JavaScript too — and its single
            // session covers all of them at once. Asking it per language would
            // run the same query twice and count every match twice.
            if asked.iter().any(|asked| Arc::ptr_eq(asked, &provider)) {
                continue;
            }
            asked.push(Arc::clone(&provider));

            let operation = self.operations.resolve(name, language).map_err(into_mcp)?;
            let (session, guard, outcome) = self
                .run_operation(operation, &provider, &project, &target, &params, &workspace)
                .await?;
            match outcome {
                OperationOutcome::Query(value) => queries.push(value),
                OperationOutcome::Edit(edit) => {
                    // Hold the request guard through the edit's application and
                    // the index sync that follows it, so a concurrent request
                    // can't reach the session while the coordinates it would
                    // resolve against are still changing.
                    let finished = self.finish_edit(edit, &session, &args, &workspace).await;
                    drop(guard);
                    return finished;
                }
            }
        }
        let limit = ops::result_limit(&params, &descriptor.params_schema);
        ok_json(&ops::merge_query_results(queries, limit))
    }

    /// The provider registered for `language`.
    fn provider_for(&self, language: Language) -> Result<Arc<dyn LanguageProvider>, McpError> {
        self.providers.get(language).ok_or_else(|| {
            McpError::internal_error(format!("no provider registered for `{language}`"), None)
        })
    }

    /// Run `operation` on the session `provider` holds for `project`, with
    /// `workspace`'s content overlaid on that session's index, returning the
    /// session and the still-held request guard alongside the outcome.
    async fn run_operation(
        &self,
        operation: Arc<dyn Operation>,
        provider: &Arc<dyn LanguageProvider>,
        project: &Project,
        target: &Target,
        params: &Value,
        workspace: &Path,
    ) -> Result<(Arc<dyn LanguageSession>, RequestGuard, OperationOutcome), McpError> {
        let session = provider.session(project).await.map_err(into_mcp)?;
        // Serialize the request and overlay the working copy's content onto the
        // shared index, so the operation sees that working copy. The overlay is
        // restored before returning; the guard travels back to the caller, which
        // holds it until any edit the operation produced has been applied.
        let guard = session.begin_request().await;
        let on_base = session.root() == Some(workspace);
        if !on_base {
            let delta = working_copy_delta(workspace);
            session
                .overlay_workspace(workspace, &delta)
                .await
                .map_err(into_mcp)?;
        }

        let ctx = OperationCtx {
            project,
            session: Arc::clone(&session),
        };
        let req = OperationRequest {
            target: target.clone(),
            params: params.clone(),
        };
        let outcome = operation.run(&ctx, &req).await;

        // Always restore the base index view, even if the operation failed.
        session.restore_overlay().await;
        Ok((session, guard, outcome.map_err(into_mcp)?))
    }

    /// Preview or apply an edit an operation produced, reporting the working
    /// copy it landed in.
    async fn finish_edit(
        &self,
        mut edit: WorkspaceEdit,
        session: &Arc<dyn LanguageSession>,
        args: &JsonObject,
        workspace: &Path,
    ) -> Result<CallToolResult, McpError> {
        // Retarget the edit (computed against the session's checkout) onto the
        // requested working copy, then refuse to escape it.
        if let Some(root) = session.root() {
            edit.retarget(root, workspace);
        }
        reject_edits_outside(&edit, workspace)?;
        // Echo the working copy the edit was resolved against, and the revision
        // it holds, so a caller can confirm the server acted on its own tree —
        // even when the two see it under different mount paths — instead of
        // inferring it from the registered root.
        let acted_on = json!({
            "workspace": workspace,
            "revision": detect_revision(workspace).map(|r| r.id),
        });
        if ops::dry_run(args) {
            let files = EditApplier::preview(&edit, workspace).map_err(into_mcp)?;
            ok_json(&json!({ "dry_run": true, "workspace": acted_on, "files": files }))
        } else {
            let applied = EditApplier::apply(&edit, workspace).map_err(into_mcp)?;
            // When editing the session's own checkout, keep its view current so
            // later operations see the applied changes.
            if session.root() == Some(workspace) {
                session.sync_changed(&applied.changed_files).await;
            }
            ok_json(&json!({ "dry_run": false, "workspace": acted_on, "applied": applied }))
        }
    }
}

/// The languages that should serve a request.
///
/// The target file's language when it names one, else the language the
/// operation reads out of its own parameters, resolved to the candidate whose
/// backend serves it. Failing both, a project-scoped query names no file at all
/// and every candidate language can hold part of the answer, so all of them
/// serve it; anything else falls back to the first, as before.
///
/// A request that does name where it belongs and cannot be placed there is
/// refused rather than broadened: fanning a handle out to backends that did not
/// issue it invites an unrelated backend's error to bury the right answer.
fn dispatch_languages(
    candidates: &[Language],
    providers: &ProviderRegistry,
    descriptor: &OperationDescriptor,
    target: &Target,
    route: LanguageRoute,
) -> Result<Vec<Language>, McpError> {
    let named = target
        .file()
        .and_then(|file| Language::from_path(file))
        .or_else(|| route.language());
    if let Some(language) = named {
        let Some(candidate) = serving_candidate(candidates, providers, language) else {
            return Err(McpError::invalid_params(
                format!(
                    "`{}` is not served for `{language}` in this project",
                    descriptor.id
                ),
                None,
            ));
        };
        return Ok(vec![candidate]);
    }
    if let LanguageRoute::Unplaceable(handle) = route {
        return Err(McpError::invalid_params(
            format!(
                "cannot tell which language `{handle}` belongs to, so `{}` has nowhere to run;                  pass a handle this project's servers issued",
                descriptor.id
            ),
            None,
        ));
    }
    if descriptor.target == TargetKind::Project && descriptor.kind == OperationKind::Query {
        return Ok(candidates.to_vec());
    }
    Ok(candidates.first().copied().into_iter().collect())
}

/// The candidate language whose backend serves `language`.
///
/// `language` itself when it is one of them, else the candidate sharing its
/// provider: one backend can serve several languages — one TypeScript server
/// answers for JavaScript too — so a JavaScript file in a project whose only
/// detected languages are Java and TypeScript still has somewhere to go.
fn serving_candidate(
    candidates: &[Language],
    providers: &ProviderRegistry,
    language: Language,
) -> Option<Language> {
    if candidates.contains(&language) {
        return Some(language);
    }
    let serving = providers.get(language)?;
    candidates.iter().copied().find(|&candidate| {
        providers
            .get(candidate)
            .is_some_and(|provider| Arc::ptr_eq(&provider, &serving))
    })
}

/// Resolve the working copy a request's edits should be applied to: an explicit
/// `workspace`, else the working copy containing an absolute target `file`, else
/// the project root. Caller-supplied paths are translated through `map`.
fn resolve_workspace(
    args: &JsonObject,
    target: &Target,
    project: &Project,
    map: &PathMap,
) -> PathBuf {
    if let Some(ws) = ops::workspace(args) {
        return map.map(&ws);
    }
    if let Some(file) = target.file()
        && file.is_absolute()
        && let Some(root) = working_copy_root_containing(file)
    {
        return root;
    }
    project.root.clone()
}

/// Build the error for a `register_project` whose root does not exist inside
/// Henka, explaining the container/filesystem boundary and offering any mounted
/// working copies whose name matches.
fn path_not_found_error(missing: &Path, suggestions: &[PathBuf]) -> McpError {
    let mut msg = format!(
        "path does not exist inside Henka: `{}`. Henka resolves paths on its own filesystem, not \
         the caller's — when it runs in a container, host paths must be mounted (by convention \
         under /workspaces) and registered by their in-container path.",
        missing.display()
    );
    if !suggestions.is_empty() {
        let list = suggestions
            .iter()
            .map(|p| format!("`{}`", p.display()))
            .collect::<Vec<_>>()
            .join(", ");
        msg.push_str(&format!(" A mounted working copy of the same name exists at: {list}."));
    }
    msg.push_str(
        " To keep registering by caller-side paths, set HENKA_PATH_MAP=<host-prefix>=<container-prefix>.",
    );
    McpError::invalid_params(msg, None)
}

/// Look for directories Henka can see whose name matches the missing path's,
/// to suggest as the intended in-container target. Scans the conventional
/// `/workspaces` mount and the parent of every already-registered project.
fn suggest_mounts(missing: &Path, registry: &ProjectRegistry) -> Vec<PathBuf> {
    let Some(name) = missing.file_name() else {
        return Vec::new();
    };

    let mut roots: Vec<PathBuf> = Vec::new();
    let mount = Path::new("/workspaces");
    if mount.is_dir() {
        roots.push(mount.to_path_buf());
    }
    for project in registry.list() {
        if let Some(parent) = project.root.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    roots.sort();
    roots.dedup();

    let mut out: Vec<PathBuf> = roots
        .into_iter()
        .map(|root| root.join(name))
        .filter(|candidate| candidate.is_dir() && candidate != missing)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Translate an absolute target `file` through the path map, so a caller can
/// name a file by a path Henka does not share verbatim. Relative paths resolve
/// against the project root and are left alone.
fn remap_target_file(map: &PathMap, target: &mut Target) {
    let file = match target {
        Target::Position { file, .. }
        | Target::Selection { file, .. }
        | Target::File { file } => file,
        Target::Project => return,
    };
    if file.is_absolute() {
        *file = map.map(file);
    }
}

/// The nearest ancestor directory of `file` that is a working-copy root (one
/// holding a `.git` or `.jj` entry).
fn working_copy_root_containing(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|dir| dir.join(".git").exists() || dir.join(".jj").exists())
        .map(Path::to_path_buf)
}

/// Validate that `workspace` is a working copy of the same repository as
/// `project_root` (or, with no VCS, is the project root itself).
fn ensure_same_repo(workspace: &Path, project_root: &Path) -> Result<(), McpError> {
    let same = match (repo_identity(workspace), repo_identity(project_root)) {
        (Some(a), Some(b)) => a == b,
        (None, None) => canonical(workspace) == canonical(project_root),
        _ => false,
    };
    if same {
        Ok(())
    } else {
        Err(McpError::invalid_params(
            format!(
                "workspace `{}` is not a working copy of the project",
                workspace.display()
            ),
            None,
        ))
    }
}

/// Canonicalize a path, falling back to the path itself when it can't be.
fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Enforce the optional `expect` guard: that the identifier (for a position) or
/// the selected text (for a selection) Henka reads at the target in `workspace`
/// matches what the caller said it should be.
///
/// This is the guard against coordinate/revision drift: a coordinate computed
/// against a different checkout can land on a neighboring token in Henka's copy
/// and otherwise produce a confident, wrong result. With no `expect`, behavior
/// is unchanged.
fn validate_expectation(
    args: &JsonObject,
    target: &Target,
    workspace: &Path,
) -> Result<(), McpError> {
    let Some(expected) = ops::expect(args) else {
        return Ok(());
    };
    // UTF-16 coordinates, matching the schema and the LSP backends.
    let enc = henka_core::PositionEncoding::Utf16;
    let (file, found, what) = match target {
        Target::Position { file, position } => {
            let content = read_for_validation(workspace, file)?;
            (
                file,
                henka_core::identifier_at(&content, *position, enc),
                "identifier",
            )
        }
        Target::Selection { file, range } => {
            let content = read_for_validation(workspace, file)?;
            (
                file,
                henka_core::text_in_range(&content, *range, enc),
                "selection",
            )
        }
        // Whole-file and project targets carry no coordinate to validate.
        Target::File { .. } | Target::Project => return Ok(()),
    };

    if found.as_deref() == Some(expected.as_str()) {
        return Ok(());
    }

    let saw = match &found {
        Some(text) => format!("`{text}`"),
        None => format!("no {what}"),
    };
    let at = position_label(target);
    let here = match detect_revision(workspace) {
        Some(rev) => {
            let branch = rev.branch.map(|b| format!(", {b}")).unwrap_or_default();
            format!(" ({} {}{branch})", rev.vcs, rev.id)
        }
        None => String::new(),
    };
    Err(McpError::invalid_params(
        format!(
            "target validation failed: expected {what} `{expected}` at {}{at}, but Henka's copy \
             has {saw} there. Henka is reading `{}`{here}. The coordinate may have been computed \
             against a different revision or checkout — call project_status to compare, or pass \
             the matching `workspace`.",
            file.display(),
            workspace.display(),
        ),
        None,
    ))
}

/// Read the target file as Henka sees it in `workspace`, for validation. A
/// missing file is itself surfaced (it usually means the path doesn't resolve
/// the way the caller expects).
fn read_for_validation(workspace: &Path, file: &Path) -> Result<String, McpError> {
    let abs = if file.is_absolute() {
        file.to_path_buf()
    } else {
        workspace.join(file)
    };
    std::fs::read_to_string(&abs).map_err(|e| {
        McpError::invalid_params(
            format!(
                "cannot read `{}` to validate the target: {e}",
                abs.display()
            ),
            None,
        )
    })
}

/// A `:line:character` suffix for a position/selection target, for error text.
fn position_label(target: &Target) -> String {
    match target {
        Target::Position { position, .. } => {
            format!(":{}:{}", position.line, position.character)
        }
        Target::Selection { range, .. } => format!(
            ":{}:{}-{}:{}",
            range.start.line, range.start.character, range.end.line, range.end.character
        ),
        _ => String::new(),
    }
}

/// Reject an edit that, after retargeting, would write to an absolute path
/// outside `workspace` (e.g. a backend emitting edits to dependency sources).
fn reject_edits_outside(edit: &WorkspaceEdit, workspace: &Path) -> Result<(), McpError> {
    let inside = |p: &Path| !p.is_absolute() || p.starts_with(workspace);
    let ok = edit.files.iter().all(|f| inside(&f.path))
        && edit.file_ops.iter().all(|op| match op {
            FileOperation::Create { path } | FileOperation::Delete { path } => inside(path),
            FileOperation::Rename { from, to } => inside(from) && inside(to),
        });
    if ok {
        Ok(())
    } else {
        Err(McpError::invalid_params(
            "refactoring would edit files outside the workspace",
            None,
        ))
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
            "Multi-tenant server for semantics-aware code operations — rename, extract, inline, \
             change-signature, organize-imports, structural replace, and semantic queries \
             (find-usages, go-to-definition, hierarchies, symbol search). Reach for these first: \
             prefer them over hand-edits and text search, since they update every reference \
             through the compiler's view, across files and overloads. One server hosts many \
             projects; pass the project `id` on every operation. Call `list_projects` to find \
             one — working copies under the workspaces mount are auto-registered; otherwise \
             `register_project` a source tree (safe, persistent, non-destructive — just do it, \
             don't ask). Prefer paths relative to the project root — they work regardless of how \
             the server mounts the tree. The server may see your checkout under a different path \
             than you do (it often runs in its own container); don't treat a differing root as a \
             separate checkout. To confirm it is your tree, call `project_status` and check its \
             `revision` and `digest` match yours — not the path. If they differ, target an \
             explicit `workspace` or guard positions with `expect`. Use `list_operations` to see \
             what a project supports; edits default to a preview (a diff), pass `dry_run: false` \
             to apply. Read the resource `skill://henka/refactoring` for the full workflow."
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
    use std::sync::atomic::{AtomicBool, Ordering};

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

    /// Reports which language's provider ran it, so a test can see where
    /// dispatch sent a request. `hinted` reads the language out of the `item`
    /// parameter, the way a call-hierarchy operation does.
    struct WhichLanguage {
        id: &'static str,
        language: Language,
        target: TargetKind,
        hinted: bool,
    }

    #[async_trait]
    impl Operation for WhichLanguage {
        fn descriptor(&self) -> OperationDescriptor {
            OperationDescriptor {
                id: self.id.into(),
                title: "Which language".into(),
                description: "Report the language that ran this".into(),
                kind: OperationKind::Query,
                languages: vec![self.language],
                target: self.target,
                params_schema: json!({ "type": "object", "properties": {} }),
            }
        }

        async fn run(
            &self,
            _ctx: &OperationCtx<'_>,
            _req: &OperationRequest,
        ) -> Result<OperationOutcome> {
            Ok(OperationOutcome::Query(
                json!({ "count": 1, "languages": [self.language.as_str()] }),
            ))
        }

        fn route(&self, params: &Value) -> LanguageRoute {
            if !self.hinted {
                return LanguageRoute::Unspecified;
            }
            let Some(uri) = params.pointer("/item/uri").and_then(Value::as_str) else {
                return LanguageRoute::Unspecified;
            };
            match Language::from_path(Path::new(uri.trim_start_matches("file://"))) {
                Some(language) => LanguageRoute::Language(language),
                None => LanguageRoute::Unplaceable(uri.into()),
            }
        }
    }

    /// Stands in for `symbol-search`: a project-scoped query reporting one
    /// match per backend, named after the backend, capped at the caller's
    /// `limit` the way the real operations cap their own answers.
    struct SymbolSearchMock {
        languages: Vec<Language>,
        name: &'static str,
    }

    #[async_trait]
    impl Operation for SymbolSearchMock {
        fn descriptor(&self) -> OperationDescriptor {
            OperationDescriptor {
                id: "symbols".into(),
                title: "Symbols".into(),
                description: "Search the project's symbols".into(),
                kind: OperationKind::Query,
                languages: self.languages.clone(),
                target: TargetKind::Project,
                params_schema: json!({
                    "type": "object",
                    "properties": { "limit": { "type": "integer", "default": 200 } }
                }),
            }
        }

        async fn run(
            &self,
            _ctx: &OperationCtx<'_>,
            req: &OperationRequest,
        ) -> Result<OperationOutcome> {
            let limit = req.params.get("limit").and_then(Value::as_u64).map_or(200, |n| n as usize);
            let symbols: Vec<Value> =
                std::iter::once(json!({ "name": self.name })).take(limit).collect();
            Ok(OperationOutcome::Query(json!({ "count": 1, "symbols": symbols })))
        }
    }

    struct SymbolProvider {
        language: Language,
        languages: Vec<Language>,
        name: &'static str,
    }

    #[async_trait]
    impl LanguageProvider for SymbolProvider {
        fn language(&self) -> Language {
            self.language
        }
        fn operations(&self) -> Vec<Arc<dyn Operation>> {
            vec![Arc::new(SymbolSearchMock {
                languages: self.languages.clone(),
                name: self.name,
            })]
        }
        async fn session(&self, _project: &Project) -> Result<Arc<dyn LanguageSession>> {
            Ok(Arc::new(WhichSession(self.language)))
        }
    }

    /// A session that serializes requests behind a mutex and records, from
    /// inside `sync_changed`, whether that mutex was still held — that is,
    /// whether the request guard outlived the edit's application.
    struct LockingSession {
        root: PathBuf,
        lock: Arc<tokio::sync::Mutex<()>>,
        held_during_sync: Arc<AtomicBool>,
    }

    #[async_trait]
    impl LanguageSession for LockingSession {
        fn language(&self) -> Language {
            Language::Java
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn root(&self) -> Option<&Path> {
            Some(&self.root)
        }
        async fn begin_request(&self) -> RequestGuard {
            RequestGuard::holding(Box::new(Arc::clone(&self.lock).lock_owned().await))
        }
        async fn sync_changed(&self, _changed: &[PathBuf]) {
            self.held_during_sync.store(self.lock.try_lock().is_err(), Ordering::SeqCst);
        }
    }

    struct LockingProvider(Arc<LockingSession>);

    #[async_trait]
    impl LanguageProvider for LockingProvider {
        fn language(&self) -> Language {
            Language::Java
        }
        fn operations(&self) -> Vec<Arc<dyn Operation>> {
            vec![Arc::new(InsertOp)]
        }
        async fn session(&self, _project: &Project) -> Result<Arc<dyn LanguageSession>> {
            Ok(Arc::clone(&self.0) as Arc<dyn LanguageSession>)
        }
    }

    struct WhichProvider(Language);
    #[async_trait]
    impl LanguageProvider for WhichProvider {
        fn language(&self) -> Language {
            self.0
        }
        fn operations(&self) -> Vec<Arc<dyn Operation>> {
            vec![
                Arc::new(WhichLanguage {
                    id: "which",
                    language: self.0,
                    target: TargetKind::Position,
                    hinted: false,
                }),
                Arc::new(WhichLanguage {
                    id: "which-project",
                    language: self.0,
                    target: TargetKind::Project,
                    hinted: false,
                }),
                Arc::new(WhichLanguage {
                    id: "which-item",
                    language: self.0,
                    target: TargetKind::Project,
                    hinted: true,
                }),
            ]
        }
        async fn session(&self, _project: &Project) -> Result<Arc<dyn LanguageSession>> {
            Ok(Arc::new(WhichSession(self.0)))
        }
    }

    struct WhichSession(Language);
    impl LanguageSession for WhichSession {
        fn language(&self) -> Language {
            self.0
        }
        fn as_any(&self) -> &dyn Any {
            self
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
        // The response echoes the working copy it resolved the edit against, so a
        // caller can confirm its own tree was the target regardless of the path.
        let echoed = |res: &CallToolResult| -> Value {
            let text = match &res.content[0].raw {
                rmcp::model::RawContent::Text(t) => t.text.clone(),
                _ => panic!("expected text content"),
            };
            serde_json::from_str::<Value>(&text).unwrap()["workspace"]["workspace"].clone()
        };
        assert_eq!(echoed(&preview).as_str(), root.to_str());

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
        assert_eq!(echoed(&applied).as_str(), root.to_str());
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
    async fn workspace_is_stripped_from_operation_params() {
        // The `workspace` envelope field must not leak into operation params.
        let params = ops::operation_params(
            &args(json!({ "project": "p", "workspace": "/some/wt", "file": "A.java", "text": "x" }))
                .unwrap(),
        );
        assert!(params.get("workspace").is_none());
        assert_eq!(params.get("text").and_then(Value::as_str), Some("x"));
    }

    #[tokio::test]
    async fn rejects_workspace_outside_project_repo() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, _) = handler_with_project(dir.path());
        // An unrelated directory is not a working copy of the project.
        let foreign = dir.path().join("foreign");
        std::fs::create_dir_all(&foreign).unwrap();

        let err = mcp
            .handle_call(call(
                "insert-text",
                args(json!({
                    "project": "p", "workspace": foreign.to_str().unwrap(),
                    "file": "Main.java", "line": 0, "character": 0, "text": "X"
                })),
            ))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not a working copy"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn applies_edit_to_named_workspace() {
        // An explicit `workspace` equal to the project root is accepted and the
        // edit lands there. (Cross-working-copy retargeting needs a real repo
        // and is covered by the jdtls integration tests.)
        let dir = tempfile::tempdir().unwrap();
        let (mcp, root) = handler_with_project(dir.path());
        let main = root.join("Main.java");

        let applied = mcp
            .handle_call(call(
                "insert-text",
                args(json!({
                    "project": "p", "workspace": root.to_str().unwrap(),
                    "file": "Main.java", "line": 0, "character": 0, "text": "X", "dry_run": false
                })),
            ))
            .await
            .unwrap();
        assert_ne!(applied.is_error, Some(true));
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "Xhello\n");
    }

    #[tokio::test]
    async fn expect_guard_blocks_mismatched_coordinate() {
        // Main.java is "hello\n"; the identifier at 0:0 is `hello`.
        let dir = tempfile::tempdir().unwrap();
        let (mcp, _) = handler_with_project(dir.path());

        // A matching expectation passes through to the operation.
        let ok = mcp
            .handle_call(call(
                "insert-text",
                args(json!({
                    "project": "p", "file": "Main.java", "line": 0, "character": 0,
                    "expect": "hello", "text": "X"
                })),
            ))
            .await
            .unwrap();
        assert_ne!(ok.is_error, Some(true));

        // A wrong expectation fails loudly, naming what Henka actually saw.
        let err = mcp
            .handle_call(call(
                "insert-text",
                args(json!({
                    "project": "p", "file": "Main.java", "line": 0, "character": 0,
                    "expect": "goodbye", "text": "X"
                })),
            ))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("target validation failed")
                && err.message.contains("`goodbye`")
                && err.message.contains("`hello`"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn project_status_reports_vcs_state() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, root) = handler_with_project(dir.path());

        // Make the project root a git working copy with one commit.
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return; // git unavailable
        }
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        let res = mcp
            .handle_call(call("project_status", args(json!({ "id": "p" }))))
            .await
            .unwrap();
        let text = match &res.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        let vcs = value.get("vcs").expect("vcs field present");
        assert_eq!(vcs.get("vcs").and_then(Value::as_str), Some("git"));
        assert!(
            vcs.get("revision").and_then(Value::as_str).is_some(),
            "revision reported: {value}"
        );
        // A fresh commit with no further edits is clean: no changed files, no digest.
        assert_eq!(vcs.get("dirty").and_then(Value::as_bool), Some(false));
        assert_eq!(
            vcs.get("changed_files").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
        assert!(vcs.get("digest").is_none(), "clean tree has no digest: {value}");
        // The base project fields are still present (flattened).
        assert_eq!(value.get("id").and_then(Value::as_str), Some("p"));

        // Now dirty the tree: status reports the changed file and a digest a
        // caller can reproduce in its own checkout to confirm the same content.
        std::fs::write(root.join("Main.java"), "changed\n").unwrap();
        let res = mcp
            .handle_call(call("project_status", args(json!({ "id": "p" }))))
            .await
            .unwrap();
        let text = match &res.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        let vcs = value.get("vcs").expect("vcs field present");
        assert_eq!(vcs.get("dirty").and_then(Value::as_bool), Some(true));
        let changed = vcs.get("changed_files").and_then(Value::as_array).unwrap();
        let main = changed
            .iter()
            .find(|f| f["path"].as_str() == Some("Main.java"))
            .unwrap_or_else(|| panic!("changed_files lists the edit: {value}"));
        // Each changed file carries its git blob object id (git is present in CI).
        assert!(
            main["oid"].as_str().is_some(),
            "changed file reports a git blob oid: {value}"
        );
        assert!(
            vcs.get("digest").and_then(Value::as_str).is_some(),
            "dirty tree reports a digest: {value}"
        );
    }

    #[tokio::test]
    async fn project_status_reports_host_path_under_mount() {
        // With a path map active, status reverse-maps the in-container root back
        // to the caller's own path, so a caller can confirm it's their copy.
        let dir = tempfile::tempdir().unwrap();
        let (mut mcp, root) = handler_with_project(dir.path());
        let canon_root = root.canonicalize().unwrap();
        let parent = canon_root.parent().unwrap();
        mcp.path_map = Arc::new(PathMap::parse(&format!("/host/src={}", parent.display())));

        let res = mcp
            .handle_call(call("project_status", args(json!({ "id": "p" }))))
            .await
            .unwrap();
        let text = match &res.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value.get("host_path").and_then(Value::as_str),
            Some("/host/src/proj")
        );
    }

    #[tokio::test]
    async fn register_project_suggests_matching_mount() {
        // handler_with_project registers `p` at <tmp>/proj, so <tmp> is scanned.
        let dir = tempfile::tempdir().unwrap();
        let (mcp, _) = handler_with_project(dir.path());

        // A sibling working copy Henka can see, with the wanted name.
        let wanted = dir.path().join("wanted");
        std::fs::create_dir_all(&wanted).unwrap();
        std::fs::write(wanted.join("pom.xml"), "<project/>").unwrap();

        // Register a non-existent path whose basename matches that sibling.
        let err = mcp
            .handle_call(call(
                "register_project",
                args(json!({ "root": "/no/such/place/wanted", "id": "w" })),
            ))
            .await
            .unwrap_err();

        assert!(err.message.contains("does not exist inside Henka"), "got: {}", err.message);
        assert!(
            err.message.contains(&wanted.display().to_string()),
            "expected suggestion of {}, got: {}",
            wanted.display(),
            err.message
        );
        assert!(err.message.contains("HENKA_PATH_MAP"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn register_project_translates_host_path() {
        // A caller speaks a host path; the path map rewrites its prefix onto a
        // location Henka can actually see, and registration succeeds there.
        let dir = tempfile::tempdir().unwrap();
        let (mut mcp, _) = handler_with_project(dir.path());

        let container = dir.path().join("container");
        let proj = container.join("svc");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("pom.xml"), "<project/>").unwrap();

        mcp.path_map = Arc::new(PathMap::parse(&format!(
            "/virtual/host={}",
            container.display()
        )));

        let res = mcp
            .handle_call(call(
                "register_project",
                args(json!({ "root": "/virtual/host/svc", "id": "svc" })),
            ))
            .await
            .unwrap();
        assert_ne!(res.is_error, Some(true));

        // The response explains the rewrite: the caller's path, the path Henka
        // reads, and that they are the same tree.
        let text = match &res.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        let translation = value.get("translation").expect("translation reported");
        assert_eq!(
            translation.get("from").and_then(Value::as_str),
            Some("/virtual/host/svc")
        );
        assert!(
            translation
                .get("note")
                .and_then(Value::as_str)
                .is_some_and(|n| n.contains("same tree")),
            "note reassures same tree: {translation}"
        );

        let reg = mcp.registry.read().await;
        assert_eq!(reg.get("svc").unwrap().root, proj.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn list_projects_auto_registers_under_workspace_roots() {
        // A workspaces root with a child project Henka has never been told
        // about: listing projects scans the root and surfaces it.
        let dir = tempfile::tempdir().unwrap();
        let (mut mcp, _) = handler_with_project(dir.path());

        let workspaces = dir.path().join("workspaces");
        let child = workspaces.join("svc-x");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("pom.xml"), "<project/>").unwrap();
        mcp.auto_register_roots = Arc::new(vec![workspaces]);

        let res = mcp
            .handle_call(call("list_projects", args(json!({}))))
            .await
            .unwrap();
        let text = match &res.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            text.contains("svc-x"),
            "expected auto-registered project in listing, got: {text}"
        );
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

    /// Build a handler over a project holding both Java and TypeScript sources,
    /// with a provider registered for each — the mixed-language project where
    /// dispatch has to pick.
    fn mixed_handler(dir: &Path) -> (HenkaMcp, std::path::PathBuf) {
        let cfg = dir.join("projects.toml");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pom.xml"), "<project/>").unwrap();
        std::fs::write(root.join("Main.java"), "hello\n").unwrap();
        std::fs::write(root.join("app.ts"), "export {};\n").unwrap();

        let mut registry = ProjectRegistry::load(&cfg).unwrap();
        let project = registry.register(Some("p".into()), &root).unwrap().clone();
        assert!(project.has_language(Language::Java));
        assert!(project.has_language(Language::TypeScript));

        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(WhichProvider(Language::Java)));
        providers.register(Arc::new(WhichProvider(Language::TypeScript)));

        (HenkaMcp::new(registry, providers), root)
    }

    /// The `languages` list a `WhichLanguage` result reports.
    fn languages_in(result: &CallToolResult) -> Vec<String> {
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        serde_json::from_str::<Value>(&text).unwrap()["languages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn file_targeted_operation_runs_on_its_own_languages_provider() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, _) = mixed_handler(dir.path());

        for (file, language) in [("app.ts", "typescript"), ("Main.java", "java")] {
            let result = mcp
                .handle_call(call(
                    "which",
                    args(json!({ "project": "p", "file": file, "line": 0, "character": 0 })),
                ))
                .await
                .unwrap();
            assert_ne!(result.is_error, Some(true));
            assert_eq!(languages_in(&result), vec![language.to_string()]);
        }
    }

    #[tokio::test]
    async fn project_scoped_query_is_answered_by_every_language() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, _) = mixed_handler(dir.path());

        let result = mcp
            .handle_call(call("which-project", args(json!({ "project": "p" }))))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert_eq!(languages_in(&result), vec!["java", "typescript"]);
    }

    #[tokio::test]
    async fn project_scoped_query_follows_the_language_named_in_its_params() {
        let dir = tempfile::tempdir().unwrap();
        let (mcp, root) = mixed_handler(dir.path());

        let uri = format!("file://{}", root.join("app.ts").display());
        let result = mcp
            .handle_call(call(
                "which-item",
                args(json!({ "project": "p", "item": { "uri": uri } })),
            ))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert_eq!(languages_in(&result), vec!["typescript"]);
    }
    /// Build a handler over a project holding `files`, served by `providers`.
    fn handler_over(
        dir: &Path,
        files: &[(&str, &str)],
        providers: ProviderRegistry,
    ) -> (HenkaMcp, std::path::PathBuf) {
        let cfg = dir.join("projects.toml");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        for (name, content) in files {
            std::fs::write(root.join(name), content).unwrap();
        }
        let mut registry = ProjectRegistry::load(&cfg).unwrap();
        let root = registry.register(Some("p".into()), &root).unwrap().root.clone();
        (HenkaMcp::new(registry, providers), root)
    }

    /// The whole result a `SymbolSearchMock` dispatch produced.
    fn result_json(result: &CallToolResult) -> Value {
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn one_backend_serving_two_languages_answers_a_project_query_once() {
        // A TypeScript project with a JavaScript file in it: both languages are
        // detected, but one backend searches both, so the query must reach it
        // once rather than double every match it reports.
        let dir = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register_for(
            &[Language::TypeScript, Language::JavaScript],
            Arc::new(SymbolProvider {
                language: Language::TypeScript,
                languages: vec![Language::TypeScript, Language::JavaScript],
                name: "typescript",
            }),
        );
        let files = [("app.ts", "export {};\n"), ("rollup.config.js", "\n")];
        let (mcp, _) = handler_over(dir.path(), &files, providers);

        let result = mcp
            .handle_call(call("symbols", args(json!({ "project": "p" }))))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            result_json(&result),
            json!({ "count": 1, "symbols": [{ "name": "typescript" }] })
        );
    }

    #[tokio::test]
    async fn a_merged_query_respects_the_limit_the_caller_asked_for() {
        // Two backends, one match each: `limit: 1` caps neither on its own, so
        // the cap has to reach the merged list — with `count` still reporting
        // both matches.
        let dir = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(SymbolProvider {
            language: Language::Java,
            languages: vec![Language::Java],
            name: "java",
        }));
        providers.register(Arc::new(SymbolProvider {
            language: Language::TypeScript,
            languages: vec![Language::TypeScript],
            name: "typescript",
        }));
        let files = [("Main.java", "hello\n"), ("app.ts", "export {};\n")];
        let (mcp, _) = handler_over(dir.path(), &files, providers);

        let result = mcp
            .handle_call(call("symbols", args(json!({ "project": "p", "limit": 1 }))))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            result_json(&result),
            json!({ "count": 2, "symbols": [{ "name": "java" }], "truncated": true })
        );
    }

    #[tokio::test]
    async fn the_request_guard_outlives_the_edit_and_its_index_sync() {
        // The guard serializes requests against the shared session; releasing it
        // before the edit is applied and synced would let the next request
        // resolve coordinates this one is still changing.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let held = Arc::new(AtomicBool::new(false));
        let session = Arc::new(LockingSession {
            root: root.canonicalize().unwrap_or(root),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            held_during_sync: Arc::clone(&held),
        });
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(LockingProvider(Arc::clone(&session))));
        let (mcp, project_root) =
            handler_over(dir.path(), &[("Main.java", "hello\n")], providers);
        // The session has to be rooted at the edited working copy, or dispatch
        // never reaches the sync this is about.
        assert_eq!(session.root(), Some(project_root.as_path()));

        let applied = mcp
            .handle_call(call(
                "insert-text",
                args(json!({
                    "project": "p", "file": "Main.java", "line": 0, "character": 0,
                    "text": "X", "dry_run": false
                })),
            ))
            .await
            .unwrap();
        assert_ne!(applied.is_error, Some(true));
        assert!(
            held.load(Ordering::SeqCst),
            "the request guard was released before the index sync"
        );
    }
    #[tokio::test]
    async fn a_handle_from_a_shared_backend_reaches_that_backend() {
        // A Java/TypeScript project with no JavaScript of its own: a JavaScript
        // handle still belongs to the TypeScript server, which serves both, so
        // it must go there rather than to every language in the project.
        let dir = tempfile::tempdir().unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(WhichProvider(Language::Java)));
        providers.register_for(
            &[Language::TypeScript, Language::JavaScript],
            Arc::new(WhichProvider(Language::TypeScript)),
        );
        let files = [("Main.java", "hello\n"), ("app.ts", "export {};\n")];
        let (mcp, root) = handler_over(dir.path(), &files, providers);

        let uri = format!("file://{}", root.join("vendor/bundle.js").display());
        let result = mcp
            .handle_call(call(
                "which-item",
                args(json!({ "project": "p", "item": { "uri": uri } })),
            ))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert_eq!(languages_in(&result), vec!["typescript"]);
    }

    #[tokio::test]
    async fn an_unplaceable_handle_is_refused_rather_than_fanned_out() {
        // A handle naming no language Henka serves has no right backend. Asking
        // every language instead would let an unrelated backend's error bury
        // the answer, so the request is refused, naming the handle.
        let dir = tempfile::tempdir().unwrap();
        let (mcp, _) = mixed_handler(dir.path());

        let err = mcp
            .handle_call(call(
                "which-item",
                args(json!({ "project": "p", "item": { "uri": "nowhere://opaque/handle" } })),
            ))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("nowhere://opaque/handle") && err.message.contains("which-item"),
            "got: {}",
            err.message
        );
    }
}
