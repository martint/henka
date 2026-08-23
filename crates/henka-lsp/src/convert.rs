//! Mapping LSP responses into the core model.
//!
//! Language servers speak LSP, which expresses positions in UTF-16 and returns
//! edits as a `WorkspaceEdit` (either a `changes` map or `documentChanges`).
//! These helpers convert those into the core [`WorkspaceEdit`] and into
//! structured query results, so every LSP-backed provider shares one mapping.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use henka_core::{
    FileEdit, FileOperation, Position, PositionEncoding, Range, TextEdit, WorkspaceEdit,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Result;

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
    Resource {
        kind: String,
        #[serde(default)]
        uri: Option<String>,
        #[serde(rename = "oldUri", default)]
        old_uri: Option<String>,
        #[serde(rename = "newUri", default)]
        new_uri: Option<String>,
    },
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

/// A `workspace/symbol` match's location. LSP 3.17 allows a `WorkspaceSymbol`
/// to report just a `uri`, leaving `range` for a later `workspaceSymbol/resolve`
/// call; Henka doesn't advertise that capability, so such a symbol is dropped
/// rather than resolved.
#[derive(Debug, Deserialize)]
struct LspSymbolLocation {
    uri: String,
    #[serde(default)]
    range: Option<LspRange>,
}

#[derive(Debug, Deserialize)]
struct LspSymbolInfo {
    name: String,
    kind: u32,
    location: LspSymbolLocation,
    #[serde(rename = "containerName", default)]
    container_name: Option<String>,
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

/// Convert an LSP `WorkspaceEdit` JSON value into the core model, including any
/// file-level resource operations (create/rename/delete).
pub fn to_core_workspace_edit(value: Value) -> Result<WorkspaceEdit> {
    if value.is_null() {
        return Ok(WorkspaceEdit::empty());
    }
    let lsp: LspWorkspaceEdit = serde_json::from_value(value)?;

    // Accumulate edits per file URI, preserving any file operations in order.
    let mut by_uri: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();
    let mut file_ops: Vec<FileOperation> = Vec::new();

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
                LspDocumentChange::Resource {
                    kind,
                    uri,
                    old_uri,
                    new_uri,
                } => {
                    if let Some(op) = resource_op(&kind, uri, old_uri, new_uri) {
                        file_ops.push(op);
                    }
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
        file_ops,
    })
}

/// Map an LSP resource operation to a core [`FileOperation`].
fn resource_op(
    kind: &str,
    uri: Option<String>,
    old_uri: Option<String>,
    new_uri: Option<String>,
) -> Option<FileOperation> {
    match kind {
        "create" => Some(FileOperation::Create {
            path: uri_to_path(&uri?),
        }),
        "delete" => Some(FileOperation::Delete {
            path: uri_to_path(&uri?),
        }),
        "rename" => Some(FileOperation::Rename {
            from: uri_to_path(&old_uri?),
            to: uri_to_path(&new_uri?),
        }),
        _ => None,
    }
}

/// Convert an LSP `Location[]` response into a structured find-usages result,
/// with paths expressed relative to `root` where possible.
pub fn locations_to_query(value: Value, root: &Path) -> Result<Value> {
    if value.is_null() {
        return Ok(json!({ "usages": [] }));
    }
    let locations: Vec<LspLocation> = serde_json::from_value(value)?;

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

/// The default cap on the number of symbols `symbols_to_query` returns when
/// the caller doesn't specify a `limit`.
pub const DEFAULT_SYMBOL_SEARCH_LIMIT: usize = 200;

/// Convert an LSP `SymbolInformation[]` (or `WorkspaceSymbol[]`) response
/// from `workspace/symbol` into a structured symbol-search result, with
/// paths expressed relative to `root` where possible and matches capped at
/// `limit`.
///
/// `count` always reports the total number matched, even when the returned
/// `symbols` list is truncated to `limit` — a truncated response also carries
/// `"truncated": true` so the caller knows to narrow its query rather than
/// assuming it saw everything.
pub fn symbols_to_query(value: Value, root: &Path, limit: usize) -> Result<Value> {
    if value.is_null() {
        return Ok(json!({ "count": 0, "symbols": [] }));
    }
    let items: Vec<LspSymbolInfo> = serde_json::from_value(value)?;

    // A symbol without a range is a `WorkspaceSymbol` awaiting resolve; Henka
    // doesn't resolve it, so drop it instead of failing the batch.
    let matched: Vec<(LspSymbolInfo, LspRange)> = items
        .into_iter()
        .filter_map(|mut s| s.location.range.take().map(|range| (s, range)))
        .collect();

    // Count first, cap second, build last: the matches past the cap are never
    // returned, so nothing should be spent describing them.
    let total = matched.len();
    let truncated = total > limit;

    let symbols: Vec<Value> = matched
        .into_iter()
        .take(limit)
        .map(|(s, range)| {
            let path = uri_to_path(&s.location.uri);
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let mut obj = json!({
                "name": s.name,
                "kind": symbol_kind_name(s.kind),
                "file": rel,
                "start_line": range.start.line,
                "start_character": range.start.character,
                "end_line": range.end.line,
                "end_character": range.end.character,
            });
            if let Some(container) = s.container_name {
                obj["container_name"] = json!(container);
            }
            obj
        })
        .collect();

    let mut out = json!({ "count": total, "symbols": symbols });
    if truncated {
        out["truncated"] = json!(true);
    }
    Ok(out)
}

/// Map an LSP `SymbolKind` (1-26) to a lowercase name, so a result is
/// self-describing without the caller needing the LSP spec memorized.
fn symbol_kind_name(kind: u32) -> String {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum_member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        other => return other.to_string(),
    }
    .to_string()
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
    fn maps_rename_file_operation() {
        let value = json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": "file:///proj/Old.java", "version": 1 },
                    "edits": [
                        { "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 9}}, "newText": "New" }
                    ]
                },
                { "kind": "rename", "oldUri": "file:///proj/Old.java", "newUri": "file:///proj/New.java" }
            ]
        });
        let edit = to_core_workspace_edit(value).unwrap();
        assert_eq!(edit.files.len(), 1);
        assert_eq!(
            edit.file_ops,
            vec![FileOperation::Rename {
                from: PathBuf::from("/proj/Old.java"),
                to: PathBuf::from("/proj/New.java"),
            }]
        );
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

    #[test]
    fn null_symbol_search_is_empty() {
        let out = symbols_to_query(Value::Null, Path::new("/proj"), 10).unwrap();
        assert_eq!(out, json!({ "count": 0, "symbols": [] }));
    }

    #[test]
    fn symbol_path_relative_to_root() {
        let value = json!([{
            "name": "Foo",
            "kind": 5,
            "location": {
                "uri": "file:///proj/src/Foo.java",
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}
            }
        }]);
        let out = symbols_to_query(value, Path::new("/proj"), 10).unwrap();
        assert_eq!(out["symbols"][0]["file"], json!("src/Foo.java"));
        assert_eq!(out["symbols"][0]["kind"], json!("class"));
    }

    #[test]
    fn symbol_path_outside_root_stays_absolute() {
        let value = json!([{
            "name": "Foo",
            "kind": 5,
            "location": {
                "uri": "file:///other/Foo.java",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}
            }
        }]);
        let out = symbols_to_query(value, Path::new("/proj"), 10).unwrap();
        assert_eq!(out["symbols"][0]["file"], json!("/other/Foo.java"));
    }

    #[test]
    fn container_name_present_and_absent() {
        let value = json!([
            {
                "name": "bar",
                "kind": 6,
                "location": {
                    "uri": "file:///proj/Foo.java",
                    "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 3}}
                },
                "containerName": "Foo"
            },
            {
                "name": "Foo",
                "kind": 5,
                "location": {
                    "uri": "file:///proj/Foo.java",
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}
                }
            }
        ]);
        let out = symbols_to_query(value, Path::new("/proj"), 10).unwrap();
        assert_eq!(out["symbols"][0]["container_name"], json!("Foo"));
        assert!(out["symbols"][1].get("container_name").is_none());
    }

    #[test]
    fn symbol_missing_range_is_dropped_not_fatal() {
        let value = json!([
            {
                "name": "Unresolved",
                "kind": 5,
                "location": { "uri": "file:///proj/Foo.java" }
            },
            {
                "name": "Resolved",
                "kind": 5,
                "location": {
                    "uri": "file:///proj/Foo.java",
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}
                }
            }
        ]);
        let out = symbols_to_query(value, Path::new("/proj"), 10).unwrap();
        assert_eq!(out["count"], json!(1));
        assert_eq!(out["symbols"][0]["name"], json!("Resolved"));
    }

    #[test]
    fn symbol_search_truncates_and_reports_total() {
        let matches: Vec<Value> = (0..5)
            .map(|i| {
                json!({
                    "name": format!("Foo{i}"),
                    "kind": 5,
                    "location": {
                        "uri": format!("file:///proj/Foo{i}.java"),
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}
                    }
                })
            })
            .collect();
        let out = symbols_to_query(json!(matches), Path::new("/proj"), 2).unwrap();
        assert_eq!(out["count"], json!(5));
        assert_eq!(out["symbols"].as_array().unwrap().len(), 2);
        assert_eq!(out["truncated"], json!(true));
    }
}
