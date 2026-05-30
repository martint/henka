//! Locating, launching, and driving an Eclipse JDT language server.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use refactor_lsp::LspClient;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Mutex, broadcast};
use tokio::time::Duration;

/// Upper bound on source files opened to index a loose-file project.
const MAX_INDEX_FILES: usize = 2000;

/// Directories not worth opening when indexing a project.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".jj",
    ".data",
    "config",
    "target",
    "build",
    "out",
    "node_modules",
    ".gradle",
];

use crate::error::{JavaError, Result};

/// A located jdtls distribution.
#[derive(Debug, Clone)]
pub struct JdtlsInstall {
    /// Root of the extracted distribution.
    home: PathBuf,
}

impl JdtlsInstall {
    /// Locate a jdtls distribution.
    ///
    /// Searches, in order: `$JDTLS_HOME`, `./.cache/jdtls`, and
    /// `$XDG_CACHE_HOME`/`~/.cache` under `refactor-mcp/jdtls`.
    pub fn locate() -> Result<Self> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(home) = std::env::var_os("JDTLS_HOME") {
            candidates.push(PathBuf::from(home));
        }
        candidates.push(PathBuf::from(".cache/jdtls"));
        candidates.push(cache_base().join("jdtls"));

        for home in &candidates {
            if launcher_in(home).is_some() {
                return Ok(Self { home: home.clone() });
            }
        }
        let looked = candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(JavaError::JdtlsNotFound(looked))
    }

    /// Use the jdtls distribution at a specific path, validating it contains a
    /// launcher jar.
    pub fn at(home: impl Into<PathBuf>) -> Result<Self> {
        let home = home.into();
        if launcher_in(&home).is_some() {
            Ok(Self { home })
        } else {
            Err(JavaError::JdtlsIncomplete(
                home,
                "no plugins/org.eclipse.equinox.launcher_*.jar".into(),
            ))
        }
    }

    /// The OSGi launcher jar.
    pub fn launcher_jar(&self) -> Result<PathBuf> {
        launcher_in(&self.home).ok_or_else(|| {
            JavaError::JdtlsIncomplete(
                self.home.clone(),
                "no plugins/org.eclipse.equinox.launcher_*.jar".into(),
            )
        })
    }

    /// The platform configuration directory for this OS/arch.
    pub fn platform_config(&self) -> Result<PathBuf> {
        let names: &[&str] = if cfg!(target_os = "macos") {
            if cfg!(target_arch = "aarch64") {
                &["config_mac_arm", "config_mac"]
            } else {
                &["config_mac"]
            }
        } else if cfg!(target_os = "windows") {
            &["config_win"]
        } else if cfg!(target_arch = "aarch64") {
            &["config_linux_arm", "config_linux"]
        } else {
            &["config_linux"]
        };
        for name in names {
            let dir = self.home.join(name);
            if dir.is_dir() {
                return Ok(dir);
            }
        }
        Err(JavaError::JdtlsIncomplete(
            self.home.clone(),
            "no platform config_* directory".into(),
        ))
    }
}

/// Find the equinox launcher jar within a jdtls home, if present.
fn launcher_in(home: &Path) -> Option<PathBuf> {
    let plugins = home.join("plugins");
    let entries = std::fs::read_dir(&plugins).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("org.eclipse.equinox.launcher_") && name.ends_with(".jar") {
            return Some(entry.path());
        }
    }
    None
}

/// The base cache directory: `$XDG_CACHE_HOME`/`~/.cache` under `refactor-mcp`.
pub fn cache_base() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("refactor-mcp")
}

/// The Java executable to launch jdtls with (`$JAVA_HOME/bin/java` or `java`).
pub fn java_executable() -> String {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let candidate = PathBuf::from(home).join("bin").join("java");
        if candidate.is_file() {
            return candidate.display().to_string();
        }
    }
    "java".to_string()
}

/// A running, initialized jdtls session for one project.
pub struct JdtlsSession {
    client: LspClient,
    root: PathBuf,
    opened: Mutex<HashSet<PathBuf>>,
    indexed: AtomicBool,
}

impl JdtlsSession {
    /// Launch jdtls for `root`, using `data_dir` as its workspace data
    /// directory, and perform the initialize handshake.
    pub async fn start(install: &JdtlsInstall, root: &Path, data_dir: &Path) -> Result<Self> {
        let launcher = install.launcher_jar()?;
        let platform_config = install.platform_config()?;

        // Give the session its own writable configuration so concurrent
        // projects don't contend over the shared one.
        let config_dir = data_dir.join("config");
        copy_dir(&platform_config, &config_dir)?;
        std::fs::create_dir_all(data_dir)?;

        let mut command = Command::new(java_executable());
        command
            .kill_on_drop(true)
            .arg("-Declipse.application=org.eclipse.jdt.ls.core.id1")
            .arg("-Dosgi.bundles.defaultStartLevel=4")
            .arg("-Declipse.product=org.eclipse.jdt.ls.core.product")
            .arg("-Dlog.level=ALL")
            .arg("-Xmx1G")
            .arg("--add-modules=ALL-SYSTEM")
            .arg("--add-opens")
            .arg("java.base/java.util=ALL-UNNAMED")
            .arg("--add-opens")
            .arg("java.base/java.lang=ALL-UNNAMED")
            .arg("-jar")
            .arg(&launcher)
            .arg("-configuration")
            .arg(&config_dir)
            .arg("-data")
            .arg(data_dir);

        let client = LspClient::spawn(command)?;
        let session = Self {
            client,
            root: root.to_path_buf(),
            opened: Mutex::new(HashSet::new()),
            indexed: AtomicBool::new(false),
        };
        // Subscribe before initializing so the readiness signal isn't missed.
        let mut status = session.client.subscribe();
        session.initialize().await?;
        wait_for_ready(&mut status).await;
        Ok(session)
    }

    /// The underlying LSP client.
    pub fn client(&self) -> &LspClient {
        &self.client
    }

    /// The project root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `file://` URI for a path, resolved against the project root.
    pub fn uri_for(&self, path: &Path) -> String {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        path_to_file_uri(&abs)
    }

    /// Open a document in the server if it isn't already, returning its URI.
    pub async fn ensure_open(&self, path: &Path) -> Result<String> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let uri = path_to_file_uri(&abs);

        {
            let opened = self.opened.lock().await;
            if opened.contains(&abs) {
                return Ok(uri);
            }
        }

        let text = std::fs::read_to_string(&abs)?;
        self.client
            .notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": "java",
                        "version": 1,
                        "text": text,
                    }
                }),
            )
            .await?;
        self.opened.lock().await.insert(abs);
        Ok(uri)
    }

    /// Ensure the project's sources are resolved before an operation reads from
    /// them, done once per session.
    ///
    /// A build-tool project (Maven/Gradle) is fully indexed by jdtls's import,
    /// so this is a safety net; but a loose-file ("invisible") project — or one
    /// whose import failed (e.g. offline) — resolves files only as they are
    /// opened. Opening the project's sources up front makes cross-file results
    /// (rename, find-usages) complete in those cases too.
    pub async fn ensure_indexed(&self) -> Result<()> {
        if self.indexed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let files = collect_java_files(&self.root);
        self.open_and_reconcile(&files).await
    }

    /// Open the given files (if not already open) and wait for jdtls to
    /// reconcile each newly-opened one, signalled by a `publishDiagnostics`
    /// notification. This makes the server resolve those files before a
    /// subsequent request (e.g. a rename) reads from them.
    pub async fn open_and_reconcile(&self, paths: &[PathBuf]) -> Result<()> {
        // Subscribe before opening so reconcile signals aren't missed.
        let mut events = self.client.subscribe();

        let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
        for path in paths {
            let abs = if path.is_absolute() {
                path.clone()
            } else {
                self.root.join(path)
            };
            let already_open = self.opened.lock().await.contains(&abs);
            let uri = self.ensure_open(path).await?;
            if !already_open {
                pending.insert(uri);
            }
        }
        if pending.is_empty() {
            return Ok(());
        }

        let _ = tokio::time::timeout(Duration::from_secs(15), async {
            while !pending.is_empty() {
                match events.recv().await {
                    Ok((method, params)) if method == "textDocument/publishDiagnostics" => {
                        if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                            pending.remove(uri);
                        }
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
        Ok(())
    }

    /// Sync files that changed on disk (after an edit was applied) into the
    /// server: any that were open are closed so jdtls re-reads them from disk,
    /// and a watched-files change is announced so the index updates.
    pub async fn sync_changed_impl(&self, changed: &[PathBuf]) {
        let mut to_close = Vec::new();
        let mut watch = Vec::new();
        {
            let mut opened = self.opened.lock().await;
            for path in changed {
                let abs = if path.is_absolute() {
                    path.clone()
                } else {
                    self.root.join(path)
                };
                let uri = path_to_file_uri(&abs);
                if opened.remove(&abs) {
                    to_close.push(uri.clone());
                }
                watch.push(json!({ "uri": uri, "type": 2 })); // 2 = Changed
            }
        }
        for uri in to_close {
            let _ = self
                .client
                .notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": uri } }),
                )
                .await;
        }
        let _ = self
            .client
            .notify(
                "workspace/didChangeWatchedFiles",
                json!({ "changes": watch }),
            )
            .await;
    }

    /// Shut the server down.
    pub async fn shutdown(&self) -> Result<()> {
        self.client.shutdown().await?;
        Ok(())
    }

    /// Send `initialize`/`initialized`, advertising the JDT extended client
    /// capabilities that unlock its command-based refactorings.
    async fn initialize(&self) -> Result<()> {
        let root_uri = path_to_file_uri(&self.root);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "workspace": {
                    "applyEdit": true,
                    "workspaceEdit": { "documentChanges": true, "resourceOperations": ["create", "rename", "delete"] },
                    "configuration": true,
                    "executeCommand": { "dynamicRegistration": true },
                    "symbol": { "dynamicRegistration": true },
                },
                "textDocument": {
                    "synchronization": { "didSave": true, "dynamicRegistration": true },
                    "rename": { "dynamicRegistration": true, "prepareSupport": true },
                    "references": { "dynamicRegistration": true },
                    "definition": { "dynamicRegistration": true },
                    "implementation": { "dynamicRegistration": true },
                    "documentSymbol": { "dynamicRegistration": true, "hierarchicalDocumentSymbolSupport": true },
                    "callHierarchy": { "dynamicRegistration": true },
                    "typeHierarchy": { "dynamicRegistration": true },
                    "codeAction": {
                        "dynamicRegistration": true,
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": ["", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source", "source.organizeImports"]
                            }
                        },
                        "resolveSupport": { "properties": ["edit"] }
                    }
                }
            },
            // Deliberately do NOT advertise the "advanced" refactoring
            // capabilities (advancedExtractRefactoringSupport, executeClient
            // CommandSupport, …). Those make jdtls delegate refactoring UI to
            // the client via client-side commands; without them, jdtls computes
            // the refactoring itself and returns the edit inline on the code
            // action, which is what a headless client needs.
            "initializationOptions": {
                "extendedClientCapabilities": {
                    "classFileContentsSupport": true
                }
            }
        });

        let _: Value = self.client.request("initialize", params).await?;
        self.client.notify("initialized", json!({})).await?;
        Ok(())
    }
}

/// How long to wait for jdtls to finish importing the project before serving
/// requests. Reaching the deadline is non-fatal — requests may simply see an
/// incomplete index until the import catches up.
const READY_TIMEOUT: Duration = Duration::from_secs(180);

/// Wait until jdtls reports it has finished importing the project, signalled by
/// a `language/status` notification of type `Started`/`ServiceReady`.
async fn wait_for_ready(status: &mut broadcast::Receiver<(String, Value)>) {
    let waited = tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            match status.recv().await {
                Ok((method, params)) if method == "language/status" => {
                    let kind = params
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if kind == "ServiceReady" {
                        return;
                    }
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
    .await;
    if waited.is_err() {
        tracing::warn!("timed out waiting for jdtls to become ready; proceeding");
    }
}

/// Convert an absolute path to a `file://` URI, percent-encoding the few
/// characters that matter for typical source paths.
fn path_to_file_uri(path: &Path) -> String {
    let mut encoded = String::from("file://");
    for ch in path.display().to_string().chars() {
        match ch {
            ' ' => encoded.push_str("%20"),
            '#' => encoded.push_str("%23"),
            '?' => encoded.push_str("%3F"),
            other => encoded.push(other),
        }
    }
    encoded
}

/// Collect up to [`MAX_INDEX_FILES`] `.java` files under `root`, skipping build
/// and VCS directories.
fn collect_java_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_INDEX_FILES {
                return out;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !SKIP_DIRS.contains(&name.as_ref()) {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "java") {
                out.push(path);
            }
        }
    }
    out
}

/// Recursively copy a directory tree.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}
