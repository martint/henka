//! Detecting a project's version-control revision.
//!
//! The semantic index is keyed by revision so that switching branches reuses a
//! warm index rather than rebuilding it. This module reads the current revision
//! from jujutsu or git; it never mutates the repository.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
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

/// A canonical identity for a repository, shared by all of its working copies
/// (git worktrees, jj workspaces).
///
/// Two directories that are different checkouts of the same repository resolve
/// to the same `RepoId`, which lets the server keep one semantic index per
/// repository instead of one per checkout.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoId {
    /// Which VCS the identity was derived from.
    pub vcs: Vcs,
    /// The canonical identity path: the git common directory or the jj repo
    /// directory.
    pub path: PathBuf,
}

impl RepoId {
    /// A stable, filesystem-safe slug for use as a directory name (e.g. a data
    /// directory). Derived from a hash of the identity path so it never
    /// contains path separators or case-sensitive surprises.
    pub fn slug(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.path.hash(&mut hasher);
        format!("{}-{:016x}", self.vcs, hasher.finish())
    }
}

/// Resolve the identity of the repository containing `path`, or `None` if it is
/// not a jj or git repository (or the VCS tool is unavailable).
///
/// jj is preferred when both are present (a colocated repo), matching
/// [`detect_revision`]. Detection is read-only.
pub fn repo_identity(path: &Path) -> Option<RepoId> {
    if path.join(".jj").is_dir()
        && let Some(repo) = jj_repo_dir(path)
    {
        return Some(RepoId {
            vcs: Vcs::Jj,
            path: repo,
        });
    }

    if path.join(".git").exists() {
        let common = run(path, "git", &["rev-parse", "--git-common-dir"])?;
        let common = common.trim();
        let abs = if Path::new(common).is_absolute() {
            PathBuf::from(common)
        } else {
            path.join(common)
        };
        return Some(RepoId {
            vcs: Vcs::Git,
            path: abs.canonicalize().unwrap_or(abs),
        });
    }

    None
}

/// The relative paths of files modified in the working copy at `root` versus
/// its base, restricted to files that currently exist on disk (so they can be
/// read and overlaid). Empty for a clean git worktree.
///
/// Uses `git status` for git and `jj diff` for jj. The jj query snapshots the
/// working copy (jj's normal behavior), unlike the read-only [`detect_revision`].
pub fn working_copy_delta(root: &Path) -> Vec<PathBuf> {
    let paths = if root.join(".jj").is_dir() {
        run(root, "jj", &["diff", "--summary", "-r", "@"]).map(|s| parse_jj_diff_summary(&s))
    } else if root.join(".git").exists() {
        run_bytes(root, "git", &["status", "--porcelain", "-z"])
            .map(|b| parse_git_porcelain_z(&b))
    } else {
        None
    };

    paths
        .unwrap_or_default()
        .into_iter()
        .filter(|rel| root.join(rel).is_file())
        .collect()
}

/// Parse `git status --porcelain -z` output. Entries are NUL-terminated; a
/// rename entry is two NUL-separated fields (`old\0new`) and contributes its
/// destination. The two-character status prefix and following space are
/// stripped.
fn parse_git_porcelain_z(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let mut fields = text.split('\0').filter(|s| !s.is_empty());
    let mut out = Vec::new();
    while let Some(entry) = fields.next() {
        // Each entry is `XY <path>`; the status is the first two chars.
        let status = entry.as_bytes();
        let is_rename = status.first() == Some(&b'R') || status.get(1) == Some(&b'R');
        let path = entry.get(3..).unwrap_or("");
        if is_rename {
            // The destination is this entry's path; the next field is the
            // original name, which we skip.
            out.push(PathBuf::from(path));
            let _ = fields.next();
        } else {
            out.push(PathBuf::from(path));
        }
    }
    out
}

/// Parse `jj diff --summary` output: one `STATUS path` per line, where STATUS is
/// `A`/`M`/`D`, or `R old new` / `C old new` for rename/copy (destination last).
fn parse_jj_diff_summary(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((status, rest)) = line.split_once(' ') else {
            continue;
        };
        let path = match status {
            "R" | "C" => rest.rsplit_once(' ').map(|(_, dst)| dst).unwrap_or(rest),
            _ => rest,
        };
        out.push(PathBuf::from(path));
    }
    out
}

/// Resolve a working copy's `.jj/repo` to the shared repo directory. For the
/// default workspace this is a directory; for a secondary workspace it is a
/// file whose contents point at the main repo's `.jj/repo`.
fn jj_repo_dir(root: &Path) -> Option<PathBuf> {
    let repo = root.join(".jj").join("repo");
    let resolved = if repo.is_file() {
        // A secondary workspace: the file holds the path to the real repo dir.
        let target = std::fs::read_to_string(&repo).ok()?;
        PathBuf::from(target.trim())
    } else {
        repo
    };
    Some(resolved.canonicalize().unwrap_or(resolved))
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

/// Like [`run`], but returns raw stdout bytes (for NUL-delimited output).
fn run_bytes(root: &Path, program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
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
    fn repo_identity_none_without_vcs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(repo_identity(dir.path()).is_none());
    }

    #[test]
    fn slug_is_stable_and_path_safe() {
        let id = RepoId {
            vcs: Vcs::Git,
            path: PathBuf::from("/some/repo/.git"),
        };
        let slug = id.slug();
        assert_eq!(slug, id.slug(), "slug is stable for the same identity");
        assert!(slug.starts_with("git-"));
        assert!(!slug.contains('/'));
    }

    /// Initialize a git repo with one commit at `root`; returns false (so the
    /// test can early-return) if git is unavailable.
    fn git_init_commit(root: &Path) -> bool {
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return false;
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
        true
    }

    #[test]
    fn parse_git_porcelain_z_modified_and_untracked() {
        let bytes = b" M src/A.java\0?? src/B.java\0A  src/C.java\0";
        let got = parse_git_porcelain_z(bytes);
        assert_eq!(
            got,
            vec![
                PathBuf::from("src/A.java"),
                PathBuf::from("src/B.java"),
                PathBuf::from("src/C.java"),
            ]
        );
    }

    #[test]
    fn parse_git_porcelain_z_rename_takes_destination() {
        // A rename entry is `R  new\0old`; the destination comes first.
        let bytes = b"R  new.java\0old.java\0 M other.java\0";
        let got = parse_git_porcelain_z(bytes);
        assert_eq!(
            got,
            vec![PathBuf::from("new.java"), PathBuf::from("other.java")]
        );
    }

    #[test]
    fn parse_jj_diff_summary_variants() {
        let text = "M src/A.java\nA src/B.java\nD src/C.java\nR old.java new.java\n";
        let got = parse_jj_diff_summary(text);
        assert_eq!(
            got,
            vec![
                PathBuf::from("src/A.java"),
                PathBuf::from("src/B.java"),
                PathBuf::from("src/C.java"),
                PathBuf::from("new.java"),
            ]
        );
    }

    #[test]
    fn git_delta_lists_dirty_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !git_init_commit(root) {
            return;
        }
        // Modify the committed file and add a new one.
        std::fs::write(root.join("a.txt"), "changed").unwrap();
        std::fs::write(root.join("b.txt"), "new").unwrap();

        let delta = working_copy_delta(root);
        assert!(delta.contains(&PathBuf::from("a.txt")), "{delta:?}");
        assert!(delta.contains(&PathBuf::from("b.txt")), "{delta:?}");
    }

    #[test]
    fn git_clean_worktree_has_empty_delta() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !git_init_commit(root) {
            return;
        }
        assert!(working_copy_delta(root).is_empty());
    }

    #[test]
    fn git_worktrees_share_identity() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        if !git_init_commit(&main) {
            return;
        }
        let wt = dir.path().join("wt");
        let added = Command::new("git")
            .args(["worktree", "add", "-q", wt.to_str().unwrap()])
            .current_dir(&main)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !added {
            return;
        }

        let main_id = repo_identity(&main).expect("main identity");
        let wt_id = repo_identity(&wt).expect("worktree identity");
        assert_eq!(main_id.vcs, Vcs::Git);
        assert_eq!(
            main_id, wt_id,
            "a worktree shares its main checkout's repo identity"
        );
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
