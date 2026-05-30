//! Locating, launching, and driving an Eclipse JDT language server.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use refactor_lsp::LspClient;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::Mutex;

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
        };
        session.initialize().await?;
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
            "initializationOptions": {
                "extendedClientCapabilities": {
                    "classFileContentsSupport": true,
                    "advancedExtractRefactoringSupport": true,
                    "advancedOrganizeImportsSupport": true,
                    "moveRefactoringSupport": true,
                    "resolveAdditionalTextEditsSupport": true,
                    "advancedIntroduceParameterRefactoringSupport": true,
                    "extractInterfaceSupport": true,
                    "executeClientCommandSupport": true,
                    "inferSelectionSupport": ["extractMethod", "extractVariable", "extractConstant", "extractField"]
                }
            }
        });

        let _: Value = self.client.request("initialize", params).await?;
        self.client.notify("initialized", json!({})).await?;
        Ok(())
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
