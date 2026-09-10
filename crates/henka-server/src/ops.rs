//! Turning operation descriptors into MCP tools, and MCP tool arguments back
//! into operation requests.
//!
//! Each operation in the catalog becomes one MCP tool whose input schema is the
//! common envelope (project + target + `dry_run` for edits) merged with the
//! operation's own parameter schema. Dispatch reverses that: it pulls the
//! envelope fields out of the call arguments and leaves the rest as the
//! operation's parameters.

use std::path::PathBuf;
use std::sync::Arc;

use henka_core::operation::{OperationDescriptor, OperationKind, TargetKind};
use henka_core::{Position, Range, Target};
use rmcp::ErrorData as McpError;
use rmcp::model::Tool;
use serde_json::{Map, Value, json};

/// A JSON object, matching rmcp's tool-schema representation.
pub type JsonObject = Map<String, Value>;

/// Envelope field names that are not part of an operation's own parameters.
const ENVELOPE_KEYS: &[&str] = &[
    "project",
    "workspace",
    "file",
    "line",
    "character",
    "start_line",
    "start_character",
    "end_line",
    "end_character",
    "expect",
    "dry_run",
];

/// Build the MCP tool for an operation.
pub fn operation_tool(descriptor: &OperationDescriptor) -> Tool {
    let description = match descriptor.kind {
        OperationKind::Edit => format!(
            "{} (edit; defaults to a preview — pass dry_run=false to apply).",
            descriptor.description
        ),
        OperationKind::Query => format!("{} (read-only query).", descriptor.description),
    };
    Tool::new(
        descriptor.id.clone(),
        description,
        build_input_schema(descriptor),
    )
}

/// Construct the merged input schema for an operation tool.
fn build_input_schema(descriptor: &OperationDescriptor) -> Arc<JsonObject> {
    let mut props = Map::new();
    let mut required: Vec<Value> = Vec::new();

    props.insert(
        "project".into(),
        json!({ "type": "string", "description": "Id of the registered project to act on." }),
    );
    required.push("project".into());

    props.insert(
        "workspace".into(),
        json!({
            "type": "string",
            "description": "Path to the working copy (git worktree / jj workspace) to apply edits to. \
                            Defaults to the project root, or the working copy containing an absolute `file`."
        }),
    );

    let file_prop = json!({ "type": "string", "description": "File path, relative to the project root unless absolute." });
    let line_prop = |what: &str| json!({ "type": "integer", "minimum": 0, "description": format!("Zero-based {what} line.") });
    let char_prop = |what: &str| json!({ "type": "integer", "minimum": 0, "description": format!("Zero-based {what} character (UTF-16).") });

    match descriptor.target {
        TargetKind::Position => {
            props.insert("file".into(), file_prop);
            props.insert("line".into(), line_prop("target"));
            props.insert("character".into(), char_prop("target"));
            props.insert("expect".into(), json!({
                "type": "string",
                "description": "Optional guard: the identifier you expect at this position. \
                                Henka checks its own copy of the file matches and errors instead \
                                of acting on a mis-resolved coordinate (e.g. one computed against \
                                a different revision)."
            }));
            required.extend(["file".into(), "line".into(), "character".into()]);
        }
        TargetKind::Selection => {
            props.insert("file".into(), file_prop);
            props.insert("start_line".into(), line_prop("selection start"));
            props.insert("start_character".into(), char_prop("selection start"));
            props.insert("end_line".into(), line_prop("selection end"));
            props.insert("end_character".into(), char_prop("selection end"));
            props.insert("expect".into(), json!({
                "type": "string",
                "description": "Optional guard: the exact text you expect the selection to cover. \
                                Henka checks its own copy of the file matches and errors instead \
                                of acting on a mis-resolved range."
            }));
            required.extend([
                "file".into(),
                "start_line".into(),
                "start_character".into(),
                "end_line".into(),
                "end_character".into(),
            ]);
        }
        TargetKind::File => {
            props.insert("file".into(), file_prop);
            required.push("file".into());
        }
        TargetKind::Project => {}
    }

    // Merge the operation's own parameters.
    if let Value::Object(schema) = &descriptor.params_schema {
        if let Some(Value::Object(params)) = schema.get("properties") {
            for (k, v) in params {
                props.insert(k.clone(), v.clone());
            }
        }
        if let Some(Value::Array(req)) = schema.get("required") {
            required.extend(req.iter().cloned());
        }
    }

    if descriptor.kind == OperationKind::Edit {
        props.insert(
            "dry_run".into(),
            json!({
                "type": "boolean",
                "default": true,
                "description": "If true (the default), return a diff without modifying files. Pass false to apply."
            }),
        );
    }

    let schema = json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": required,
    });
    match schema {
        Value::Object(map) => Arc::new(map),
        _ => Arc::new(JsonObject::new()),
    }
}

/// Extract the project id from call arguments.
pub fn project_id(args: &JsonObject) -> Result<String, McpError> {
    args.get("project")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| McpError::invalid_params("missing required `project`", None))
}

/// The explicit `workspace` path from call arguments, if given.
pub fn workspace(args: &JsonObject) -> Option<PathBuf> {
    args.get("workspace")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// The caller's expected identifier/selection text at the target, if given.
pub fn expect(args: &JsonObject) -> Option<String> {
    args.get("expect")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Whether the call requested a preview (defaults to true for edits).
pub fn dry_run(args: &JsonObject) -> bool {
    args.get("dry_run").and_then(Value::as_bool).unwrap_or(true)
}

/// Build the operation [`Target`] from call arguments for the given target kind.
pub fn parse_target(args: &JsonObject, kind: TargetKind) -> Result<Target, McpError> {
    match kind {
        TargetKind::Position => Ok(Target::Position {
            file: get_str(args, "file")?.into(),
            position: Position::new(get_u32(args, "line")?, get_u32(args, "character")?),
        }),
        TargetKind::Selection => Ok(Target::Selection {
            file: get_str(args, "file")?.into(),
            range: Range::new(
                Position::new(
                    get_u32(args, "start_line")?,
                    get_u32(args, "start_character")?,
                ),
                Position::new(get_u32(args, "end_line")?, get_u32(args, "end_character")?),
            ),
        }),
        TargetKind::File => Ok(Target::File {
            file: get_str(args, "file")?.into(),
        }),
        TargetKind::Project => Ok(Target::Project),
    }
}

/// The operation-specific parameters: all arguments minus the envelope fields.
pub fn operation_params(args: &JsonObject) -> Value {
    let params: Map<String, Value> = args
        .iter()
        .filter(|(k, _)| !ENVELOPE_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Value::Object(params)
}

fn get_str(args: &JsonObject, key: &str) -> Result<String, McpError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| McpError::invalid_params(format!("missing or non-string `{key}`"), None))
}

fn get_u32(args: &JsonObject, key: &str) -> Result<u32, McpError> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            McpError::invalid_params(
                format!("missing or invalid `{key}` (expected a non-negative integer)"),
                None,
            )
        })
}

/// The cap a query's result lists have to respect: the caller's `limit`, else
/// the default the operation declares for it. `None` when the operation takes
/// no `limit` at all.
pub fn result_limit(params: &Value, params_schema: &Value) -> Option<usize> {
    params
        .get("limit")
        .or_else(|| params_schema.pointer("/properties/limit/default"))
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

/// Combine the results of one project-scoped query that several languages
/// answered into a single result holding at most `limit` of each kind of match.
///
/// A project-scoped query names no file, so in a mixed-language project every
/// language's server can hold part of the answer. The result shapes belong to
/// the operations, so the merge is structural, matching how those results are
/// built — a list of findings alongside counters and flags describing it:
/// objects merge key by key, lists concatenate, counts add, flags or together,
/// and anything else keeps the first language's answer.
///
/// Every language applied the caller's cap to its own answer, so their
/// concatenation can exceed it; the merged lists are cut back to it, while the
/// counts keep reporting everything matched, as they do for a single language.
pub fn merge_query_results(results: Vec<Value>, limit: Option<usize>) -> Value {
    let merged = results.into_iter().reduce(merge).unwrap_or_else(|| json!({}));
    match limit {
        Some(limit) => cap_lists(merged, limit),
        None => merged,
    }
}

/// Cut every list in a merged result back to `limit`, flagging `truncated` when
/// one was shortened — the same signal an operation raises when it caps its own
/// answer, so a caller can tell it has not seen everything.
fn cap_lists(mut merged: Value, limit: usize) -> Value {
    let Value::Object(fields) = &mut merged else {
        return merged;
    };
    let mut truncated = false;
    for value in fields.values_mut() {
        if let Value::Array(items) = value
            && items.len() > limit
        {
            items.truncate(limit);
            truncated = true;
        }
    }
    if truncated {
        fields.insert("truncated".into(), json!(true));
    }
    merged
}

fn merge(left: Value, right: Value) -> Value {
    match (left, right) {
        (Value::Object(mut left), Value::Object(right)) => {
            for (key, value) in right {
                let merged = match left.remove(&key) {
                    Some(existing) => merge(existing, value),
                    None => value,
                };
                left.insert(key, merged);
            }
            Value::Object(left)
        }
        (Value::Array(mut left), Value::Array(right)) => {
            left.extend(right);
            Value::Array(left)
        }
        (Value::Bool(left), Value::Bool(right)) => json!(left || right),
        (Value::Number(left), Value::Number(right)) => {
            match (left.as_u64(), right.as_u64()) {
                (Some(left), Some(right)) => json!(left + right),
                _ => Value::Number(left),
            }
        }
        (left, _) => left,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merging_one_result_leaves_it_alone() {
        let one = json!({ "count": 1, "symbols": [{ "name": "Foo" }] });
        assert_eq!(merge_query_results(vec![one.clone()], None), one);
    }

    #[test]
    fn merging_concatenates_lists_adds_counts_and_ors_flags() {
        let merged = merge_query_results(
            vec![
                json!({ "count": 2, "symbols": ["a", "b"], "truncated": false }),
                json!({ "count": 1, "symbols": ["c"], "truncated": true }),
            ],
            None,
        );
        assert_eq!(
            merged,
            json!({ "count": 3, "symbols": ["a", "b", "c"], "truncated": true })
        );
    }

    #[test]
    fn merging_keeps_keys_only_one_language_reported() {
        let merged = merge_query_results(
            vec![json!({ "symbols": [] }), json!({ "symbols": [], "truncated": true })],
            None,
        );
        assert_eq!(merged, json!({ "symbols": [], "truncated": true }));
    }

    #[test]
    fn merging_nothing_is_an_empty_result() {
        assert_eq!(merge_query_results(vec![], None), json!({}));
    }
    #[test]
    fn merged_lists_are_cut_back_to_the_limit() {
        // Each language returned one match under `limit: 1`, so neither capped
        // its own answer; their concatenation is cut back and flagged, while
        // `count` still reports everything that matched.
        let merged = merge_query_results(
            vec![json!({ "count": 1, "symbols": ["a"] }), json!({ "count": 1, "symbols": ["b"] })],
            Some(1),
        );
        assert_eq!(merged, json!({ "count": 2, "symbols": ["a"], "truncated": true }));
    }

    #[test]
    fn a_merged_list_within_the_limit_is_not_flagged() {
        let merged = merge_query_results(
            vec![json!({ "count": 1, "symbols": ["a"] }), json!({ "count": 1, "symbols": ["b"] })],
            Some(2),
        );
        assert_eq!(merged, json!({ "count": 2, "symbols": ["a", "b"] }));
    }

    #[test]
    fn the_limit_is_the_callers_else_the_operations_default() {
        let schema = json!({ "properties": { "limit": { "default": 200 } } });
        assert_eq!(result_limit(&json!({ "limit": 5 }), &schema), Some(5));
        assert_eq!(result_limit(&json!({}), &schema), Some(200));
        assert_eq!(result_limit(&json!({}), &json!({ "properties": {} })), None);
    }
}
