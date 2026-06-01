//! The workspace-edit model and its application to the working tree.
//!
//! An [`WorkspaceEdit`] is the single currency every edit operation returns: an
//! ordered set of text changes across one or more files, expressed in
//! line/character coordinates with an explicit [`PositionEncoding`] (LSP-style
//! backends use UTF-16). [`EditApplier`] turns one into either a unified-diff
//! [preview](EditApplier::preview) or an in-place [apply](EditApplier::apply).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// How the `character` field of a [`Position`] is counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionEncoding {
    /// UTF-8 bytes.
    Utf8,
    /// UTF-16 code units (the LSP default).
    Utf16,
    /// Unicode scalar values (code points).
    Utf32,
}

/// A zero-based position in a text document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based offset within the line, counted per the edit's
    /// [`PositionEncoding`].
    pub character: u32,
}

impl Position {
    /// Construct a position.
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A half-open range `[start, end)` in a text document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    /// Start position (inclusive).
    pub start: Position,
    /// End position (exclusive).
    pub end: Position,
}

impl Range {
    /// Construct a range.
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }
}

/// A single replacement of `range` with `new_text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    /// The range to replace.
    pub range: Range,
    /// The replacement text (empty to delete).
    pub new_text: String,
}

/// All edits to a single file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    /// File the edits apply to. Resolved relative to the project root unless
    /// absolute.
    pub path: PathBuf,
    /// Edits within the file, in any order; the applier orders them safely.
    pub edits: Vec<TextEdit>,
}

/// A file-level operation that accompanies text edits (e.g. a rename
/// refactoring that also renames the file holding a public class).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum FileOperation {
    /// Create a new (empty) file.
    Create {
        /// Path to create.
        path: PathBuf,
    },
    /// Rename or move a file.
    Rename {
        /// Existing path.
        from: PathBuf,
        /// New path.
        to: PathBuf,
    },
    /// Delete a file.
    Delete {
        /// Path to delete.
        path: PathBuf,
    },
}

/// An ordered set of changes across one or more files: text edits plus any
/// file-level operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEdit {
    /// How positions in this edit are counted.
    pub encoding: PositionEncoding,
    /// Per-file text edits.
    pub files: Vec<FileEdit>,
    /// File-level operations, applied after the text edits.
    #[serde(default)]
    pub file_ops: Vec<FileOperation>,
}

impl WorkspaceEdit {
    /// An empty edit (UTF-16 by convention).
    pub fn empty() -> Self {
        Self {
            encoding: PositionEncoding::Utf16,
            files: Vec::new(),
            file_ops: Vec::new(),
        }
    }

    /// Whether this edit changes nothing.
    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|f| f.edits.is_empty()) && self.file_ops.is_empty()
    }

    /// Re-root every absolute path under `from_root` to the same relative
    /// location under `to_root`, so an edit computed against one checkout can be
    /// applied to a sibling working copy. Paths not under `from_root` (relative
    /// paths, or absolute paths elsewhere such as JDK sources) are left as-is.
    pub fn retarget(&mut self, from_root: &Path, to_root: &Path) {
        for file in &mut self.files {
            file.path = retarget_path(&file.path, from_root, to_root);
        }
        for op in &mut self.file_ops {
            match op {
                FileOperation::Create { path } | FileOperation::Delete { path } => {
                    *path = retarget_path(path, from_root, to_root);
                }
                FileOperation::Rename { from, to } => {
                    *from = retarget_path(from, from_root, to_root);
                    *to = retarget_path(to, from_root, to_root);
                }
            }
        }
    }
}

/// Re-root `path` from `from_root` to `to_root` if it lies under `from_root`;
/// otherwise return it unchanged.
fn retarget_path(path: &Path, from_root: &Path, to_root: &Path) -> PathBuf {
    match path.strip_prefix(from_root) {
        Ok(rel) => to_root.join(rel),
        Err(_) => path.to_path_buf(),
    }
}

/// The unified diff for a single file produced by [`EditApplier::preview`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// File path, relative to the project root where possible.
    pub path: PathBuf,
    /// Unified-diff text, or empty if the file is unchanged.
    pub diff: String,
}

/// Summary of an applied edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedEdit {
    /// Paths that were changed, relative to the project root where possible.
    pub changed_files: Vec<PathBuf>,
}

/// Applies [`WorkspaceEdit`]s against a project root.
pub struct EditApplier;

impl EditApplier {
    /// Compute the unified diff each file edit would produce, without touching
    /// disk.
    pub fn preview(edit: &WorkspaceEdit, root: &Path) -> Result<Vec<FileDiff>> {
        let mut diffs = Vec::new();
        for file in &edit.files {
            let (original, updated) = Self::rewrite_file(file, edit.encoding, root)?;
            let rel = relativize(&file.path, root);
            let diff = unified_diff(&original, &updated, &rel);
            diffs.push(FileDiff { path: rel, diff });
        }
        for op in &edit.file_ops {
            diffs.push(file_op_diff(op, root));
        }
        Ok(diffs)
    }

    /// Apply the edit to the working tree in place. Files are rewritten only if
    /// their content actually changes.
    pub fn apply(edit: &WorkspaceEdit, root: &Path) -> Result<AppliedEdit> {
        // Compute every rewrite first so a failure leaves the tree untouched.
        let mut planned: Vec<(PathBuf, PathBuf, String)> = Vec::new();
        for file in &edit.files {
            let (original, updated) = Self::rewrite_file(file, edit.encoding, root)?;
            if original != updated {
                let abs = resolve_path(&file.path, root);
                planned.push((abs, relativize(&file.path, root), updated));
            }
        }

        let mut changed_files = Vec::new();
        for (abs, rel, updated) in planned {
            std::fs::write(&abs, updated)?;
            changed_files.push(rel);
        }

        // File-level operations run after text edits, so edits to a file that
        // is about to be renamed land before the rename.
        for op in &edit.file_ops {
            match op {
                FileOperation::Create { path } => {
                    let abs = resolve_path(path, root);
                    if !abs.exists() {
                        if let Some(parent) = abs.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&abs, "")?;
                    }
                    changed_files.push(relativize(path, root));
                }
                FileOperation::Rename { from, to } => {
                    let from_abs = resolve_path(from, root);
                    let to_abs = resolve_path(to, root);
                    if let Some(parent) = to_abs.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::rename(&from_abs, &to_abs)?;
                    changed_files.push(relativize(to, root));
                }
                FileOperation::Delete { path } => {
                    let abs = resolve_path(path, root);
                    if abs.exists() {
                        std::fs::remove_file(&abs)?;
                    }
                    changed_files.push(relativize(path, root));
                }
            }
        }
        Ok(AppliedEdit { changed_files })
    }

    /// Read a file and return `(original, updated)` content after applying its
    /// edits. Detects overlaps and out-of-range positions.
    fn rewrite_file(
        file: &FileEdit,
        encoding: PositionEncoding,
        root: &Path,
    ) -> Result<(String, String)> {
        let abs = resolve_path(&file.path, root);
        let original = std::fs::read_to_string(&abs)?;

        // Resolve every edit to a byte range.
        let mut byte_edits: Vec<(usize, usize, &str)> = Vec::with_capacity(file.edits.len());
        for e in &file.edits {
            let start = resolve_offset(&original, e.range.start, encoding).ok_or_else(|| {
                Error::PositionOutOfRange {
                    path: abs.clone(),
                    line: e.range.start.line,
                    character: e.range.start.character,
                }
            })?;
            let end = resolve_offset(&original, e.range.end, encoding).ok_or_else(|| {
                Error::PositionOutOfRange {
                    path: abs.clone(),
                    line: e.range.end.line,
                    character: e.range.end.character,
                }
            })?;
            byte_edits.push((start, end, e.new_text.as_str()));
        }

        // Order by start; reject overlaps.
        byte_edits.sort_by_key(|(start, _, _)| *start);
        for window in byte_edits.windows(2) {
            if window[0].1 > window[1].0 {
                return Err(Error::OverlappingEdits(abs));
            }
        }

        // Splice from the end so earlier offsets stay valid.
        let mut updated = original.clone();
        for (start, end, new_text) in byte_edits.into_iter().rev() {
            updated.replace_range(start..end, new_text);
        }
        Ok((original, updated))
    }
}

/// Resolve a (line, character) position to a byte offset in `text`, honoring
/// the position encoding. Returns `None` if the line is out of range.
fn resolve_offset(text: &str, pos: Position, encoding: PositionEncoding) -> Option<usize> {
    let line_start = nth_line_start(text, pos.line)?;
    let line = &text[line_start..];
    // Restrict to this line's content (exclude the trailing newline).
    let line_len = line.find('\n').unwrap_or(line.len());
    let content = &line[..line_len];
    let content = content.strip_suffix('\r').unwrap_or(content);

    let mut units = 0u32;
    let mut byte = line_start;
    for ch in content.chars() {
        if units >= pos.character {
            break;
        }
        units += encoded_len(ch, encoding);
        byte += ch.len_utf8();
    }
    // Positions past end-of-line clamp to end-of-content (LSP permits this).
    Some(byte)
}

/// Byte offset of the start of line `n` (zero-based), or `None` if out of range.
fn nth_line_start(text: &str, n: u32) -> Option<usize> {
    if n == 0 {
        return Some(0);
    }
    let mut seen = 0u32;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            seen += 1;
            if seen == n {
                return Some(i + 1);
            }
        }
    }
    None
}

/// The number of encoding units a character occupies.
fn encoded_len(ch: char, encoding: PositionEncoding) -> u32 {
    match encoding {
        PositionEncoding::Utf8 => ch.len_utf8() as u32,
        PositionEncoding::Utf16 => ch.len_utf16() as u32,
        PositionEncoding::Utf32 => 1,
    }
}

/// Whether `ch` can appear in a source identifier (letters, digits, `_`, `$`).
fn is_identifier_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

/// The identifier `text` contains at `pos`, or `None` if `pos` does not touch
/// one (it lands in whitespace, punctuation, or out of range).
///
/// A position anywhere within an identifier — including just past its last
/// character, as LSP permits — resolves to the whole identifier. This is used to
/// validate that a caller-supplied coordinate refers to the symbol it intended,
/// catching coordinates computed against a different revision of the file.
pub fn identifier_at(text: &str, pos: Position, encoding: PositionEncoding) -> Option<String> {
    let offset = resolve_offset(text, pos, encoding)?;
    let mut start = offset;
    for (i, ch) in text[..offset].char_indices().rev() {
        if is_identifier_char(ch) {
            start = i;
        } else {
            break;
        }
    }
    let mut end = offset;
    for ch in text[offset..].chars() {
        if is_identifier_char(ch) {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    (start != end).then(|| text[start..end].to_string())
}

/// The slice of `text` covered by `range`, or `None` if either endpoint is out
/// of range or the range is inverted.
pub fn text_in_range(text: &str, range: Range, encoding: PositionEncoding) -> Option<String> {
    let start = resolve_offset(text, range.start, encoding)?;
    let end = resolve_offset(text, range.end, encoding)?;
    (start <= end).then(|| text[start..end].to_string())
}

/// Resolve an edit path against the project root.
fn resolve_path(path: &Path, root: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// Express a path relative to the project root when possible.
fn relativize(path: &Path, root: &Path) -> PathBuf {
    let abs = resolve_path(path, root);
    abs.strip_prefix(root).map(Path::to_path_buf).unwrap_or(abs)
}

/// Describe a file operation as a one-line diff entry for previews.
fn file_op_diff(op: &FileOperation, root: &Path) -> FileDiff {
    match op {
        FileOperation::Create { path } => {
            let rel = relativize(path, root);
            FileDiff {
                diff: format!("create {}", rel.display()),
                path: rel,
            }
        }
        FileOperation::Rename { from, to } => {
            let rel_to = relativize(to, root);
            FileDiff {
                diff: format!(
                    "rename {} -> {}",
                    relativize(from, root).display(),
                    rel_to.display()
                ),
                path: rel_to,
            }
        }
        FileOperation::Delete { path } => {
            let rel = relativize(path, root);
            FileDiff {
                diff: format!("delete {}", rel.display()),
                path: rel,
            }
        }
    }
}

/// Produce a unified diff between two texts, labeled with the file path.
fn unified_diff(original: &str, updated: &str, path: &Path) -> String {
    if original == updated {
        return String::new();
    }
    let display = path.display().to_string();
    let diff = similar::TextDiff::from_lines(original, updated);
    diff.unified_diff()
        .context_radius(3)
        .header(&format!("a/{display}"), &format!("b/{display}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, ch: u32) -> Position {
        Position::new(line, ch)
    }

    #[test]
    fn identifier_at_resolves_whole_identifier() {
        let text = "    boolean isCovariantTypeBase(Type typeBase) {\n";
        let enc = PositionEncoding::Utf16;
        // A coordinate landing inside the method name resolves to all of it,
        // whether at its start, middle, or just past its end.
        let name = "isCovariantTypeBase";
        let start = 12; // column of `isCovariantTypeBase`
        assert_eq!(identifier_at(text, pos(0, start), enc).as_deref(), Some(name));
        assert_eq!(
            identifier_at(text, pos(0, start + 5), enc).as_deref(),
            Some(name)
        );
        assert_eq!(
            identifier_at(text, pos(0, start + name.len() as u32), enc).as_deref(),
            Some(name)
        );
        // A different column lands on the parameter, not the method.
        assert_eq!(identifier_at(text, pos(0, 37), enc).as_deref(), Some("typeBase"));
        // Whitespace and out-of-range lines touch no identifier.
        assert_eq!(identifier_at(text, pos(0, 0), enc), None);
        assert_eq!(identifier_at(text, pos(9, 0), enc), None);
    }

    #[test]
    fn text_in_range_returns_selected_slice() {
        let text = "let answer = 42;\n";
        let enc = PositionEncoding::Utf16;
        assert_eq!(
            text_in_range(text, Range::new(pos(0, 4), pos(0, 10)), enc).as_deref(),
            Some("answer")
        );
        // Inverted ranges are rejected.
        assert_eq!(text_in_range(text, Range::new(pos(0, 10), pos(0, 4)), enc), None);
    }

    fn single_file_edit(root: &Path, name: &str, edits: Vec<TextEdit>) -> WorkspaceEdit {
        WorkspaceEdit {
            encoding: PositionEncoding::Utf16,
            files: vec![FileEdit {
                path: root.join(name),
                edits,
            }],
            file_ops: Vec::new(),
        }
    }

    #[test]
    fn applies_single_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello world\n").unwrap();

        let edit = single_file_edit(
            dir.path(),
            "a.txt",
            vec![TextEdit {
                range: Range::new(pos(0, 6), pos(0, 11)),
                new_text: "there".into(),
            }],
        );
        let applied = EditApplier::apply(&edit, dir.path()).unwrap();
        assert_eq!(applied.changed_files, vec![PathBuf::from("a.txt")]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello there\n");
    }

    #[test]
    fn applies_multiple_edits_in_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "aaa bbb ccc\n").unwrap();

        // Replace "aaa" and "ccc"; given out of order, applier must order them.
        let edit = single_file_edit(
            dir.path(),
            "a.txt",
            vec![
                TextEdit {
                    range: Range::new(pos(0, 8), pos(0, 11)),
                    new_text: "ZZZ".into(),
                },
                TextEdit {
                    range: Range::new(pos(0, 0), pos(0, 3)),
                    new_text: "AAA".into(),
                },
            ],
        );
        EditApplier::apply(&edit, dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "AAA bbb ZZZ\n");
    }

    #[test]
    fn rejects_overlapping_edits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();

        let edit = single_file_edit(
            dir.path(),
            "a.txt",
            vec![
                TextEdit {
                    range: Range::new(pos(0, 0), pos(0, 5)),
                    new_text: "x".into(),
                },
                TextEdit {
                    range: Range::new(pos(0, 3), pos(0, 8)),
                    new_text: "y".into(),
                },
            ],
        );
        let err = EditApplier::apply(&edit, dir.path()).unwrap_err();
        assert!(matches!(err, Error::OverlappingEdits(_)));
    }

    #[test]
    fn utf16_offsets_account_for_astral_chars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        // "💡" is one scalar, two UTF-16 units, four UTF-8 bytes.
        std::fs::write(&path, "💡x\n").unwrap();

        // In UTF-16, the 'x' starts at character 2.
        let edit = single_file_edit(
            dir.path(),
            "a.txt",
            vec![TextEdit {
                range: Range::new(pos(0, 2), pos(0, 3)),
                new_text: "Y".into(),
            }],
        );
        EditApplier::apply(&edit, dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "💡Y\n");
    }

    #[test]
    fn preview_does_not_write_and_shows_diff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello world\n").unwrap();

        let edit = single_file_edit(
            dir.path(),
            "a.txt",
            vec![TextEdit {
                range: Range::new(pos(0, 6), pos(0, 11)),
                new_text: "there".into(),
            }],
        );
        let diffs = EditApplier::preview(&edit, dir.path()).unwrap();
        // File is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world\n");
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].path, PathBuf::from("a.txt"));
        assert!(diffs[0].diff.contains("-hello world"));
        assert!(diffs[0].diff.contains("+hello there"));
    }

    #[test]
    fn applies_text_edit_then_renames_file() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("Greeting.java");
        std::fs::write(&old, "class Greeting {}\n").unwrap();

        let edit = WorkspaceEdit {
            encoding: PositionEncoding::Utf16,
            files: vec![FileEdit {
                path: old.clone(),
                edits: vec![TextEdit {
                    range: Range::new(pos(0, 6), pos(0, 14)),
                    new_text: "Salutation".into(),
                }],
            }],
            file_ops: vec![FileOperation::Rename {
                from: old.clone(),
                to: dir.path().join("Salutation.java"),
            }],
        };

        EditApplier::apply(&edit, dir.path()).unwrap();
        assert!(!old.exists(), "old file removed");
        let new = dir.path().join("Salutation.java");
        assert_eq!(
            std::fs::read_to_string(&new).unwrap(),
            "class Salutation {}\n"
        );
    }

    #[test]
    fn retarget_rewrites_file_and_op_paths() {
        let mut edit = WorkspaceEdit {
            encoding: PositionEncoding::Utf16,
            files: vec![FileEdit {
                path: PathBuf::from("/base/src/A.java"),
                edits: vec![],
            }],
            file_ops: vec![FileOperation::Rename {
                from: PathBuf::from("/base/src/Old.java"),
                to: PathBuf::from("/base/src/New.java"),
            }],
        };
        edit.retarget(Path::new("/base"), Path::new("/wt"));
        assert_eq!(edit.files[0].path, PathBuf::from("/wt/src/A.java"));
        match &edit.file_ops[0] {
            FileOperation::Rename { from, to } => {
                assert_eq!(from, &PathBuf::from("/wt/src/Old.java"));
                assert_eq!(to, &PathBuf::from("/wt/src/New.java"));
            }
            other => panic!("unexpected op {other:?}"),
        }
    }

    #[test]
    fn retarget_leaves_foreign_paths() {
        let mut edit = WorkspaceEdit {
            encoding: PositionEncoding::Utf16,
            files: vec![FileEdit {
                path: PathBuf::from("/usr/lib/jvm/src/java/lang/String.java"),
                edits: vec![],
            }],
            file_ops: vec![],
        };
        edit.retarget(Path::new("/base"), Path::new("/wt"));
        assert_eq!(
            edit.files[0].path,
            PathBuf::from("/usr/lib/jvm/src/java/lang/String.java"),
            "paths outside the source root are untouched"
        );
    }

    #[test]
    fn retarget_then_apply_writes_to_target() {
        // The session checkout (`base`) does not even exist on disk; the edit
        // must land in the target working copy after retargeting.
        let base = PathBuf::from("/nonexistent-base-checkout");
        let to = tempfile::tempdir().unwrap();
        let target_file = to.path().join("a.txt");
        std::fs::write(&target_file, "hello world\n").unwrap();

        let mut edit = WorkspaceEdit {
            encoding: PositionEncoding::Utf16,
            files: vec![FileEdit {
                path: base.join("a.txt"),
                edits: vec![TextEdit {
                    range: Range::new(pos(0, 6), pos(0, 11)),
                    new_text: "there".into(),
                }],
            }],
            file_ops: vec![],
        };
        edit.retarget(&base, to.path());
        let applied = EditApplier::apply(&edit, to.path()).unwrap();

        assert_eq!(applied.changed_files, vec![PathBuf::from("a.txt")]);
        assert_eq!(std::fs::read_to_string(&target_file).unwrap(), "hello there\n");
    }

    #[test]
    fn out_of_range_line_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one line\n").unwrap();

        let edit = single_file_edit(
            dir.path(),
            "a.txt",
            vec![TextEdit {
                range: Range::new(pos(5, 0), pos(5, 1)),
                new_text: "x".into(),
            }],
        );
        let err = EditApplier::apply(&edit, dir.path()).unwrap_err();
        assert!(matches!(err, Error::PositionOutOfRange { .. }));
    }
}
