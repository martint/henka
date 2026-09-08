//! Mapping LSP responses into the core model.
//!
//! Language servers speak LSP, which expresses positions in UTF-16 and returns
//! edits as a `WorkspaceEdit` (either a `changes` map or `documentChanges`).
//! These helpers convert those into the core [`WorkspaceEdit`] and into
//! structured query results, so every LSP-backed provider shares one mapping.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use henka_core::{
    FileEdit, FileOperation, Language, LanguageRoute, Position, PositionEncoding, Range, TextEdit,
    WorkspaceEdit,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Result;

#[derive(Debug, Clone, Copy, Deserialize)]
struct LspPosition {
    line: u32,
    character: u32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
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

/// One end of a goto response. `textDocument/definition` and its siblings may
/// answer with a `Location` (`uri`/`range`) or a `LocationLink`
/// (`targetUri`/`targetRange`/`targetSelectionRange`); which one arrives is a
/// property of the server, not of the question asked, so both are accepted.
#[derive(Debug, Deserialize)]
struct LspGotoTarget {
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    range: Option<LspRange>,
    #[serde(rename = "targetUri", default)]
    target_uri: Option<String>,
    #[serde(rename = "targetRange", default)]
    target_range: Option<LspRange>,
    #[serde(rename = "targetSelectionRange", default)]
    target_selection_range: Option<LspRange>,
}

/// A goto response as it arrives: one target, or a list of them.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LspGotoResponse {
    Many(Vec<LspGotoTarget>),
    One(LspGotoTarget),
}

/// A `textDocument/hover` response body. LSP has accumulated three content
/// shapes here — a `MarkupContent`, a bare or language-tagged `MarkedString`,
/// or an array of those — and servers in use still send each of them, so all
/// three are accepted and collapsed into one markdown string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LspHoverContents {
    // Ordered before `Markup`: a `MarkedString` object carries both `language`
    // and `value`, so it would otherwise match `Markup`'s optional `kind` and
    // lose its fence.
    Fenced {
        language: String,
        value: String,
    },
    Markup {
        value: String,
    },
    Plain(String),
    Many(Vec<LspHoverContents>),
}

#[derive(Debug, Deserialize)]
struct LspHover {
    contents: LspHoverContents,
    #[serde(default)]
    range: Option<LspRange>,
}

/// A `textDocument/documentSymbol` entry. Servers answer with either the
/// hierarchical `DocumentSymbol` (`range`/`selectionRange`/`children`) or the
/// older flat `SymbolInformation` (`location`/`containerName`); both are
/// accepted, since which one arrives is a property of the server.
#[derive(Debug, Deserialize)]
struct LspDocumentSymbol {
    name: String,
    kind: u32,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    range: Option<LspRange>,
    #[serde(rename = "selectionRange", default)]
    selection_range: Option<LspRange>,
    #[serde(default)]
    children: Option<Vec<LspDocumentSymbol>>,
    /// Present only on the flat `SymbolInformation` form.
    #[serde(default)]
    location: Option<LspLocation>,
    #[serde(rename = "containerName", default)]
    container_name: Option<String>,
}

/// A `CallHierarchyItem` — the language server's own handle on one declaration,
/// returned by `textDocument/prepareCallHierarchy` and required back verbatim
/// on the follow-up call.
#[derive(Debug, Deserialize)]
struct LspCallHierarchyItem {
    name: String,
    kind: u32,
    #[serde(default)]
    detail: Option<String>,
    uri: String,
    range: LspRange,
    #[serde(rename = "selectionRange")]
    selection_range: LspRange,
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

/// The most characters of a single source line a result will carry. A minified
/// or generated file should cost the caller one line's worth of context, not a
/// screenful.
const MAX_TEXT_CHARS: usize = 500;

/// The largest window `context_lines` may ask for on either side of a location,
/// so a wide window and a large `limit` can't combine into an unbounded result.
pub const MAX_CONTEXT_LINES: usize = 10;

/// Where a result's source text comes from, and how much of it to quote.
///
/// A coordinate on its own is not a useful answer to a caller with no editor
/// open: it has to read the file to see what is there, which is the read these
/// queries exist to replace. So every location a query reports carries the
/// source at that location, quoted from `content_root` — the working copy the
/// query was answered against, which is not the base checkout when a request
/// overlays a sibling worktree.
pub struct Source<'a> {
    /// Root that reported paths are made relative to.
    root: &'a Path,
    /// Checkout the quoted text is read from.
    content_root: PathBuf,
    /// Extra lines to include on either side of a location.
    context_lines: usize,
    /// Files already read, so one response over N hits in a file reads it once.
    /// `None` marks a file that could not be read, so it isn't retried. Shared
    /// as an `Arc<str>` so every hit in one file borrows the same content
    /// instead of copying it.
    cache: RefCell<HashMap<PathBuf, Option<Arc<str>>>>,
}

impl<'a> Source<'a> {
    /// Quote from `content_root`, reporting paths relative to `root`, with
    /// `context_lines` extra lines around each location (clamped to
    /// [`MAX_CONTEXT_LINES`]).
    pub fn new(root: &'a Path, content_root: PathBuf, context_lines: usize) -> Self {
        Self {
            root,
            content_root,
            context_lines: context_lines.min(MAX_CONTEXT_LINES),
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// A `file://` URI as the path a caller sees: relative to `root` where it
    /// lies inside it, absolute otherwise.
    fn rel(&self, uri: &str) -> String {
        let path = uri_to_path(uri);
        path.strip_prefix(self.root)
            .unwrap_or(&path)
            .display()
            .to_string()
    }

    /// Read `uri`'s content from the checkout being quoted, memoized.
    fn content(&self, uri: &str) -> Option<Arc<str>> {
        let abs = uri_to_path(uri);
        // The URI addresses the base index; an overlaid working copy holds the
        // same relative path under its own root.
        let read_from = match abs.strip_prefix(self.root) {
            Ok(rel) => self.content_root.join(rel),
            Err(_) => abs.clone(),
        };
        if let Some(cached) = self.cache.borrow().get(&read_from) {
            return cached.clone();
        }
        let content: Option<Arc<str>> = std::fs::read_to_string(&read_from)
            .ok()
            .map(Arc::from);
        self.cache.borrow_mut().insert(read_from, content.clone());
        content
    }

    /// Attach the source at `range` in `uri` to a result entry: `text`, the
    /// line(s) the range covers, plus `context`/`context_start_line` when a
    /// window was asked for.
    ///
    /// A file that can't be read, or a range past its end, leaves the entry
    /// without text rather than failing the query or — worse — quoting the
    /// wrong line: the location is the answer, the text is context for it.
    fn annotate(&self, entry: &mut Value, uri: &str, range: &LspRange) {
        let Some(content) = self.content(uri) else {
            return;
        };
        let lines: Vec<&str> = content.lines().collect();
        let first = range.start.line as usize;
        // An LSP range end is exclusive: a multiline range that ends at
        // character 0 stops before that line, so quoting it would add a line
        // the range doesn't cover.
        let end = range.end.line as usize;
        let last = match end > first && range.end.character == 0 {
            true => end - 1,
            false => end.max(first),
        };
        if first >= lines.len() {
            return;
        }
        let last = last.min(lines.len() - 1);

        entry["text"] = json!(truncate_lines(&lines[first..=last]));

        if self.context_lines > 0 {
            let from = first.saturating_sub(self.context_lines);
            let to = (last + self.context_lines).min(lines.len() - 1);
            entry["context"] = json!(truncate_lines(&lines[from..=to]));
            entry["context_start_line"] = json!(from);
        }
    }
}

/// Join `lines`, capping each at [`MAX_TEXT_CHARS`] with a trailing ellipsis.
fn truncate_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| {
            if line.chars().count() <= MAX_TEXT_CHARS {
                (*line).to_string()
            } else {
                let head: String = line.chars().take(MAX_TEXT_CHARS).collect();
                format!("{head}…")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `context_lines` parameter every location-bearing query accepts, as a
/// JSON Schema property. Shared so each operation describes it identically.
pub fn context_lines_param() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": MAX_CONTEXT_LINES,
        "default": 0,
        "description": "Extra source lines to include on either side of each result location. \
                        Each result already carries the line it points at; raise this only when \
                        the surrounding lines matter."
    })
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

/// Convert an LSP `Location[]` response into a structured find-usages result:
/// paths relative to the project root, ranges flattened, and each usage quoting
/// the source line it points at.
pub fn locations_to_query(value: Value, source: &Source<'_>) -> Result<Value> {
    if value.is_null() {
        return Ok(json!({ "count": 0, "usages": [] }));
    }
    let locations: Vec<LspLocation> = serde_json::from_value(value)?;

    let usages: Vec<Value> = locations
        .into_iter()
        .map(|loc| {
            let mut entry = json!({
                "file": source.rel(&loc.uri),
                "start_line": loc.range.start.line,
                "start_character": loc.range.start.character,
                "end_line": loc.range.end.line,
                "end_character": loc.range.end.character,
            });
            source.annotate(&mut entry, &loc.uri, &loc.range);
            entry
        })
        .collect();

    Ok(json!({ "count": usages.len(), "usages": usages }))
}

/// Convert an LSP goto response — `textDocument/definition` and its siblings —
/// into a structured list published under `key`, each entry quoting the source
/// it points at.
///
/// The answer is always a list, even where the common case has one element: a
/// definition can legitimately be several places (an overload set, an ambient
/// declaration beside its implementation), and a caller that must handle
/// "sometimes an object, sometimes an array" is one that will handle it wrong.
pub fn goto_to_query(value: Value, source: &Source<'_>, key: &str) -> Result<Value> {
    if value.is_null() {
        return Ok(json!({ "count": 0, key: [] }));
    }
    let targets = match serde_json::from_value::<LspGotoResponse>(value)? {
        LspGotoResponse::Many(targets) => targets,
        LspGotoResponse::One(target) => vec![target],
    };

    let items: Vec<Value> = targets
        .into_iter()
        .filter_map(|t| {
            let uri = t.uri.or(t.target_uri)?;
            // For a `LocationLink`, the selection range is the target's own
            // identifier — the coordinate a position-targeted operation wants,
            // and one line of source rather than a whole declaration.
            let range = t.range.or(t.target_selection_range).or(t.target_range)?;
            let mut entry = json!({
                "file": source.rel(&uri),
                "start_line": range.start.line,
                "start_character": range.start.character,
                "end_line": range.end.line,
                "end_character": range.end.character,
            });
            source.annotate(&mut entry, &uri, &range);
            Some(entry)
        })
        .collect();

    Ok(json!({ "count": items.len(), key: items }))
}

/// Convert a `textDocument/hover` response into a structured description of the
/// symbol: its resolved type or signature and whatever documentation the server
/// has, as one markdown string, plus the range it describes.
///
/// The result carries no quoted source line, unlike the location-bearing
/// queries: it is already source-derived prose, and a line of code beside it
/// would be redundant.
pub fn hover_to_query(value: Value) -> Result<Value> {
    if value.is_null() {
        return Ok(json!({ "text": "" }));
    }
    let hover: LspHover = serde_json::from_value(value)?;
    let mut out = json!({ "text": hover_text(&hover.contents) });
    if let Some(range) = hover.range {
        out["start_line"] = json!(range.start.line);
        out["start_character"] = json!(range.start.character);
        out["end_line"] = json!(range.end.line);
        out["end_character"] = json!(range.end.character);
    }
    Ok(out)
}

/// Render hover contents as markdown, fencing the language-tagged form so a
/// signature still reads as code.
fn hover_text(contents: &LspHoverContents) -> String {
    match contents {
        LspHoverContents::Markup { value, .. } | LspHoverContents::Plain(value) => value.clone(),
        LspHoverContents::Fenced { language, value } => {
            format!("```{language}\n{value}\n```")
        }
        LspHoverContents::Many(parts) => parts
            .iter()
            .map(hover_text)
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// Convert a `textDocument/documentSymbol` response into a file outline: the
/// symbols declared in `uri`, nested as they are in the source, each quoting
/// its declaration line.
///
/// `count` is the number of top-level symbols; nested ones are counted inside
/// their parent's `children`.
pub fn document_symbols_to_query(value: Value, source: &Source<'_>, uri: &str) -> Result<Value> {
    if value.is_null() {
        return Ok(json!({ "count": 0, "symbols": [] }));
    }
    let items: Vec<LspDocumentSymbol> = serde_json::from_value(value)?;
    let symbols: Vec<Value> = items
        .into_iter()
        .filter_map(|item| outline_entry(item, source, uri))
        .collect();
    Ok(json!({ "count": symbols.len(), "symbols": symbols }))
}

/// One outline entry, with its children beneath it.
fn outline_entry(item: LspDocumentSymbol, source: &Source<'_>, uri: &str) -> Option<Value> {
    // The flat form carries its range under `location`; the hierarchical form
    // has a declaration `range` and, inside it, the name's `selectionRange`.
    let full = item.range.or_else(|| item.location.map(|l| l.range))?;
    // Quote and address the symbol by its name, not its body: a class's
    // declaration range spans the whole file, and quoting that would return
    // the file — the read this operation exists to avoid.
    let name_range = item.selection_range.unwrap_or(full);

    let mut entry = json!({
        "name": item.name,
        "kind": symbol_kind_name(item.kind),
        "start_line": name_range.start.line,
        "start_character": name_range.start.character,
        "end_line": name_range.end.line,
        "end_character": name_range.end.character,
        // The extent of the declaration, so a caller can read just this member
        // instead of the whole file.
        "body_start_line": full.start.line,
        "body_end_line": full.end.line,
    });
    if let Some(detail) = item.detail {
        entry["detail"] = json!(detail);
    }
    if let Some(container) = item.container_name {
        entry["container_name"] = json!(container);
    }
    source.annotate(&mut entry, uri, &name_range);

    // A flat response has no nesting to report; it is left childless rather
    // than reassembled into a tree by guessing at names.
    let children: Vec<Value> = item
        .children
        .unwrap_or_default()
        .into_iter()
        .filter_map(|child| outline_entry(child, source, uri))
        .collect();
    entry["children"] = json!(children);
    Some(entry)
}

/// Convert a `textDocument/prepareCallHierarchy` response into the items a
/// position resolves to, each quoting its declaration line.
pub fn call_hierarchy_items_to_query(value: Value, source: &Source<'_>) -> Result<Value> {
    if value.is_null() {
        return Ok(json!({ "count": 0, "items": [] }));
    }
    let raw: Vec<Value> = serde_json::from_value(value)?;
    let items: Vec<Value> = raw.iter().filter_map(|r| call_item(r, source)).collect();
    Ok(json!({ "count": items.len(), "items": items }))
}

/// Normalize one call hierarchy item, keeping the server's own handle on it.
///
/// A normalized item is not a valid handle: servers attach a private `data`
/// field and require the item back unchanged on `callHierarchy/incomingCalls`
/// and `outgoingCalls` (jdtls does exactly this). So the entry carries both —
/// readable fields to decide *which* item is wanted, and `item`, the raw
/// `CallHierarchyItem` to name it again with.
fn call_item(raw: &Value, source: &Source<'_>) -> Option<Value> {
    let parsed: LspCallHierarchyItem = serde_json::from_value(raw.clone()).ok()?;
    // Quote and address the declaration by its name, not its body.
    let name_range = parsed.selection_range;
    let mut entry = json!({
        "name": parsed.name,
        "kind": symbol_kind_name(parsed.kind),
        "file": source.rel(&parsed.uri),
        "start_line": name_range.start.line,
        "start_character": name_range.start.character,
        "end_line": name_range.end.line,
        "end_character": name_range.end.character,
        "body_start_line": parsed.range.start.line,
        "body_end_line": parsed.range.end.line,
        "item": raw.clone(),
    });
    if let Some(detail) = parsed.detail {
        entry["detail"] = json!(detail);
    }
    source.annotate(&mut entry, &parsed.uri, &name_range);
    Some(entry)
}

/// Convert a `callHierarchy/incomingCalls` response into the callers of the
/// queried item: each caller as a normalized item (its opaque handle included,
/// so the walk can continue), paired with the call sites inside it.
pub fn incoming_calls_to_query(value: Value, source: &Source<'_>) -> Result<Value> {
    calls_to_query(value, source, "from", None)
}

/// Convert a `callHierarchy/outgoingCalls` response into what the queried item
/// calls: each callee as a normalized item, paired with the call sites — which
/// live in the *queried* item's file, `called_from`, not in the callee's.
pub fn outgoing_calls_to_query(
    value: Value,
    source: &Source<'_>,
    called_from: &str,
) -> Result<Value> {
    calls_to_query(value, source, "to", Some(called_from))
}

/// Shared shape behind the two call-hierarchy directions. `direction` is the
/// field naming the other end of the call — kept as `from`/`to` rather than
/// normalized to one neutral name, because a caller and a callee are not
/// interchangeable and a saved result should say which it holds.
///
/// `sites_in` is the file the call sites live in: for incoming calls that is
/// each caller's own file, so it is read per entry; for outgoing calls every
/// call site is in the *queried* item's file, which is passed in.
fn calls_to_query(
    value: Value,
    source: &Source<'_>,
    direction: &str,
    sites_in: Option<&str>,
) -> Result<Value> {
    if value.is_null() {
        return Ok(json!({ "count": 0, "calls": [] }));
    }
    let raw: Vec<Value> = serde_json::from_value(value)?;

    let calls: Vec<Value> = raw
        .iter()
        .filter_map(|entry| {
            let other = call_item(entry.get(direction)?, source)?;
            let sites_uri = sites_in
                .map(str::to_string)
                .or_else(|| {
                    entry
                        .get(direction)?
                        .get("uri")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let ranges: Vec<Value> = entry
                .get("fromRanges")
                .and_then(Value::as_array)
                .map(|ranges| call_sites(ranges, source, &sites_uri))
                .unwrap_or_default();
            Some(json!({ direction: other, "ranges": ranges }))
        })
        .collect();

    Ok(json!({ "count": calls.len(), "calls": calls }))
}

/// The call sites themselves, each quoting the line the call is written on —
/// which is how a caller tells `foo(a, b)` from `foo(a, b, c)` without opening
/// the file.
fn call_sites(ranges: &[Value], source: &Source<'_>, uri: &str) -> Vec<Value> {
    ranges
        .iter()
        .filter_map(|r| {
            let range: LspRange = serde_json::from_value(r.clone()).ok()?;
            let mut entry = json!({
                "file": source.rel(uri),
                "start_line": range.start.line,
                "start_character": range.start.character,
                "end_line": range.end.line,
                "end_character": range.end.character,
            });
            source.annotate(&mut entry, uri, &range);
            Some(entry)
        })
        .collect()
}

/// The default cap on the number of symbols `symbols_to_query` returns when
/// the caller doesn't specify a `limit`.
pub const DEFAULT_SYMBOL_SEARCH_LIMIT: usize = 200;

/// Convert an LSP `SymbolInformation[]` (or `WorkspaceSymbol[]`) response
/// from `workspace/symbol` into a structured symbol-search result, with
/// paths relative to the project root, each match quoting its declaring line,
/// and matches capped at `limit`.
///
/// `count` always reports the total number matched, even when the returned
/// `symbols` list is truncated to `limit` — a truncated response also carries
/// `"truncated": true` so the caller knows to narrow its query rather than
/// assuming it saw everything.
pub fn symbols_to_query(value: Value, source: &Source<'_>, limit: usize) -> Result<Value> {
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
    // returned, and quoting one costs a file read and a scan of its lines.
    let total = matched.len();
    let truncated = total > limit;

    let symbols: Vec<Value> = matched
        .into_iter()
        .take(limit)
        .map(|(s, range)| {
            let mut obj = json!({
                "name": s.name,
                "kind": symbol_kind_name(s.kind),
                "file": source.rel(&s.location.uri),
                "start_line": range.start.line,
                "start_character": range.start.character,
                "end_line": range.end.line,
                "end_character": range.end.character,
            });
            if let Some(container) = s.container_name {
                obj["container_name"] = json!(container);
            }
            // The declaring line is what lets a caller pick between several
            // same-named matches without opening each file.
            source.annotate(&mut obj, &s.location.uri, &range);
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

/// Where a call-hierarchy request belongs, read from the `item` parameter's
/// URI. An item belongs to one file, and only the server that indexed that file
/// can expand it — a project-scoped request carries no target to route by, so
/// the item itself names the language.
///
/// The URI is not always a plain path: a server hands back its own handles for
/// sources it holds outside the working copy (jdtls issues
/// `jdt://contents/java.base/java.io/PrintStream.java?…` for a class it has as
/// bytecode), keeping the source's name in the path and its own metadata in the
/// query string. Reading the path alone leaves those with the server that
/// issued them; an item this cannot place is reported as such, rather than
/// offered to servers that never saw it.
pub fn call_hierarchy_item_route(params: &Value) -> LanguageRoute {
    let Some(uri) = params.pointer("/item/uri").and_then(Value::as_str) else {
        return LanguageRoute::Unspecified;
    };
    match Language::from_path(&uri_to_path(uri_path(uri))) {
        Some(language) => LanguageRoute::Language(language),
        None => LanguageRoute::Unplaceable(uri.to_string()),
    }
}

/// A URI's path, without the query string or fragment a server may hang off its
/// own handles.
fn uri_path(uri: &str) -> &str {
    let end = uri.find(['?', '#']).unwrap_or(uri.len());
    &uri[..end]
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

    /// A source rooted at a path that holds no files, so results carry
    /// locations but no quoted text.
    fn at_root(root: &str) -> Source<'_> {
        Source::new(Path::new(root), PathBuf::from(root), 0)
    }

    /// Write `content` to `name` under a fresh root, returning both.
    fn tree(name: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(name);
        std::fs::write(&file, content).unwrap();
        (dir, file)
    }

    /// The `file://` URI for a test path.
    fn path_to_uri(path: &Path) -> String {
        format!("file://{}", path.display())
    }


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
    fn outgoing_calls_quote_call_sites_in_the_queried_file() {
        // The call sites are in the file we asked about, not in the callee's.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Caller.java"),
            "class Caller {\n  void go() {\n    target(1);\n  }\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Target.java"),
            "class Target {\n  void target(int n) {}\n}\n",
        )
        .unwrap();
        let caller_uri = path_to_uri(&dir.path().join("Caller.java"));
        let target_uri = path_to_uri(&dir.path().join("Target.java"));

        let value = json!([{
            "to": {
                "name": "target",
                "kind": 6,
                "uri": target_uri,
                "range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 24}},
                "selectionRange": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 13}}
            },
            "fromRanges": [
                {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 10}}
            ]
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = outgoing_calls_to_query(value, &source, &caller_uri).unwrap();

        let call = &out["calls"][0];
        // The callee is quoted from its own declaration...
        assert_eq!(call["to"]["file"], json!("Target.java"));
        assert_eq!(call["to"]["text"], json!("  void target(int n) {}"));
        // ...while the call site is quoted from the file we asked about.
        assert_eq!(call["ranges"][0]["file"], json!("Caller.java"));
        assert_eq!(call["ranges"][0]["text"], json!("    target(1);"));
    }

    #[test]
    fn a_method_that_calls_nothing_is_an_empty_result() {
        let out = outgoing_calls_to_query(Value::Null, &at_root("/proj"), "file:///proj/a.rs").unwrap();
        assert_eq!(out, json!({ "count": 0, "calls": [] }));
    }

    #[test]
    fn incoming_calls_quote_the_caller_and_its_call_sites() {
        let (dir, file) = tree(
            "Caller.java",
            "class Caller {\n  void go() {\n    target(1);\n  }\n}\n",
        );
        let uri = path_to_uri(&file);
        let value = json!([{
            "from": {
                "name": "go",
                "kind": 6,
                "uri": uri,
                "range": {"start": {"line": 1, "character": 2}, "end": {"line": 3, "character": 3}},
                "selectionRange": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 9}},
                "data": { "opaque": "keep-me" }
            },
            "fromRanges": [
                {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 10}}
            ]
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = incoming_calls_to_query(value, &source).unwrap();

        assert_eq!(out["count"], json!(1));
        let call = &out["calls"][0];
        // The caller: its declaration line, and a handle to keep walking.
        assert_eq!(call["from"]["name"], json!("go"));
        assert_eq!(call["from"]["text"], json!("  void go() {"));
        assert_eq!(call["from"]["item"]["data"]["opaque"], json!("keep-me"));
        // The call site: the line the call is written on.
        assert_eq!(call["ranges"][0]["file"], json!("Caller.java"));
        assert_eq!(call["ranges"][0]["start_line"], json!(2));
        assert_eq!(call["ranges"][0]["text"], json!("    target(1);"));
    }

    #[test]
    fn a_method_with_no_callers_is_an_empty_result() {
        let out = incoming_calls_to_query(Value::Null, &at_root("/proj")).unwrap();
        assert_eq!(out, json!({ "count": 0, "calls": [] }));
        let out = incoming_calls_to_query(json!([]), &at_root("/proj")).unwrap();
        assert_eq!(out["count"], json!(0));
    }

    #[test]
    fn call_hierarchy_item_keeps_the_servers_own_handle() {
        let (dir, file) = tree("Foo.java", "class Foo {\n  void run() {}\n}\n");
        let uri = path_to_uri(&file);
        let value = json!([{
            "name": "run",
            "kind": 6,
            "detail": "Foo.run()",
            "uri": uri,
            "range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 16}},
            "selectionRange": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 10}},
            "data": { "opaque": "jdtls-private" }
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = call_hierarchy_items_to_query(value, &source).unwrap();

        assert_eq!(out["count"], json!(1));
        let item = &out["items"][0];
        assert_eq!(item["name"], json!("run"));
        assert_eq!(item["kind"], json!("method"));
        assert_eq!(item["file"], json!("Foo.java"));
        assert_eq!(item["start_line"], json!(1));
        assert_eq!(item["start_character"], json!(7));
        assert_eq!(item["body_end_line"], json!(1));
        assert_eq!(item["detail"], json!("Foo.run()"));
        assert_eq!(item["text"], json!("  void run() {}"));
        // The server's private data survives untouched, so the item can be
        // handed back on the follow-up call.
        assert_eq!(item["item"]["data"]["opaque"], json!("jdtls-private"));
    }

    #[test]
    fn a_call_hierarchy_item_routes_to_the_server_that_issued_it() {
        let route = |uri: &str| call_hierarchy_item_route(&json!({ "item": { "uri": uri } }));

        assert_eq!(
            route("file:///proj/src/Foo.java"),
            LanguageRoute::Language(Language::Java)
        );
        // jdtls hands back its own handle for a class it only has as bytecode:
        // the source name is in the path, its metadata in the query string.
        let jdt = "jdt://contents/java.base/java.io/PrintStream.java\
                   ?=proj/%5C/jre%5C/java.base=/maven.pomderived=/true=/=/false=/=/PrintStream.class";
        assert_eq!(route(jdt), LanguageRoute::Language(Language::Java));
        // An item Henka cannot place names itself in the route, so dispatch can
        // say which handle it refused rather than trying every server.
        assert_eq!(
            route("nowhere://opaque/handle"),
            LanguageRoute::Unplaceable("nowhere://opaque/handle".into())
        );
        // No item at all: nothing to route by.
        assert_eq!(
            call_hierarchy_item_route(&json!({})),
            LanguageRoute::Unspecified
        );
    }

    #[test]
    fn call_hierarchy_on_a_position_that_resolves_to_nothing_is_empty() {
        let out = call_hierarchy_items_to_query(Value::Null, &at_root("/proj")).unwrap();
        assert_eq!(out, json!({ "count": 0, "items": [] }));
    }

    #[test]
    fn outline_nests_children_and_quotes_declarations() {
        let (dir, file) = tree(
            "Foo.java",
            "package p;\n\npublic class Foo {\n  int size() { return 1; }\n}\n",
        );
        let uri = path_to_uri(&file);
        let value = json!([{
            "name": "Foo",
            "kind": 5,
            "range": {"start": {"line": 2, "character": 0}, "end": {"line": 4, "character": 1}},
            "selectionRange": {"start": {"line": 2, "character": 13}, "end": {"line": 2, "character": 16}},
            "children": [{
                "name": "size",
                "kind": 6,
                "detail": "() : int",
                "range": {"start": {"line": 3, "character": 2}, "end": {"line": 3, "character": 26}},
                "selectionRange": {"start": {"line": 3, "character": 6}, "end": {"line": 3, "character": 10}}
            }]
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = document_symbols_to_query(value, &source, &uri).unwrap();

        assert_eq!(out["count"], json!(1));
        let class = &out["symbols"][0];
        assert_eq!(class["kind"], json!("class"));
        assert_eq!(class["text"], json!("public class Foo {"));
        // Addressed by its name, with the declaration's extent alongside.
        assert_eq!(class["start_line"], json!(2));
        assert_eq!(class["start_character"], json!(13));
        assert_eq!(class["body_end_line"], json!(4));

        let method = &class["children"][0];
        assert_eq!(method["name"], json!("size"));
        assert_eq!(method["detail"], json!("() : int"));
        assert_eq!(method["text"], json!("  int size() { return 1; }"));
        assert_eq!(method["children"], json!([]));
    }

    #[test]
    fn outline_accepts_the_flat_symbol_information_form() {
        let value = json!([{
            "name": "helper",
            "kind": 12,
            "location": {
                "uri": "file:///proj/a.ts",
                "range": {"start": {"line": 7, "character": 9}, "end": {"line": 7, "character": 15}}
            },
            "containerName": "mod"
        }]);
        let out = document_symbols_to_query(value, &at_root("/proj"), "file:///proj/a.ts").unwrap();
        let sym = &out["symbols"][0];
        assert_eq!(sym["kind"], json!("function"));
        assert_eq!(sym["container_name"], json!("mod"));
        assert_eq!(sym["start_line"], json!(7));
        // No nesting is reported rather than inferred from names.
        assert_eq!(sym["children"], json!([]));
    }

    #[test]
    fn outline_of_a_file_without_symbols_is_empty() {
        let out = document_symbols_to_query(Value::Null, &at_root("/proj"), "file:///proj/a.ts").unwrap();
        assert_eq!(out, json!({ "count": 0, "symbols": [] }));
    }

    #[test]
    fn hover_reads_markup_content() {
        let value = json!({
            "contents": { "kind": "markdown", "value": "`fn foo() -> u32`\n\nDoes a thing." },
            "range": {"start": {"line": 4, "character": 3}, "end": {"line": 4, "character": 6}}
        });
        let out = hover_to_query(value).unwrap();
        assert_eq!(out["text"], json!("`fn foo() -> u32`\n\nDoes a thing."));
        assert_eq!(out["start_line"], json!(4));
        assert_eq!(out["end_character"], json!(6));
    }

    #[test]
    fn hover_fences_a_language_tagged_string() {
        let value = json!({ "contents": { "language": "java", "value": "String name" } });
        let out = hover_to_query(value).unwrap();
        assert_eq!(out["text"], json!("```java\nString name\n```"));
    }

    #[test]
    fn hover_joins_a_list_of_parts_and_drops_empties() {
        let value = json!({ "contents": [
            { "language": "ts", "value": "const x: number" },
            "",
            "The count of things."
        ]});
        let out = hover_to_query(value).unwrap();
        assert_eq!(
            out["text"],
            json!("```ts\nconst x: number\n```\n\nThe count of things.")
        );
    }

    #[test]
    fn hover_accepts_a_bare_string() {
        let out = hover_to_query(json!({ "contents": "plain words" })).unwrap();
        assert_eq!(out["text"], json!("plain words"));
    }

    #[test]
    fn hover_with_nothing_to_say_is_empty_not_an_error() {
        let out = hover_to_query(Value::Null).unwrap();
        assert_eq!(out, json!({ "text": "" }));
        // No range means no coordinates, not a malformed result.
        let out = hover_to_query(json!({ "contents": "x" })).unwrap();
        assert!(out.get("start_line").is_none());
    }

    #[test]
    fn goto_accepts_a_single_location() {
        let (dir, file) = tree("Foo.java", "package p;\nclass Foo {}\n");
        let value = json!({
            "uri": path_to_uri(&file),
            "range": {"start": {"line": 1, "character": 6}, "end": {"line": 1, "character": 9}}
        });
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = goto_to_query(value, &source, "definitions").unwrap();
        assert_eq!(out["count"], json!(1));
        assert_eq!(out["definitions"][0]["file"], json!("Foo.java"));
        assert_eq!(out["definitions"][0]["text"], json!("class Foo {}"));
    }

    #[test]
    fn goto_accepts_a_location_array() {
        let value = json!([
            {
                "uri": "file:///proj/A.ts",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}
            },
            {
                "uri": "file:///proj/B.ts",
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}
            }
        ]);
        let out = goto_to_query(value, &at_root("/proj"), "definitions").unwrap();
        assert_eq!(out["count"], json!(2));
        assert_eq!(out["definitions"][1]["file"], json!("B.ts"));
    }

    #[test]
    fn goto_link_prefers_the_selection_range() {
        let value = json!([{
            "targetUri": "file:///proj/src/lib.rs",
            "targetRange": {"start": {"line": 10, "character": 0}, "end": {"line": 20, "character": 1}},
            "targetSelectionRange": {"start": {"line": 10, "character": 7}, "end": {"line": 10, "character": 10}}
        }]);
        let out = goto_to_query(value, &at_root("/proj"), "definitions").unwrap();
        assert_eq!(out["definitions"][0]["start_character"], json!(7));
        assert_eq!(out["definitions"][0]["end_line"], json!(10));
    }

    #[test]
    fn goto_link_without_a_selection_range_uses_the_target_range() {
        let value = json!([{
            "targetUri": "file:///proj/src/lib.rs",
            "targetRange": {"start": {"line": 10, "character": 0}, "end": {"line": 20, "character": 1}}
        }]);
        let out = goto_to_query(value, &at_root("/proj"), "definitions").unwrap();
        assert_eq!(out["definitions"][0]["end_line"], json!(20));
    }

    #[test]
    fn goto_nothing_found_is_an_empty_result() {
        let out = goto_to_query(Value::Null, &at_root("/proj"), "definitions").unwrap();
        assert_eq!(out, json!({ "count": 0, "definitions": [] }));
        let out = goto_to_query(json!([]), &at_root("/proj"), "definitions").unwrap();
        assert_eq!(out, json!({ "count": 0, "definitions": [] }));
    }

    #[test]
    fn usage_quotes_the_line_it_points_at() {
        let (dir, file) = tree(
            "auth.ts",
            "import x;\nfunction f() {\n  return validateToken(token);\n}\n",
        );
        let value = json!([{
            "uri": path_to_uri(&file),
            "range": {"start": {"line": 2, "character": 9}, "end": {"line": 2, "character": 22}}
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = locations_to_query(value, &source).unwrap();
        assert_eq!(out["usages"][0]["file"], json!("auth.ts"));
        assert_eq!(out["usages"][0]["text"], json!("  return validateToken(token);"));
        // No window was asked for, so none is paid for.
        assert!(out["usages"][0].get("context").is_none());
    }

    #[test]
    fn multiline_range_ending_at_character_zero_stops_at_the_line_before() {
        let (dir, file) = tree("a.rs", "one\ntwo\nthree\nfour\n");
        // An end of {line: 2, character: 0} is exclusive: the range covers
        // lines 1 and 2 of the file, not line 3.
        let value = json!([{
            "uri": path_to_uri(&file),
            "range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 1);
        let out = locations_to_query(value, &source).unwrap();
        assert_eq!(out["usages"][0]["text"], json!("two"));
        assert_eq!(out["usages"][0]["context"], json!("one\ntwo\nthree"));
        assert_eq!(out["usages"][0]["context_start_line"], json!(0));
    }

    #[test]
    fn multiline_range_ending_mid_line_includes_that_line() {
        let (dir, file) = tree("a.rs", "one\ntwo\nthree\nfour\n");
        let value = json!([{
            "uri": path_to_uri(&file),
            "range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 2}}
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = locations_to_query(value, &source).unwrap();
        assert_eq!(out["usages"][0]["text"], json!("two\nthree"));
    }

    #[test]
    fn context_lines_widen_the_window_and_report_its_start() {
        let (dir, file) = tree("a.rs", "one\ntwo\nthree\nfour\nfive\n");
        let value = json!([{
            "uri": path_to_uri(&file),
            "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 5}}
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 1);
        let out = locations_to_query(value, &source).unwrap();
        assert_eq!(out["usages"][0]["text"], json!("three"));
        assert_eq!(out["usages"][0]["context"], json!("two\nthree\nfour"));
        assert_eq!(out["usages"][0]["context_start_line"], json!(1));
    }

    #[test]
    fn context_window_is_clamped_at_the_file_edges() {
        let (dir, file) = tree("a.rs", "one\ntwo\n");
        let value = json!([{
            "uri": path_to_uri(&file),
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 5);
        let out = locations_to_query(value, &source).unwrap();
        assert_eq!(out["usages"][0]["context"], json!("one\ntwo"));
        assert_eq!(out["usages"][0]["context_start_line"], json!(0));
    }

    #[test]
    fn text_is_read_from_the_overlaid_working_copy() {
        // The URI addresses the base checkout, but the request was answered
        // against a sibling working copy whose content differs.
        let base = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("a.rs"), "let base = 1;\n").unwrap();
        std::fs::write(work.path().join("a.rs"), "let overlaid = 1;\n").unwrap();
        let value = json!([{
            "uri": path_to_uri(&base.path().join("a.rs")),
            "range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 12}}
        }]);
        let source = Source::new(base.path(), work.path().to_path_buf(), 0);
        let out = locations_to_query(value, &source).unwrap();
        assert_eq!(out["usages"][0]["text"], json!("let overlaid = 1;"));
    }

    #[test]
    fn unreadable_file_loses_its_text_not_its_place() {
        let value = json!([{
            "uri": "file:///proj/gone.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}
        }]);
        let out = locations_to_query(value, &at_root("/proj")).unwrap();
        assert_eq!(out["count"], json!(1));
        assert_eq!(out["usages"][0]["file"], json!("gone.rs"));
        assert!(out["usages"][0].get("text").is_none());
    }

    #[test]
    fn range_past_the_end_of_the_file_quotes_nothing() {
        let (dir, file) = tree("a.rs", "one\n");
        let value = json!([{
            "uri": path_to_uri(&file),
            "range": {"start": {"line": 40, "character": 0}, "end": {"line": 40, "character": 3}}
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = locations_to_query(value, &source).unwrap();
        assert!(out["usages"][0].get("text").is_none());
    }

    #[test]
    fn over_long_line_is_truncated() {
        let long = "x".repeat(MAX_TEXT_CHARS + 50);
        let (dir, file) = tree("min.js", &format!("{long}\n"));
        let value = json!([{
            "uri": path_to_uri(&file),
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = locations_to_query(value, &source).unwrap();
        let text = out["usages"][0]["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn context_lines_are_clamped_to_the_cap() {
        let source = Source::new(Path::new("/proj"), PathBuf::from("/proj"), 999);
        assert_eq!(source.context_lines, MAX_CONTEXT_LINES);
    }

    #[test]
    fn symbol_quotes_its_declaring_line() {
        let (dir, file) = tree("Foo.java", "package p;\n\npublic class Foo {\n}\n");
        let value = json!([{
            "name": "Foo",
            "kind": 5,
            "location": {
                "uri": path_to_uri(&file),
                "range": {"start": {"line": 2, "character": 13}, "end": {"line": 2, "character": 16}}
            }
        }]);
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);
        let out = symbols_to_query(value, &source, 10).unwrap();
        assert_eq!(out["symbols"][0]["text"], json!("public class Foo {"));
    }

    #[test]
    fn null_usages_report_a_count() {
        let out = locations_to_query(Value::Null, &at_root("/proj")).unwrap();
        assert_eq!(out, json!({ "count": 0, "usages": [] }));
    }

    #[test]
    fn null_symbol_search_is_empty() {
        let out = symbols_to_query(Value::Null, &at_root("/proj"), 10).unwrap();
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
        let out = symbols_to_query(value, &at_root("/proj"), 10).unwrap();
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
        let out = symbols_to_query(value, &at_root("/proj"), 10).unwrap();
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
        let out = symbols_to_query(value, &at_root("/proj"), 10).unwrap();
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
        let out = symbols_to_query(value, &at_root("/proj"), 10).unwrap();
        assert_eq!(out["count"], json!(1));
        assert_eq!(out["symbols"][0]["name"], json!("Resolved"));
    }

    #[test]
    fn symbol_search_quotes_only_the_matches_it_returns() {
        // Quoting a match reads its file and scans it; a broad search that
        // returns two of two thousand matches must pay that for two, so the
        // source cache must hold only the files behind the returned symbols.
        let dir = tempfile::tempdir().unwrap();
        let matches: Vec<Value> = (0..5)
            .map(|i| {
                let file = dir.path().join(format!("Foo{i}.java"));
                std::fs::write(&file, format!("class Foo{i} {{}}\n")).unwrap();
                json!({
                    "name": format!("Foo{i}"),
                    "kind": 5,
                    "location": {
                        "uri": path_to_uri(&file),
                        "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 10}}
                    }
                })
            })
            .collect();
        let source = Source::new(dir.path(), dir.path().to_path_buf(), 0);

        let out = symbols_to_query(json!(matches), &source, 2).unwrap();

        assert_eq!(out["count"], json!(5));
        assert_eq!(out["symbols"][0]["text"], json!("class Foo0 {}"));
        assert_eq!(out["symbols"][1]["text"], json!("class Foo1 {}"));
        assert_eq!(source.cache.borrow().len(), 2, "only the returned matches are read");
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
        let out = symbols_to_query(json!(matches), &at_root("/proj"), 2).unwrap();
        assert_eq!(out["count"], json!(5));
        assert_eq!(out["symbols"].as_array().unwrap().len(), 2);
        assert_eq!(out["truncated"], json!(true));
    }
}
