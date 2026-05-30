//! Supported languages and detection.

use std::path::Path;

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

/// A programming language the server can refactor.
///
/// Each language is backed by a provider (see the `provider` module in later
/// phases) that contributes its semantic understanding and refactorings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Language {
    /// Java.
    Java,
    /// Rust.
    Rust,
}

impl Language {
    /// The stable lowercase identifier for this language.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Java => "java",
            Language::Rust => "rust",
        }
    }

    /// The language of a source file, inferred from its extension, if
    /// recognized. Used to route an operation on a file to the right backend
    /// when a project (repository) spans more than one language.
    pub fn from_path(path: &Path) -> Option<Language> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("java") => Some(Language::Java),
            Some("rs") => Some(Language::Rust),
            _ => None,
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Directory names that never contain source worth scanning.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".jj",
    ".hg",
    "target",
    "build",
    "out",
    "node_modules",
    ".gradle",
    ".idea",
];

/// Upper bound on entries walked during detection, so registering a project
/// stays fast even on very large trees.
const MAX_WALK_ENTRIES: usize = 50_000;

/// Detect the supported languages present under `root`.
///
/// Detection is heuristic and bounded: it scans the tree (skipping build and
/// VCS directories) looking for language markers, stopping early once every
/// supported language has been found or the entry budget is exhausted.
pub fn detect_languages(root: &Path) -> Vec<Language> {
    let mut found_java = false;
    let mut found_rust = false;
    let mut walked = 0usize;

    let walker = WalkDir::new(root).into_iter().filter_entry(|entry| {
        // Always allow the root itself; otherwise skip noisy directories.
        if entry.depth() == 0 {
            return true;
        }
        let name = entry.file_name().to_string_lossy();
        !(entry.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref()))
    });

    for entry in walker.flatten() {
        walked += 1;
        if walked > MAX_WALK_ENTRIES {
            break;
        }
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy();
            if is_java_marker(&name) {
                found_java = true;
            }
            if is_rust_marker(&name) {
                found_rust = true;
            }
        }
        if found_java && found_rust {
            break;
        }
    }

    let mut langs = Vec::new();
    if found_java {
        langs.push(Language::Java);
    }
    if found_rust {
        langs.push(Language::Rust);
    }
    langs
}

/// Whether a file name marks a Java project or source file.
fn is_java_marker(name: &str) -> bool {
    name.ends_with(".java")
        || matches!(
            name,
            "pom.xml"
                | "build.gradle"
                | "build.gradle.kts"
                | "settings.gradle"
                | "settings.gradle.kts"
        )
}

/// Whether a file name marks a Rust project or source file.
fn is_rust_marker(name: &str) -> bool {
    name.ends_with(".rs") || name == "Cargo.toml"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_java_from_source_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/main/java/com/example")).unwrap();
        std::fs::write(
            dir.path().join("src/main/java/com/example/Main.java"),
            "class Main {}",
        )
        .unwrap();

        assert_eq!(detect_languages(dir.path()), vec![Language::Java]);
    }

    #[test]
    fn detects_java_from_pom() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        assert_eq!(detect_languages(dir.path()), vec![Language::Java]);
    }

    #[test]
    fn from_path_maps_known_and_unknown_extensions() {
        assert_eq!(
            Language::from_path(Path::new("src/Main.java")),
            Some(Language::Java)
        );
        assert_eq!(
            Language::from_path(Path::new("src/main.rs")),
            Some(Language::Rust)
        );
        assert_eq!(Language::from_path(Path::new("README.md")), None);
        assert_eq!(Language::from_path(Path::new("noext")), None);
    }

    #[test]
    fn detects_rust_from_cargo_and_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(detect_languages(dir.path()), vec![Language::Rust]);
    }

    #[test]
    fn detects_both_languages_in_one_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        let langs = detect_languages(dir.path());
        assert!(langs.contains(&Language::Java), "{langs:?}");
        assert!(langs.contains(&Language::Rust), "{langs:?}");
    }

    #[test]
    fn empty_tree_detects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_languages(dir.path()).is_empty());
    }

    #[test]
    fn skips_build_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        // A .java file only under target/ must not count.
        std::fs::write(dir.path().join("target/Generated.java"), "class G {}").unwrap();
        assert!(detect_languages(dir.path()).is_empty());
    }
}
