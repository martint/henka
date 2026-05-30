//! Mapping Eclipse JDT LS (LSP) responses into the core model.
//!
//! jdtls speaks LSP, which expresses positions in UTF-16 and returns edits as a
//! `WorkspaceEdit` (either a `changes` map or `documentChanges`). These helpers
//! convert those into the core [`WorkspaceEdit`] and into structured query
//! results.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use refactor_core::{FileEdit, Position, PositionEncoding, Range, TextEdit, WorkspaceEdit};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{JavaError, Result};

#[derive(Debug, Deserialize)]
struct LspPosition {
    line: u32,
    character: u32,
}

#[derive(Debug, Deserialize)]
struct LspRange {
    start: LspPosition,
    end: LspPosition,
}

#[derive(Debug, Deserialize)]
struct LspTextEdit {
    range: LspRange,
    #[serde(rename = "newText")]
    new_text: String,
}

#[derive(Debug, Deserialize)]
struct LspTextDocument {
    uri: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LspDocumentChange {
    /// A set of edits to one document.
    Edits {
        #[serde(rename = "textDocument")]
        text_document: LspTextDocument,
        edits: Vec<LspTextEdit>,
    },
    /// A resource operation (create/rename/delete) — carries a `kind`.
    Resource { kind: String },
}

#[derive(Debug, Default, Deserialize)]
struct LspWorkspaceEdit {
    #[serde(default)]
    changes: Option<BTreeMap<String, Vec<LspTextEdit>>>,
    #[serde(rename = "documentChanges", default)]
    document_changes: Option<Vec<LspDocumentChange>>,
}

#[derive(Debug, Deserialize)]
struct LspLocation {
    uri: String,
    range: LspRange,
}

impl From<LspPosition> for Position {
    fn from(p: LspPosition) -> Self {
        Position::new(p.line, p.character)
    }
}

impl From<LspRange> for Range {
    fn from(r: LspRange) -> Self {
        Range::new(r.start.into(), r.end.into())
    }
}

impl From<LspTextEdit> for TextEdit {
    fn from(e: LspTextEdit) -> Self {
        TextEdit {
            range: e.range.into(),
            new_text: e.new_text,
        }
    }
}

/// Convert an LSP `WorkspaceEdit` JSON value into the core model.
///
/// Fails if the edit includes file-level resource operations, which the edit
/// model does not yet support (so a rename that would move a file is rejected
/// rather than partially applied).
pub fn to_core_workspace_edit(value: Value) -> Result<WorkspaceEdit> {
    if value.is_null() {
        return Ok(WorkspaceEdit::empty());
    }
    let lsp: LspWorkspaceEdit =
        serde_json::from_value(value).map_err(|e| JavaError::Lsp(e.into()))?;

    // Accumulate edits per file URI.
    let mut by_uri: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();

    if let Some(changes) = lsp.changes {
        for (uri, edits) in changes {
            by_uri
                .entry(uri)
                .or_default()
                .extend(edits.into_iter().map(TextEdit::from));
        }
    }

    if let Some(doc_changes) = lsp.document_changes {
        for change in doc_changes {
            match change {
                LspDocumentChange::Edits {
                    text_document,
                    edits,
                } => {
                    by_uri
                        .entry(text_document.uri)
                        .or_default()
                        .extend(edits.into_iter().map(TextEdit::from));
                }
                LspDocumentChange::Resource { kind } => {
                    return Err(JavaError::UnsupportedFileOperation(kind));
                }
            }
        }
    }

    let files = by_uri
        .into_iter()
        .map(|(uri, edits)| FileEdit {
            path: uri_to_path(&uri),
            edits,
        })
        .collect();

    Ok(WorkspaceEdit {
        encoding: PositionEncoding::Utf16,
        files,
    })
}

/// Convert an LSP `Location[]` response into a structured find-usages result,
/// with paths expressed relative to `root` where possible.
pub fn locations_to_query(value: Value, root: &Path) -> Result<Value> {
    if value.is_null() {
        return Ok(json!({ "usages": [] }));
    }
    let locations: Vec<LspLocation> =
        serde_json::from_value(value).map_err(|e| JavaError::Lsp(e.into()))?;

    let usages: Vec<Value> = locations
        .into_iter()
        .map(|loc| {
            let path = uri_to_path(&loc.uri);
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            json!({
                "file": rel,
                "start_line": loc.range.start.line,
                "start_character": loc.range.start.character,
                "end_line": loc.range.end.line,
                "end_character": loc.range.end.character,
            })
        })
        .collect();

    Ok(json!({ "count": usages.len(), "usages": usages }))
}

/// Convert a `file://` URI back to a path, decoding the characters we encode.
pub fn uri_to_path(uri: &str) -> PathBuf {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let mut decoded = String::with_capacity(rest.len());
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hi = chars.next();
            let lo = chars.next();
            if let (Some(hi), Some(lo)) = (hi, lo)
                && let Ok(byte) = u8::from_str_radix(&format!("{hi}{lo}"), 16)
            {
                decoded.push(byte as char);
                continue;
            }
            decoded.push('%');
            if let Some(hi) = hi {
                decoded.push(hi);
            }
            if let Some(lo) = lo {
                decoded.push(lo);
            }
        } else {
            decoded.push(c);
        }
    }
    PathBuf::from(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_changes_map() {
        let value = json!({
            "changes": {
                "file:///proj/A.java": [
                    { "range": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 7}}, "newText": "bar" }
                ]
            }
        });
        let edit = to_core_workspace_edit(value).unwrap();
        assert_eq!(edit.files.len(), 1);
        assert_eq!(edit.files[0].path, PathBuf::from("/proj/A.java"));
        assert_eq!(edit.files[0].edits[0].new_text, "bar");
    }

    #[test]
    fn maps_document_changes() {
        let value = json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": "file:///proj/B.java", "version": 1 },
                    "edits": [
                        { "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "Baz" }
                    ]
                }
            ]
        });
        let edit = to_core_workspace_edit(value).unwrap();
        assert_eq!(edit.files.len(), 1);
        assert_eq!(edit.files[0].path, PathBuf::from("/proj/B.java"));
    }

    #[test]
    fn rejects_resource_operations() {
        let value = json!({
            "documentChanges": [
                { "kind": "rename", "oldUri": "file:///proj/Old.java", "newUri": "file:///proj/New.java" }
            ]
        });
        let err = to_core_workspace_edit(value).unwrap_err();
        assert!(matches!(err, JavaError::UnsupportedFileOperation(_)));
    }

    #[test]
    fn null_edit_is_empty() {
        assert!(to_core_workspace_edit(Value::Null).unwrap().is_empty());
    }

    #[test]
    fn decodes_uri() {
        assert_eq!(
            uri_to_path("file:///a/b%20c/D.java"),
            PathBuf::from("/a/b c/D.java")
        );
    }
}
