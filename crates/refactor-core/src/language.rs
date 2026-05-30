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
}

impl Language {
    /// The stable lowercase identifier for this language.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Java => "java",
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
        }
        if found_java {
            break;
        }
    }

    let mut langs = Vec::new();
    if found_java {
        langs.push(Language::Java);
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
