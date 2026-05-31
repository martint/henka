//! The TypeScript/JavaScript [`LanguageProvider`]: typescript-language-server
//! sessions and the TS/JS operations. One provider serves both languages.

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

use crate::error::TsError;
use crate::operations::{CodeActionOp, FindUsagesOp, RenameOp};
use crate::server::{TsSession, locate};

/// The languages this backend serves; register the provider for all of them.
pub const LANGUAGES: &[Language] = &[Language::TypeScript, Language::JavaScript];

#[async_trait]
impl LanguageSession for TsSession {
    fn language(&self) -> Language {
        Language::TypeScript
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn sync_changed(&self, changed: &[PathBuf]) {
        self.sync_changed_impl(changed).await;
    }

    fn root(&self) -> Option<&Path> {
        Some(TsSession::root(self))
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

/// Provides TypeScript and JavaScript semantics via typescript-language-server,
/// with one shared session per repository so every git worktree / jj workspace
/// reuses one warm server.
pub struct TsProvider {
    program: PathBuf,
    sessions: Mutex<HashMap<String, Arc<TsSession>>>,
}

/// The pooling key for a project's shared session: the repository identity when
/// the root is under version control, else a stable key derived from the root.
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

impl TsProvider {
    /// Create the provider, locating a typescript-language-server up front so a
    /// missing install is reported at startup rather than on first use.
    pub fn new() -> Result<Self, TsError> {
        let program = locate()?;
        Ok(Self {
            program,
            sessions: Mutex::new(HashMap::new()),
        })
    }
}

#[async_trait]
impl LanguageProvider for TsProvider {
    fn language(&self) -> Language {
        Language::TypeScript
    }

    fn operations(&self) -> Vec<Arc<dyn Operation>> {
        let mut ops: Vec<Arc<dyn Operation>> = vec![Arc::new(RenameOp), Arc::new(FindUsagesOp)];
        ops.extend(CodeActionOp::ts_set());
        ops
    }

    async fn session(&self, project: &Project) -> CoreResult<Arc<dyn LanguageSession>> {
        let key = session_key(project);

        if let Some(session) = self.sessions.lock().await.get(&key) {
            return Ok(Arc::clone(session) as Arc<dyn LanguageSession>);
        }

        let session = Arc::new(
            TsSession::start(&self.program, &project.root)
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
    use super::*;

    #[test]
    fn novcs_roots_get_distinct_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = |root: PathBuf| Project {
            id: "p".into(),
            root,
            languages: vec![Language::TypeScript],
        };
        assert_ne!(
            session_key(&p(dir.path().join("a"))),
            session_key(&p(dir.path().join("b")))
        );
    }
}
