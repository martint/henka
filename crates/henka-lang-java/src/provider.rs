//! The Java [`LanguageProvider`]: jdtls-backed sessions and (in later phases)
//! the Java operations.

use std::any::Any;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use henka_core::operation::Operation;
use henka_core::provider::{LanguageProvider, LanguageSession, RequestGuard};
use henka_core::{Error as CoreError, Language, Project, Result as CoreResult, repo_identity};
use tokio::sync::Mutex;

use crate::error::JavaError;
use crate::jdtls::{JdtlsInstall, JdtlsSession, index_base};
use crate::operations::{ChangeSignatureOp, CodeActionOp, FindUsagesOp, RenameOp};

#[async_trait]
impl LanguageSession for JdtlsSession {
    fn language(&self) -> Language {
        Language::Java
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn sync_changed(&self, changed: &[PathBuf]) {
        self.sync_changed_impl(changed).await;
    }

    fn root(&self) -> Option<&Path> {
        Some(JdtlsSession::root(self))
    }

    async fn begin_request(&self) -> RequestGuard {
        self.begin_request_impl().await
    }

    async fn overlay_workspace(&self, workspace_root: &Path, delta: &[PathBuf]) -> CoreResult<()> {
        self.overlay_workspace_impl(workspace_root, delta)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))
    }

    async fn restore_overlay(&self) {
        self.restore_overlay_impl().await;
    }
}

/// Provides Java semantics via Eclipse JDT LS, with one shared session per
/// repository (keyed by [`session_key`]) so every git worktree / jj workspace
/// of a repository reuses one warm index.
pub struct JavaProvider {
    install: JdtlsInstall,
    workspaces: PathBuf,
    sessions: Mutex<HashMap<String, Arc<JdtlsSession>>>,
}

/// The pooling key for a project's shared session: the repository identity when
/// the root is under version control — so every working copy of one repository
/// maps to the same key — else a stable key derived from the root path.
fn session_key(project: &Project) -> String {
    match repo_identity(&project.root) {
        Some(id) => id.slug(),
        None => {
            let mut hasher = DefaultHasher::new();
            project.root.hash(&mut hasher);
            format!("novcs-{:016x}", hasher.finish())
        }
    }
}

impl JavaProvider {
    /// Create the provider, locating a jdtls distribution up front so a missing
    /// install is reported at startup rather than on first use.
    pub fn new() -> Result<Self, JavaError> {
        let install = JdtlsInstall::locate()?;
        Ok(Self {
            install,
            workspaces: index_base().join("workspaces"),
            sessions: Mutex::new(HashMap::new()),
        })
    }
}

#[async_trait]
impl LanguageProvider for JavaProvider {
    fn language(&self) -> Language {
        Language::Java
    }

    fn operations(&self) -> Vec<Arc<dyn Operation>> {
        let mut ops: Vec<Arc<dyn Operation>> = vec![
            Arc::new(RenameOp),
            Arc::new(FindUsagesOp),
            Arc::new(ChangeSignatureOp),
        ];
        ops.extend(CodeActionOp::java_set());
        ops
    }

    async fn session(&self, project: &Project) -> CoreResult<Arc<dyn LanguageSession>> {
        // Key by repository identity so every working copy shares one session.
        let key = session_key(project);

        if let Some(session) = self.sessions.lock().await.get(&key) {
            return Ok(Arc::clone(session) as Arc<dyn LanguageSession>);
        }

        // Start outside the lock — jdtls startup is slow — then de-dup on insert.
        // The session is rooted at the registered project root; other working
        // copies are served by overlaying their content per request.
        let data_dir = self.workspaces.join(&key);
        let bundles: Vec<PathBuf> = crate::jdtls::locate_bundle().into_iter().collect();
        let session = Arc::new(
            JdtlsSession::start(&self.install, &project.root, &data_dir, &bundles)
                .await
                .map_err(|e| CoreError::Backend(e.to_string()))?,
        );

        let mut sessions = self.sessions.lock().await;
        let session = Arc::clone(sessions.entry(key).or_insert(session));
        Ok(session as Arc<dyn LanguageSession>)
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use henka_core::Language;

    use super::*;

    fn project_at(root: PathBuf) -> Project {
        Project {
            id: "p".into(),
            root,
            languages: vec![Language::Java],
        }
    }

    #[test]
    fn novcs_roots_get_distinct_keys() {
        let dir = tempfile::tempdir().unwrap();
        let a = project_at(dir.path().join("a"));
        let b = project_at(dir.path().join("b"));
        assert_ne!(session_key(&a), session_key(&b));
        assert_eq!(session_key(&a), session_key(&a), "stable per root");
    }

    #[test]
    fn git_worktrees_share_session_key() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&main)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }
        for args in [
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            Command::new("git")
                .args(&args)
                .current_dir(&main)
                .status()
                .unwrap();
        }
        std::fs::write(main.join("a.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&main)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(&main)
            .status()
            .unwrap();
        let wt = dir.path().join("wt");
        if !Command::new("git")
            .args(["worktree", "add", "-q", wt.to_str().unwrap()])
            .current_dir(&main)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }

        assert_eq!(
            session_key(&project_at(main)),
            session_key(&project_at(wt)),
            "a worktree shares the main checkout's session key"
        );
    }
}
