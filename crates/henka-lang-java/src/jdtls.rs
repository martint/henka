//! Locating, launching, and driving an Eclipse JDT language server.
//!
//! The generic document lifecycle (open/index/overlay/sync) lives in
//! [`henka_lsp::LspSession`]; this module adds the jdtls-specific launch, the
//! `initialize` handshake, and the import-readiness wait.

use std::path::{Path, PathBuf};

use henka_core::provider::RequestGuard;
use henka_lsp::{LspClient, LspSession, path_to_file_uri};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::time::Duration;

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
    /// `$XDG_CACHE_HOME`/`~/.cache` under `henka/jdtls`.
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

/// The base cache directory: `$XDG_CACHE_HOME`/`~/.cache` under `henka`.
pub fn cache_base() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("henka")
}

/// Locate the refactoring delegate-command bundle jar, if built.
///
/// Searches `$HENKA_JDTLS_BUNDLE`, then `jdtls-bundle/` relative to the
/// working directory, then the cache dir. Returns `None` if not found, in which
/// case the parameterized refactorings (change-signature, move) are unavailable.
pub fn locate_bundle() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HENKA_JDTLS_BUNDLE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    [
        PathBuf::from("jdtls-bundle/henka-jdtls-bundle.jar"),
        cache_base().join("henka-jdtls-bundle.jar"),
    ]
    .into_iter()
    .find(|p| p.is_file())
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

/// A running, initialized jdtls session for one project, wrapping the generic
/// [`LspSession`] with jdtls-specific startup.
pub struct JdtlsSession {
    session: LspSession,
}

impl JdtlsSession {
    /// Launch jdtls for `root`, using `data_dir` as its workspace data
    /// directory, loading the given extension `bundles` (jar paths), and
    /// perform the initialize handshake.
    pub async fn start(
        install: &JdtlsInstall,
        root: &Path,
        data_dir: &Path,
        bundles: &[PathBuf],
    ) -> Result<Self> {
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
        // Subscribe before initializing so the readiness signal isn't missed.
        let mut status = client.subscribe();
        let bundles: Vec<String> = bundles.iter().map(|p| p.display().to_string()).collect();
        initialize(&client, root, &bundles).await?;
        wait_for_ready(&mut status).await;
        Ok(Self {
            session: LspSession::new(client, root, "java", &["java"]),
        })
    }

    /// The underlying LSP client.
    pub fn client(&self) -> &LspClient {
        self.session.client()
    }

    /// The project root.
    pub fn root(&self) -> &Path {
        self.session.root()
    }

    /// The `file://` URI for a path, resolved against the project root.
    pub fn uri_for(&self, path: &Path) -> String {
        self.session.uri_for(path)
    }

    /// Open a document if it isn't already, returning its URI.
    pub async fn ensure_open(&self, path: &Path) -> Result<String> {
        Ok(self.session.ensure_open(path).await?)
    }

    /// Open the project's sources once so cross-file results are complete.
    pub async fn ensure_indexed(&self) -> Result<()> {
        Ok(self.session.ensure_indexed().await?)
    }

    /// Open the given files and wait for the server to reconcile them.
    pub async fn open_and_reconcile(&self, paths: &[PathBuf]) -> Result<()> {
        Ok(self.session.open_and_reconcile(paths).await?)
    }

    /// Sync files changed on disk back into the server.
    pub async fn sync_changed_impl(&self, changed: &[PathBuf]) {
        self.session.sync_changed(changed).await;
    }

    /// Begin one request, serializing on the session and clearing leaked overlays.
    pub async fn begin_request_impl(&self) -> RequestGuard {
        self.session.begin_request().await
    }

    /// Overlay a working copy's content onto the shared index.
    pub async fn overlay_workspace_impl(
        &self,
        workspace_root: &Path,
        delta: &[PathBuf],
    ) -> Result<()> {
        Ok(self.session.overlay_workspace(workspace_root, delta).await?)
    }

    /// Restore the base index view after an overlay.
    pub async fn restore_overlay_impl(&self) {
        self.session.restore_overlay().await;
    }

    /// Shut the server down.
    pub async fn shutdown(&self) -> Result<()> {
        Ok(self.session.shutdown().await?)
    }
}

/// Send `initialize`/`initialized`, advertising the JDT extended client
/// capabilities that unlock its command-based refactorings.
async fn initialize(client: &LspClient, root: &Path, bundles: &[String]) -> Result<()> {
    let root_uri = path_to_file_uri(root);
    let mut init_options = json!({
        "extendedClientCapabilities": { "classFileContentsSupport": true }
    });
    if !bundles.is_empty() {
        init_options["bundles"] = json!(bundles);
    }
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
        // action, which is what a headless client needs. The parameterized
        // refactorings (change-signature, move) instead go through our own
        // delegate-command bundle, loaded via `bundles` below.
        "initializationOptions": init_options
    });

    let _: Value = client.request("initialize", params).await?;
    client.notify("initialized", json!({})).await?;
    Ok(())
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
