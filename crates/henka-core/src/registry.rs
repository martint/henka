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

    /// Auto-register every immediate subdirectory of each `root` that has a
    /// detected language and is not already registered, deriving each id from
    /// the directory name. Returns the ids newly registered.
    ///
    /// Best-effort and idempotent: roots that cannot be read, entries that are
    /// not directories, directories with no detected language, and id
    /// collisions are all skipped silently. It is meant for auto-populating
    /// from a workspaces mount whose children are checkouts/worktrees, so that
    /// callers can target projects without registering them by hand.
    pub fn auto_register(&mut self, roots: &[PathBuf]) -> Vec<String> {
        let mut added = Vec::new();
        for root in roots {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            // A stable order keeps id derivation deterministic when two sibling
            // names would collide to the same slug (the first one wins).
            dirs.sort();
            for dir in dirs {
                // Skip a directory already registered (possibly under a
                // hand-chosen id) so a rescan never registers the same tree
                // twice.
                if self.is_registered_root(&dir) {
                    continue;
                }
                if let Ok(project) = self.register(None, &dir) {
                    added.push(project.id);
                }
            }
        }
        added
    }

    /// Whether `dir` is already registered as some project's root, comparing by
    /// canonical path (roots are canonicalized at registration).
    fn is_registered_root(&self, dir: &Path) -> bool {
        let canon = dir.canonicalize();
        let canon = canon.as_deref().unwrap_or(dir);
        self.projects.values().any(|p| p.root == canon)
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

/// Resolve the default registry path, in order: `$HENKA_CONFIG`,
/// `$HENKA_DATA/projects.toml`, `$XDG_CONFIG_HOME/henka/projects.toml`, else
/// `$HOME/.config/henka/projects.toml`.
pub fn default_config_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("HENKA_CONFIG") {
        return PathBuf::from(explicit);
    }
    if let Some(data) = data_root() {
        return data.join("projects.toml");
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("henka").join("projects.toml")
}

/// The single root for all of Henka's persistent state, if set via `$HENKA_DATA`.
/// When set, both the project registry and the per-repository indexes live under
/// it, so one host-mounted directory holds everything (e.g. `/data` in a
/// container). `None` falls back to the per-purpose XDG locations.
pub fn data_root() -> Option<PathBuf> {
    std::env::var_os("HENKA_DATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
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
    fn data_root_places_registry_under_it() {
        // HENKA_CONFIG wins over HENKA_DATA; HENKA_DATA roots the registry
        // otherwise. Use a unique value and restore the env to stay isolated.
        let prev_data = std::env::var_os("HENKA_DATA");
        let prev_cfg = std::env::var_os("HENKA_CONFIG");
        // SAFETY: single-threaded test; env restored before returning.
        unsafe {
            std::env::remove_var("HENKA_CONFIG");
            std::env::set_var("HENKA_DATA", "/srv/henka-data");
        }
        assert_eq!(
            default_config_path(),
            PathBuf::from("/srv/henka-data/projects.toml")
        );
        assert_eq!(data_root(), Some(PathBuf::from("/srv/henka-data")));

        unsafe {
            std::env::set_var("HENKA_CONFIG", "/explicit/cfg.toml");
        }
        assert_eq!(default_config_path(), PathBuf::from("/explicit/cfg.toml"));

        unsafe {
            match prev_data {
                Some(v) => std::env::set_var("HENKA_DATA", v),
                None => std::env::remove_var("HENKA_DATA"),
            }
            match prev_cfg {
                Some(v) => std::env::set_var("HENKA_CONFIG", v),
                None => std::env::remove_var("HENKA_CONFIG"),
            }
        }
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
    fn auto_registers_child_projects_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("projects.toml");
        let workspaces = tmp.path().join("workspaces");
        std::fs::create_dir_all(&workspaces).unwrap();
        java_project("svc-a", &workspaces);
        java_project("svc-b", &workspaces);
        // A child with no detectable language is skipped.
        std::fs::create_dir_all(workspaces.join("empty")).unwrap();

        let mut reg = ProjectRegistry::load(&cfg).unwrap();
        let mut added = reg.auto_register(&[workspaces.clone()]);
        added.sort();
        assert_eq!(added, vec!["svc-a".to_string(), "svc-b".to_string()]);

        // A second scan adds nothing — registration is not duplicated.
        assert!(reg.auto_register(&[workspaces.clone()]).is_empty());
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn auto_register_skips_root_already_registered_under_custom_id() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("projects.toml");
        let workspaces = tmp.path().join("workspaces");
        std::fs::create_dir_all(&workspaces).unwrap();
        let svc = java_project("svc", &workspaces);

        let mut reg = ProjectRegistry::load(&cfg).unwrap();
        // Pre-register the same tree under a hand-chosen id.
        reg.register(Some("custom".into()), &svc).unwrap();

        // The scan must not register the tree again under its derived id.
        assert!(reg.auto_register(&[workspaces]).is_empty());
        assert_eq!(reg.len(), 1);
        assert!(reg.get("svc").is_err());
    }

    #[test]
    fn auto_register_ignores_unreadable_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("projects.toml");
        let mut reg = ProjectRegistry::load(&cfg).unwrap();
        // A non-existent root is silently skipped, not an error.
        assert!(reg.auto_register(&[tmp.path().join("nope")]).is_empty());
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
