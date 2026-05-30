//! The multi-tenant project registry, with persistence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::language::{Language, detect_languages};
use crate::project::{Project, validate_project_id};

/// In-memory set of registered projects, backed by a TOML file on disk.
///
/// The registry is the multi-tenant heart of the server: it maps project ids
/// to their source trees and survives restarts by persisting to
/// [`config_path`](ProjectRegistry::config_path).
#[derive(Debug)]
pub struct ProjectRegistry {
    config_path: PathBuf,
    projects: BTreeMap<String, Project>,
}

impl ProjectRegistry {
    /// Load the registry from `config_path`, creating an empty one if the file
    /// does not yet exist.
    pub fn load(config_path: impl Into<PathBuf>) -> Result<Self> {
        let config_path = config_path.into();
        let projects = match std::fs::read_to_string(&config_path) {
            Ok(text) => {
                let file: RegistryFile =
                    toml::from_str(&text).map_err(|source| Error::ConfigRead {
                        path: config_path.clone(),
                        source,
                    })?;
                file.into_projects()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(Error::Io(e)),
        };
        Ok(Self {
            config_path,
            projects,
        })
    }

    /// The path this registry persists to.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Register a new project rooted at `root`, choosing an explicit `id` or
    /// deriving one from the directory name.
    ///
    /// Fails if the id is invalid or already taken, if the root is missing or
    /// not a directory, or if no supported language is detected. The
    /// registration is persisted before returning.
    pub fn register(&mut self, id: Option<String>, root: impl Into<PathBuf>) -> Result<Project> {
        let root = normalize_root(root.into())?;

        let id = match id {
            Some(id) => id,
            None => derive_id(&root),
        };
        if !validate_project_id(&id) {
            return Err(Error::InvalidProjectId(id));
        }
        if self.projects.contains_key(&id) {
            return Err(Error::ProjectAlreadyExists(id));
        }

        let languages = detect_languages(&root);
        if languages.is_empty() {
            return Err(Error::NoLanguageDetected(root));
        }

        let project = Project {
            id: id.clone(),
            root,
            languages,
        };
        self.projects.insert(id, project.clone());
        self.save()?;
        Ok(project)
    }

    /// Remove a registered project, returning it. Source is never touched.
    pub fn unregister(&mut self, id: &str) -> Result<Project> {
        let project = self
            .projects
            .remove(id)
            .ok_or_else(|| Error::ProjectNotFound(id.to_string()))?;
        self.save()?;
        Ok(project)
    }

    /// Look up a registered project by id.
    pub fn get(&self, id: &str) -> Result<&Project> {
        self.projects
            .get(id)
            .ok_or_else(|| Error::ProjectNotFound(id.to_string()))
    }

    /// All registered projects, ordered by id.
    pub fn list(&self) -> impl Iterator<Item = &Project> {
        self.projects.values()
    }

    /// Number of registered projects.
    pub fn len(&self) -> usize {
        self.projects.len()
    }

    /// Whether no projects are registered.
    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }

    /// Persist the current registry to disk, creating parent directories.
    fn save(&self) -> Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = RegistryFile::from_projects(&self.projects);
        let text = toml::to_string_pretty(&file)?;
        std::fs::write(&self.config_path, text)?;
        Ok(())
    }
}

/// Resolve the default registry path: `$HENKA_CONFIG`, else
/// `$XDG_CONFIG_HOME/henka/projects.toml`, else
/// `$HOME/.config/henka/projects.toml`.
pub fn default_config_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("HENKA_CONFIG") {
        return PathBuf::from(explicit);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("henka").join("projects.toml")
}

/// Normalize and validate a project root path.
fn normalize_root(root: PathBuf) -> Result<PathBuf> {
    if !root.exists() {
        return Err(Error::PathNotFound(root));
    }
    if !root.is_dir() {
        return Err(Error::NotADirectory(root));
    }
    // Canonicalize so registrations are stable regardless of how the path was
    // spelled; fall back to the original on platforms/paths that can't.
    Ok(root.canonicalize().unwrap_or(root))
}

/// Derive a project id from a directory name, sanitizing to a valid slug.
fn derive_id(root: &Path) -> String {
    let raw = root
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let slug: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "project".to_string()
    } else {
        slug
    }
}

/// On-disk shape of the registry file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    projects: BTreeMap<String, ProjectEntry>,
}

/// On-disk shape of a single project (id is the map key).
#[derive(Debug, Serialize, Deserialize)]
struct ProjectEntry {
    root: PathBuf,
    languages: Vec<Language>,
}

impl RegistryFile {
    fn from_projects(projects: &BTreeMap<String, Project>) -> Self {
        let projects = projects
            .iter()
            .map(|(id, p)| {
                (
                    id.clone(),
                    ProjectEntry {
                        root: p.root.clone(),
                        languages: p.languages.clone(),
                    },
                )
            })
            .collect();
        Self { projects }
    }

    fn into_projects(self) -> BTreeMap<String, Project> {
        self.projects
            .into_iter()
            .map(|(id, entry)| {
                (
                    id.clone(),
                    Project {
                        id,
                        root: entry.root,
                        languages: entry.languages,
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a directory that looks like a Java project.
    fn java_project(name: &str, parent: &Path) -> PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pom.xml"), "<project/>").unwrap();
        root
    }

    #[test]
    fn register_get_unregister() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("projects.toml");
        let root = java_project("svc", tmp.path());

        let mut reg = ProjectRegistry::load(&cfg).unwrap();
        let project = reg.register(Some("svc".into()), &root).unwrap();
        assert_eq!(project.id, "svc");
        assert_eq!(project.languages, vec![Language::Java]);
        assert_eq!(reg.get("svc").unwrap().id, "svc");

        let removed = reg.unregister("svc").unwrap();
        assert_eq!(removed.id, "svc");
        assert!(reg.get("svc").is_err());
    }

    #[test]
    fn persists_across_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("projects.toml");
        let root = java_project("svc", tmp.path());

        {
            let mut reg = ProjectRegistry::load(&cfg).unwrap();
            reg.register(Some("svc".into()), &root).unwrap();
        }

        let reg = ProjectRegistry::load(&cfg).unwrap();
        assert_eq!(reg.len(), 1);
        let project = reg.get("svc").unwrap();
        assert_eq!(project.languages, vec![Language::Java]);
    }

    #[test]
    fn rejects_duplicate_id() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("projects.toml");
        let root = java_project("svc", tmp.path());

        let mut reg = ProjectRegistry::load(&cfg).unwrap();
        reg.register(Some("svc".into()), &root).unwrap();
        let err = reg.register(Some("svc".into()), &root).unwrap_err();
        assert!(matches!(err, Error::ProjectAlreadyExists(_)));
    }

    #[test]
    fn rejects_root_without_language() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("projects.toml");
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        let mut reg = ProjectRegistry::load(&cfg).unwrap();
        let err = reg.register(Some("empty".into()), &empty).unwrap_err();
        assert!(matches!(err, Error::NoLanguageDetected(_)));
    }

    #[test]
    fn derives_id_from_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("projects.toml");
        let root = java_project("My Service!", tmp.path());

        let mut reg = ProjectRegistry::load(&cfg).unwrap();
        let project = reg.register(None, &root).unwrap();
        assert_eq!(project.id, "my-service");
    }
}
