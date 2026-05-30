//! The Java [`LanguageProvider`]: jdtls-backed sessions and (in later phases)
//! the Java operations.

use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use refactor_core::operation::Operation;
use refactor_core::provider::{LanguageProvider, LanguageSession};
use refactor_core::{Error as CoreError, Language, Project, Result as CoreResult, detect_revision};
use tokio::sync::Mutex;

use crate::error::JavaError;
use crate::jdtls::{JdtlsInstall, JdtlsSession, cache_base};
use crate::operations::{CodeActionOp, FindUsagesOp, RenameOp};

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
}

/// Provides Java semantics via Eclipse JDT LS, one pooled session per
/// (project root, revision) so switching branches reuses a warm session.
pub struct JavaProvider {
    install: JdtlsInstall,
    workspaces: PathBuf,
    sessions: Mutex<HashMap<(PathBuf, String), Arc<JdtlsSession>>>,
}

impl JavaProvider {
    /// Create the provider, locating a jdtls distribution up front so a missing
    /// install is reported at startup rather than on first use.
    pub fn new() -> Result<Self, JavaError> {
        let install = JdtlsInstall::locate()?;
        Ok(Self {
            install,
            workspaces: cache_base().join("workspaces"),
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
        let mut ops: Vec<Arc<dyn Operation>> = vec![Arc::new(RenameOp), Arc::new(FindUsagesOp)];
        ops.extend(CodeActionOp::java_set());
        ops
    }

    async fn session(&self, project: &Project) -> CoreResult<Arc<dyn LanguageSession>> {
        // Key the session by the project's current revision so that switching
        // branches gets its own warm index and switching back reuses one.
        let revision = detect_revision(&project.root)
            .map(|r| r.key())
            .unwrap_or_else(|| "no-vcs".to_string());
        let key = (project.root.clone(), revision.clone());

        if let Some(session) = self.sessions.lock().await.get(&key) {
            return Ok(Arc::clone(session) as Arc<dyn LanguageSession>);
        }

        // Start outside the lock — jdtls startup is slow — then de-dup on insert.
        let data_dir = self.workspaces.join(&project.id).join(&revision);
        let session = Arc::new(
            JdtlsSession::start(&self.install, &project.root, &data_dir)
                .await
                .map_err(|e| CoreError::Backend(e.to_string()))?,
        );

        let mut sessions = self.sessions.lock().await;
        let session = Arc::clone(sessions.entry(key).or_insert(session));
        Ok(session as Arc<dyn LanguageSession>)
    }
}
