//! A reusable per-project document session over an LSP connection.
//!
//! [`LspSession`] owns the open-document bookkeeping, source indexing, applied-
//! edit sync, and the working-copy **overlay** that lets one warm index serve a
//! sibling working copy (git worktree / jj workspace). It is backend-agnostic:
//! a provider performs its own launch, `initialize` handshake, and readiness
//! wait, then wraps the live [`LspClient`] in an `LspSession` for everything
//! else.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use henka_core::provider::RequestGuard;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};
use tokio::time::Duration;

use crate::LspClient;
use crate::Result;

/// Upper bound on source files opened to index a project.
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

/// Files presented on top of the base index by an overlay, recorded so the
/// overlay can be restored after the request.
#[derive(Default)]
struct OverlayState {
    /// Base-abs paths that were already open and got a `didChange` to the
    /// working copy's content; restored by changing them back to base content.
    changed: HashSet<PathBuf>,
    /// Base-abs paths opened solely for the overlay; restored by closing them.
    opened: HashSet<PathBuf>,
}

/// A document session for one project root over a live LSP connection.
pub struct LspSession {
    client: LspClient,
    root: PathBuf,
    /// The source extensions this session indexes, each mapped to the LSP
    /// `languageId` to tag documents of that extension with — e.g.
    /// `("java", "java")` or `("tsx", "typescriptreact")`.
    langs: Vec<(String, String)>,
    opened: Mutex<HashSet<PathBuf>>,
    indexed: AtomicBool,
    /// Serializes requests so a working-copy overlay stays coherent.
    request: Arc<Mutex<()>>,
    /// Files currently overlaid on the base index.
    overlay: Mutex<OverlayState>,
    /// Set while an overlay is active, so a leaked overlay (a request that did
    /// not restore) is cleared at the start of the next request.
    overlay_dirty: AtomicBool,
    /// Monotonic version counter for `didChange` notifications.
    doc_version: AtomicU32,
}

impl LspSession {
    /// Wrap an already-initialized client. `langs` maps each source extension
    /// this session indexes to the LSP `languageId` documents of that extension
    /// are opened with.
    pub fn new(client: LspClient, root: &Path, langs: &[(&str, &str)]) -> Self {
        Self {
            client,
            root: root.to_path_buf(),
            langs: langs
                .iter()
                .map(|(ext, id)| (ext.to_string(), id.to_string()))
                .collect(),
            opened: Mutex::new(HashSet::new()),
            indexed: AtomicBool::new(false),
            request: Arc::new(Mutex::new(())),
            overlay: Mutex::new(OverlayState::default()),
            overlay_dirty: AtomicBool::new(false),
            doc_version: AtomicU32::new(1),
        }
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
        path_to_file_uri(&self.abs(path))
    }

    /// Resolve a path against the project root.
    fn abs(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    /// Open a document in the server if it isn't already, returning its URI.
    pub async fn ensure_open(&self, path: &Path) -> Result<String> {
        let abs = self.abs(path);
        let uri = path_to_file_uri(&abs);

        {
            let opened = self.opened.lock().await;
            if opened.contains(&abs) {
                return Ok(uri);
            }
        }

        let text = std::fs::read_to_string(&abs)?;
        self.did_open(&uri, self.language_id_for(&abs), &text).await?;
        self.opened.lock().await.insert(abs);
        Ok(uri)
    }

    /// The LSP `languageId` for a path, by its extension; falls back to the
    /// first configured language when the extension is unknown.
    fn language_id_for(&self, path: &Path) -> &str {
        let ext = path.extension().and_then(|e| e.to_str());
        self.langs
            .iter()
            .find(|(e, _)| Some(e.as_str()) == ext)
            .or_else(|| self.langs.first())
            .map(|(_, id)| id.as_str())
            .unwrap_or("plaintext")
    }

    /// Notify the server that `uri` is open with the given `text`.
    async fn did_open(&self, uri: &str, language_id: &str, text: &str) -> Result<()> {
        self.client
            .notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": 1,
                        "text": text,
                    }
                }),
            )
            .await?;
        Ok(())
    }

    /// Notify the server of a full-document change of `uri` to `text`.
    async fn did_change_full(&self, uri: &str, text: &str) -> Result<()> {
        let version = self.doc_version.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        self.client
            .notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }],
                }),
            )
            .await?;
        Ok(())
    }

    /// Notify the server that `uri` is closed (server re-reads it from disk).
    async fn did_close(&self, uri: &str) -> Result<()> {
        self.client
            .notify(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await?;
        Ok(())
    }

    /// Ensure the project's sources are resolved before an operation reads from
    /// them, done once per session. Opening the sources up front makes cross-file
    /// results (rename, find-usages) complete even for a loose-file project or
    /// one whose build-tool import has not finished.
    pub async fn ensure_indexed(&self) -> Result<()> {
        if self.indexed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let files = collect_source_files(&self.root, |p| self.indexes(p));
        self.open_and_reconcile(&files).await
    }

    /// Open the given files (if not already open) and wait for the server to
    /// reconcile each newly-opened one, so a subsequent request reads from them.
    pub async fn open_and_reconcile(&self, paths: &[PathBuf]) -> Result<()> {
        // Subscribe before opening so reconcile signals aren't missed.
        let mut events = self.client.subscribe();

        let mut pending: HashSet<String> = HashSet::new();
        for path in paths {
            let abs = self.abs(path);
            let already_open = self.opened.lock().await.contains(&abs);
            let uri = self.ensure_open(path).await?;
            if !already_open {
                pending.insert(uri);
            }
        }
        self.drain_until_reconciled(&mut events, pending).await;
        Ok(())
    }

    /// Wait until the server has published diagnostics for each URI in `pending`
    /// (signalling it reconciled that document), bounded by a timeout. `events`
    /// must have been subscribed before the documents were sent.
    async fn drain_until_reconciled(
        &self,
        events: &mut broadcast::Receiver<(String, Value)>,
        mut pending: HashSet<String>,
    ) {
        if pending.is_empty() {
            return;
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
    }

    /// Sync files that changed on disk (after an edit was applied) into the
    /// server: any that were open are closed so the server re-reads them from
    /// disk, and a watched-files change is announced so the index updates.
    pub async fn sync_changed(&self, changed: &[PathBuf]) {
        let mut to_close = Vec::new();
        let mut watch = Vec::new();
        {
            let mut opened = self.opened.lock().await;
            for path in changed {
                let abs = self.abs(path);
                let uri = path_to_file_uri(&abs);
                if opened.remove(&abs) {
                    to_close.push(uri.clone());
                }
                watch.push(json!({ "uri": uri, "type": 2 })); // 2 = Changed
            }
        }
        for uri in to_close {
            let _ = self.did_close(&uri).await;
        }
        let _ = self
            .client
            .notify(
                "workspace/didChangeWatchedFiles",
                json!({ "changes": watch }),
            )
            .await;
    }

    /// Acquire the per-session request lock, held for one request's duration so
    /// a working-copy overlay can't be observed by a concurrent request. Also
    /// clears any overlay leaked by a prior request that failed to restore.
    pub async fn begin_request(&self) -> RequestGuard {
        let guard = self.request.clone().lock_owned().await;
        if self.overlay_dirty.load(Ordering::Acquire) {
            self.restore_overlay().await;
        }
        RequestGuard::holding(Box::new(guard))
    }

    /// Present `workspace_root`'s modified `delta` files (relative paths) on top
    /// of the base index, so an operation sees that working copy's content. The
    /// content is addressed under the base root's URIs, so the warm index is
    /// reused. A no-op when the working copy *is* the base checkout.
    pub async fn overlay_workspace(&self, workspace_root: &Path, delta: &[PathBuf]) -> Result<()> {
        if workspace_root == self.root {
            return Ok(());
        }
        // Subscribe before sending so reconcile signals aren't missed.
        let mut events = self.client.subscribe();
        let mut pending: HashSet<String> = HashSet::new();
        for rel in delta {
            if !self.indexes(rel) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(workspace_root.join(rel)) else {
                continue;
            };
            let base_abs = self.root.join(rel);
            let uri = path_to_file_uri(&base_abs);
            let already_open = self.opened.lock().await.contains(&base_abs);
            if already_open {
                self.did_change_full(&uri, &content).await?;
                self.overlay.lock().await.changed.insert(base_abs);
            } else {
                self.did_open(&uri, self.language_id_for(&base_abs), &content)
                    .await?;
                self.opened.lock().await.insert(base_abs.clone());
                self.overlay.lock().await.opened.insert(base_abs);
            }
            self.overlay_dirty.store(true, Ordering::Release);
            pending.insert(uri);
        }
        self.drain_until_reconciled(&mut events, pending).await;
        Ok(())
    }

    /// Undo the active overlay: change overlaid documents back to their base
    /// content, close any opened solely for the overlay, and clear the record.
    /// Idempotent — safe to call to clear a leaked overlay.
    pub async fn restore_overlay(&self) {
        let state = std::mem::take(&mut *self.overlay.lock().await);
        if state.changed.is_empty() && state.opened.is_empty() {
            self.overlay_dirty.store(false, Ordering::Release);
            return;
        }
        let mut events = self.client.subscribe();
        let mut pending: HashSet<String> = HashSet::new();
        for base_abs in &state.changed {
            if let Ok(base_content) = std::fs::read_to_string(base_abs) {
                let uri = path_to_file_uri(base_abs);
                let _ = self.did_change_full(&uri, &base_content).await;
                pending.insert(uri);
            }
        }
        for base_abs in &state.opened {
            let uri = path_to_file_uri(base_abs);
            let _ = self.did_close(&uri).await;
            self.opened.lock().await.remove(base_abs);
        }
        self.drain_until_reconciled(&mut events, pending).await;
        self.overlay_dirty.store(false, Ordering::Release);
    }

    /// Shut the server down.
    pub async fn shutdown(&self) -> Result<()> {
        self.client.shutdown().await?;
        Ok(())
    }

    /// Whether `path` is a source file this session indexes (by extension).
    fn indexes(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| self.langs.iter().any(|(ext, _)| ext == e))
    }
}

/// Convert an absolute path to a `file://` URI, percent-encoding the few
/// characters that matter for typical source paths.
pub fn path_to_file_uri(path: &Path) -> String {
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

/// Collect up to [`MAX_INDEX_FILES`] files under `root` for which `wanted`
/// returns true, skipping build and VCS directories.
fn collect_source_files(root: &Path, wanted: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
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
            } else if wanted(&path) {
                out.push(path);
            }
        }
    }
    out
}
