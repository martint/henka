//! The project (tenant) model.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::language::Language;

/// A registered project: a local source tree the server can refactor.
///
/// A project pins an [`id`](Project::id) (a slug clients use to address it), a
/// [`root`](Project::root) path to an existing source tree, and the
/// [`languages`](Project::languages) detected within it. Registering a project
/// never copies or moves source; the server operates on the tree in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Short slug used to address the project on every call.
    pub id: String,
    /// Absolute path to the project's source tree.
    pub root: PathBuf,
    /// Languages detected under the root.
    pub languages: Vec<Language>,
}

impl Project {
    /// Whether the project contains the given language.
    pub fn has_language(&self, language: Language) -> bool {
        self.languages.contains(&language)
    }
}

/// Validate a project id: a non-empty slug of lowercase letters, digits and
/// dashes, not starting or ending with a dash.
pub fn validate_project_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ids() {
        assert!(validate_project_id("my-service"));
        assert!(validate_project_id("svc1"));
        assert!(validate_project_id("a"));
    }

    #[test]
    fn invalid_ids() {
        assert!(!validate_project_id(""));
        assert!(!validate_project_id("-leading"));
        assert!(!validate_project_id("trailing-"));
        assert!(!validate_project_id("Has_Underscore"));
        assert!(!validate_project_id("UpperCase"));
        assert!(!validate_project_id("with space"));
    }
}
