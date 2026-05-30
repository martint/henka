//! Language providers and their analysis sessions.
//!
//! A [`LanguageProvider`] supplies the semantic understanding for one language:
//! it contributes that language's [`Operation`]s and creates per-project
//! [`LanguageSession`]s. The session is the handle an operation downcasts to
//! reach its backend (e.g. an LSP connection). Providers are kept in a
//! [`ProviderRegistry`].

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::language::Language;
use crate::operation::Operation;
use crate::project::Project;

/// A per-project, language-specific analysis session.
///
/// The trait itself is deliberately minimal; concrete providers expose their
/// real capabilities on the underlying type, which operations reach via
/// [`as_any`](LanguageSession::as_any) and downcasting.
pub trait LanguageSession: Send + Sync {
    /// The language this session serves.
    fn language(&self) -> Language;

    /// Access the concrete session type for downcasting.
    fn as_any(&self) -> &dyn Any;
}

/// Supplies semantic understanding and operations for one language.
#[async_trait]
pub trait LanguageProvider: Send + Sync {
    /// The language this provider serves.
    fn language(&self) -> Language;

    /// The operations this language contributes to the catalog.
    fn operations(&self) -> Vec<Arc<dyn Operation>>;

    /// Obtain the analysis session for `project`, starting or reusing it.
    ///
    /// Implementations are expected to pool sessions per project so repeated
    /// calls are cheap and the index stays warm.
    async fn session(&self, project: &Project) -> Result<Arc<dyn LanguageSession>>;
}

/// The set of registered language providers, keyed by language.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<Language, Arc<dyn LanguageProvider>>,
}

impl ProviderRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider, replacing any existing one for its language.
    pub fn register(&mut self, provider: Arc<dyn LanguageProvider>) {
        self.providers.insert(provider.language(), provider);
    }

    /// The provider for a language, if registered.
    pub fn get(&self, language: Language) -> Option<Arc<dyn LanguageProvider>> {
        self.providers.get(&language).map(Arc::clone)
    }

    /// Every operation contributed by every registered provider.
    pub fn operations(&self) -> Vec<Arc<dyn Operation>> {
        self.providers
            .values()
            .flat_map(|p| p.operations())
            .collect()
    }

    /// The languages with a registered provider.
    pub fn languages(&self) -> impl Iterator<Item = Language> + '_ {
        self.providers.keys().copied()
    }
}
