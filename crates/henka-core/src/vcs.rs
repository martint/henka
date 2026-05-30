//! Detecting a project's version-control revision.
//!
//! The semantic index is keyed by revision so that switching branches reuses a
//! warm index rather than rebuilding it. This module reads the current revision
//! from jujutsu or git; it never mutates the repository.

use std::fmt;
use std::path::Path;
use std::process::Command;

/// The version-control system backing a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Vcs {
    /// Jujutsu.
    Jj,
    /// Git.
    Git,
}

impl fmt::Display for Vcs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Vcs::Jj => "jj",
            Vcs::Git => "git",
        })
    }
}

/// A project's current revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Revision {
    /// Which VCS reported it.
    pub vcs: Vcs,
    /// A short revision identifier (jj change id or git commit hash).
    pub id: String,
    /// The branch/bookmark name, if any.
    pub branch: Option<String>,
}

impl Revision {
    /// A stable cache key for this revision (`vcs-id`).
    pub fn key(&self) -> String {
        format!("{}-{}", self.vcs, self.id)
    }
}

/// Detect the current revision of the repository at `root`, or `None` if it is
/// not a jj or git repository (or the VCS tool is unavailable).
///
/// jj is preferred when both are present (a colocated repo). The working copy is
/// never snapshotted — detection is read-only.
pub fn detect_revision(root: &Path) -> Option<Revision> {
    if root.join(".jj").is_dir() {
        let id = run(
            root,
            "jj",
            &[
                "log",
                "-r",
                "@",
                "--no-graph",
                "--ignore-working-copy",
                "-T",
                "change_id.short()",
            ],
        )?;
        let branch = run(
            root,
            "jj",
            &[
                "log",
                "-r",
                "@",
                "--no-graph",
                "--ignore-working-copy",
                "-T",
                r#"bookmarks.join(" ")"#,
            ],
        )
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
        return Some(Revision {
            vcs: Vcs::Jj,
            id: id.trim().to_string(),
            branch,
        });
    }

    if root.join(".git").exists() {
        let id = run(root, "git", &["rev-parse", "--short", "HEAD"])?;
        let branch = run(root, "git", &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "HEAD");
        return Some(Revision {
            vcs: Vcs::Git,
            id: id.trim().to_string(),
            branch,
        });
    }

    None
}

/// Run a VCS command in `root`, returning its trimmed stdout on success.
fn run(root: &Path, program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_vcs_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_revision(dir.path()).is_none());
    }

    #[test]
    fn detects_git_revision() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Initialize a git repo with one commit; skip if git is unavailable.
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
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
                .current_dir(root)
                .status()
                .unwrap();
        }
        std::fs::write(root.join("a.txt"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(root)
            .status()
            .unwrap();

        let rev = detect_revision(root).expect("git revision");
        assert_eq!(rev.vcs, Vcs::Git);
        assert!(!rev.id.is_empty());
    }
}
