//! Translating caller-supplied paths into the paths Henka sees.
//!
//! Henka commonly runs in a container with host working copies bind-mounted
//! under a different prefix (the host's `/home/me/src` mounted at, say,
//! `/workspaces`). A caller — an agent on the host — speaks host paths, but
//! Henka can only resolve its own in-container paths, so `register_project` on a
//! host path fails and absolute coordinates miss.
//!
//! A [`PathMap`], configured via `HENKA_PATH_MAP`, rewrites the prefixes of
//! caller-supplied absolute paths so they land on the mounted location. It is
//! deliberately generic: the mapping is a set of `host=container` prefix pairs,
//! with no knowledge of any particular mount convention — that belongs to
//! whatever sets up the container (e.g. the compose file). Format: `host=container`
//! pairs separated by commas, semicolons, or newlines, e.g.
//! `HENKA_PATH_MAP=/home/me/src=/workspaces`.

use std::path::{Path, PathBuf};

/// An ordered set of `host -> container` path-prefix rewrites.
#[derive(Debug, Clone, Default)]
pub struct PathMap {
    entries: Vec<(PathBuf, PathBuf)>,
}

impl PathMap {
    /// Build a map from the `HENKA_PATH_MAP` environment variable, or an empty
    /// (identity) map when it is unset.
    pub fn from_env() -> Self {
        std::env::var("HENKA_PATH_MAP")
            .map(|spec| Self::parse(&spec))
            .unwrap_or_default()
    }

    /// Parse a `host=container,host=container` specification. Entries without a
    /// `=`, or with an empty side, are ignored.
    pub fn parse(spec: &str) -> Self {
        let entries = spec
            .split([',', ';', '\n'])
            .filter_map(|entry| {
                let (host, container) = entry.trim().split_once('=')?;
                let (host, container) = (host.trim(), container.trim());
                (!host.is_empty() && !container.is_empty())
                    .then(|| (PathBuf::from(host), PathBuf::from(container)))
            })
            .collect();
        Self { entries }
    }

    /// Whether the map performs no rewrites.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The container-side prefix of each rewrite (the right-hand sides) — the
    /// locations Henka actually sees mounted working copies under. Used to
    /// derive where to look for projects to auto-register.
    pub fn container_prefixes(&self) -> Vec<PathBuf> {
        self.entries.iter().map(|(_, c)| c.clone()).collect()
    }

    /// Rewrite `path` by its longest matching host prefix, or return it
    /// unchanged when no prefix matches (including for relative paths).
    pub fn map(&self, path: &Path) -> PathBuf {
        let mut best: Option<(usize, PathBuf)> = None;
        for (host, container) in &self.entries {
            if let Ok(rest) = path.strip_prefix(host) {
                let depth = host.components().count();
                if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                    best = Some((depth, container.join(rest)));
                }
            }
        }
        best.map_or_else(|| path.to_path_buf(), |(_, mapped)| mapped)
    }

    /// Rewrite a container path back to its caller-side (host) path by the
    /// longest matching container prefix, or `None` when no prefix matches. The
    /// inverse of [`map`](PathMap::map), for showing a caller which of its own
    /// paths a mounted project root corresponds to.
    pub fn reverse(&self, path: &Path) -> Option<PathBuf> {
        let mut best: Option<(usize, PathBuf)> = None;
        for (host, container) in &self.entries {
            if let Ok(rest) = path.strip_prefix(container) {
                let depth = container.components().count();
                if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                    best = Some((depth, host.join(rest)));
                }
            }
        }
        best.map(|(_, mapped)| mapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_longest_matching_prefix() {
        // A more specific mapping wins over a broader one regardless of order.
        let map = PathMap::parse("/home/me=/broad, /home/me/src=/workspaces");
        assert_eq!(
            map.map(Path::new("/home/me/src/trino.multiset")),
            PathBuf::from("/workspaces/trino.multiset")
        );
        // The prefix itself maps to the bare container root.
        assert_eq!(
            map.map(Path::new("/home/me/src")),
            PathBuf::from("/workspaces")
        );
        // A path under only the broader mapping uses it.
        assert_eq!(
            map.map(Path::new("/home/me/docs")),
            PathBuf::from("/broad/docs")
        );
    }

    #[test]
    fn reverse_maps_container_paths_back_to_host() {
        let map = PathMap::parse("/home/me/src=/workspaces, /data/repos=/mnt/repos");
        assert_eq!(
            map.reverse(Path::new("/workspaces/trino.multiset")),
            Some(PathBuf::from("/home/me/src/trino.multiset"))
        );
        assert_eq!(
            map.reverse(Path::new("/mnt/repos/svc")),
            Some(PathBuf::from("/data/repos/svc"))
        );
        // A path under no container prefix has no host counterpart.
        assert_eq!(map.reverse(Path::new("/elsewhere/x")), None);
    }

    #[test]
    fn leaves_unmatched_and_relative_paths_alone() {
        let map = PathMap::parse("/home/me/src=/workspaces");
        // Not under any host prefix (and not a component boundary match).
        assert_eq!(
            map.map(Path::new("/home/me/srcfoo")),
            PathBuf::from("/home/me/srcfoo")
        );
        assert_eq!(map.map(Path::new("rel/path")), PathBuf::from("rel/path"));
    }

    #[test]
    fn empty_and_malformed_specs_are_identity() {
        assert!(PathMap::parse("").is_empty());
        assert!(PathMap::parse("no-equals-sign, =/c, /h=").is_empty());
        let map = PathMap::parse("");
        assert_eq!(map.map(Path::new("/a/b")), PathBuf::from("/a/b"));
    }

    #[test]
    fn trailing_empty_entry_is_ignored() {
        // The compose file always appends a `,` before optional extra rewrites,
        // leaving a trailing empty entry when there are none — it must be a no-op.
        let map = PathMap::parse("/home/me/src=/workspaces,");
        assert_eq!(
            map.map(Path::new("/home/me/src/proj")),
            PathBuf::from("/workspaces/proj")
        );
    }
}
