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
            required.extend(["file".into(), "line".into(), "character".into()]);
        }
        TargetKind::Selection => {
            props.insert("file".into(), file_prop);
            props.insert("start_line".into(), line_prop("selection start"));
            props.insert("start_character".into(), char_prop("selection start"));
            props.insert("end_line".into(), line_prop("selection end"));
            props.insert("end_character".into(), char_prop("selection end"));
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
