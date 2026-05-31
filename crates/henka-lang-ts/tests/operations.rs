//! Integration tests for the TypeScript/JavaScript operations against a real
//! typescript-language-server.
//!
//! Ignored by default (they launch the server, which needs Node on PATH) and
//! skip when no server is available. Run with:
//!
//! ```text
//! cargo test -p henka-lang-ts -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use henka_core::operation::{Operation, OperationCtx, OperationOutcome, OperationRequest, Target};
use henka_core::{EditApplier, Language, LanguageSession, Position, Project, Range};
use henka_lang_ts::operations::{CodeActionOp, FindUsagesOp, RenameOp};
use henka_lang_ts::server::{TsSession, locate};
use serde_json::json;

/// The server binary to test with: the one bundled under the repo's `.cache`
/// (installed by `cargo xtask typescript`), else whatever `locate` finds.
fn ts_server() -> Option<PathBuf> {
    let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../.cache/typescript-language-server/node_modules/.bin/typescript-language-server",
    );
    if bundled.is_file() {
        return Some(bundled);
    }
    locate().ok()
}

async fn session_for(root: &Path) -> Option<Arc<dyn LanguageSession>> {
    let program = ts_server()?;
    let session = TsSession::start(&program, root)
        .await
        .expect("typescript-language-server should initialize");
    Some(Arc::new(session))
}

fn project(root: &Path) -> Project {
    Project {
        id: "demo".into(),
        root: root.to_path_buf(),
        languages: vec![Language::TypeScript, Language::JavaScript],
    }
}

fn write(root: &Path, name: &str, content: &str) {
    std::fs::write(root.join("tsconfig.json"), "{ \"include\": [\".\"] }").unwrap();
    std::fs::write(root.join(name), content).unwrap();
}

async fn run_edit(op: &dyn Operation, ctx: &OperationCtx<'_>, target: Target, root: &Path, file: &str) -> String {
    let outcome = op
        .run(ctx, &OperationRequest { target, params: json!({}) })
        .await
        .expect("operation should succeed");
    let edit = match outcome {
        OperationOutcome::Edit(edit) => edit,
        _ => panic!("expected an edit"),
    };
    assert!(!edit.is_empty(), "expected a non-empty edit");
    EditApplier::apply(&edit, root).expect("edit should apply");
    std::fs::read_to_string(root.join(file)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches typescript-language-server (needs Node); run with --ignored"]
async fn rename_updates_references() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app.ts",
        "export function greet(): string {\n    return \"hi\";\n}\n\nexport function run(): void {\n    console.log(greet());\n}\n",
    );
    let Some(session) = session_for(root).await else {
        return;
    };
    let project = project(root);
    let ctx = OperationCtx { project: &project, session };

    // `greet` is at line 0, character 16.
    let req = OperationRequest {
        target: Target::Position {
            file: Path::new("app.ts").to_path_buf(),
            position: Position::new(0, 16),
        },
        params: json!({ "new_name": "greeting" }),
    };
    let outcome = RenameOp.run(&ctx, &req).await.expect("rename should succeed");
    let edit = match outcome {
        OperationOutcome::Edit(edit) => edit,
        _ => panic!("expected an edit"),
    };
    EditApplier::apply(&edit, root).expect("edit should apply");

    let app = std::fs::read_to_string(root.join("app.ts")).unwrap();
    assert!(app.contains("function greeting()"), "declaration renamed: {app}");
    assert!(app.contains("greeting());"), "call site renamed: {app}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches typescript-language-server (needs Node); run with --ignored"]
async fn find_usages_locates_references() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app.ts",
        "export function greet(): string {\n    return \"hi\";\n}\n\nexport function run(): void {\n    console.log(greet());\n}\n",
    );
    let Some(session) = session_for(root).await else {
        return;
    };
    let project = project(root);
    let ctx = OperationCtx { project: &project, session };

    let req = OperationRequest {
        target: Target::Position {
            file: Path::new("app.ts").to_path_buf(),
            position: Position::new(0, 16),
        },
        params: json!({ "include_declaration": true }),
    };
    let outcome = FindUsagesOp.run(&ctx, &req).await.expect("find-usages should succeed");
    let value = match outcome {
        OperationOutcome::Query(value) => value,
        _ => panic!("expected a query result"),
    };
    let count = value.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
    assert!(count >= 1, "expected at least one usage, got: {value}");
}

fn op(id: &str) -> Arc<dyn Operation> {
    CodeActionOp::ts_set()
        .into_iter()
        .find(|o| o.descriptor().id == id)
        .unwrap_or_else(|| panic!("operation `{id}` not found"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches typescript-language-server (needs Node); run with --ignored"]
async fn extract_constant() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // `1 + 2 + 3` is on line 1, columns 11..20.
    write(root, "app.ts", "export function calc(): number {\n    return 1 + 2 + 3;\n}\n");
    let Some(session) = session_for(root).await else {
        return;
    };
    let project = project(root);
    let ctx = OperationCtx { project: &project, session };

    let after = run_edit(
        op("extract-constant").as_ref(),
        &ctx,
        Target::Selection {
            file: Path::new("app.ts").to_path_buf(),
            range: Range::new(Position::new(1, 11), Position::new(1, 20)),
        },
        root,
        "app.ts",
    )
    .await;
    assert!(after.contains("const "), "introduced a constant: {after}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches typescript-language-server (needs Node); run with --ignored"]
async fn extract_function() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // `1 + 2 + 3` is on line 1, columns 11..20.
    write(root, "app.ts", "export function calc(): number {\n    return 1 + 2 + 3;\n}\n");
    let Some(session) = session_for(root).await else {
        return;
    };
    let project = project(root);
    let ctx = OperationCtx { project: &project, session };

    let after = run_edit(
        op("extract-function").as_ref(),
        &ctx,
        Target::Selection {
            file: Path::new("app.ts").to_path_buf(),
            range: Range::new(Position::new(1, 11), Position::new(1, 20)),
        },
        root,
        "app.ts",
    )
    .await;
    assert!(after.contains("function "), "introduced a function: {after}");
}
