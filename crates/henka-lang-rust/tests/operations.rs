//! Integration tests for the Rust operations against a real rust-analyzer.
//!
//! Ignored by default (they launch rust-analyzer) and skip when no analyzer is
//! available. Run with:
//!
//! ```text
//! cargo test -p henka-lang-rust -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use henka_core::operation::{Operation, OperationCtx, OperationOutcome, OperationRequest, Target};
use henka_core::{EditApplier, Language, LanguageSession, Position, Project, Range};
use henka_lang_rust::analyzer::{RaSession, locate};
use henka_lang_rust::operations::{CodeActionOp, FindUsagesOp, RenameOp};
use serde_json::json;

/// The rust-analyzer binary to test with: the one bundled under the repo's
/// `.cache` (fetched by `cargo xtask rust-analyzer`), else whatever `locate`
/// finds. Returns `None` if no analyzer is available, so the test can skip.
fn ra_bin() -> Option<PathBuf> {
    let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.cache/rust-analyzer/rust-analyzer");
    if bundled.is_file() {
        return Some(bundled);
    }
    locate().ok()
}

/// Write a small cargo library crate where `run` calls `greet`.
fn write_project(root: &Path) {
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn greet() -> &'static str {\n    \"hi\"\n}\n\npub fn run() {\n    let _ = greet();\n}\n",
    )
    .unwrap();
}

async fn session_for(root: &Path) -> Option<Arc<dyn LanguageSession>> {
    let program = ra_bin()?;
    let session = RaSession::start(&program, root)
        .await
        .expect("rust-analyzer should initialize");
    Some(Arc::new(session))
}

fn project(root: &Path) -> Project {
    Project {
        id: "demo".into(),
        root: root.to_path_buf(),
        languages: vec![Language::Rust],
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches rust-analyzer; run with --ignored"]
async fn rename_updates_references_across_functions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_project(root);

    let Some(session) = session_for(root).await else {
        return; // no rust-analyzer available
    };
    let project = project(root);
    let ctx = OperationCtx {
        project: &project,
        session,
    };

    // `greet` is at line 0, character 7 in src/lib.rs.
    let req = OperationRequest {
        target: Target::Position {
            file: Path::new("src/lib.rs").to_path_buf(),
            position: Position::new(0, 7),
        },
        params: json!({ "new_name": "greeting" }),
    };

    let outcome = RenameOp.run(&ctx, &req).await.expect("rename should succeed");
    let edit = match outcome {
        OperationOutcome::Edit(edit) => edit,
        _ => panic!("rename should produce an edit"),
    };
    assert!(!edit.is_empty(), "rename should produce edits");
    EditApplier::apply(&edit, root).expect("edit should apply");

    let lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(lib.contains("fn greeting()"), "declaration renamed: {lib}");
    assert!(lib.contains("greeting();"), "call site renamed: {lib}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches rust-analyzer; run with --ignored"]
async fn find_usages_locates_references() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_project(root);

    let Some(session) = session_for(root).await else {
        return;
    };
    let project = project(root);
    let ctx = OperationCtx {
        project: &project,
        session,
    };

    let req = OperationRequest {
        target: Target::Position {
            file: Path::new("src/lib.rs").to_path_buf(),
            position: Position::new(0, 7),
        },
        params: json!({ "include_declaration": true }),
    };

    let outcome = FindUsagesOp
        .run(&ctx, &req)
        .await
        .expect("find-usages should succeed");
    let value = match outcome {
        OperationOutcome::Query(value) => value,
        _ => panic!("find-usages should produce a query result"),
    };

    let count = value.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
    assert!(count >= 1, "expected at least one usage, got: {value}");
}

/// Find a code-action operation by id from the Rust set.
fn op(id: &str) -> Arc<dyn Operation> {
    CodeActionOp::rust_set()
        .into_iter()
        .find(|o| o.descriptor().id == id)
        .unwrap_or_else(|| panic!("operation `{id}` not found"))
}

/// Write a crate whose `calc` returns the expression `1 + 2 + 3` on line 1,
/// columns 4..13 — a stable selection for the extract refactorings.
fn write_calc(root: &Path) {
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn calc() -> i32 {\n    1 + 2 + 3\n}\n",
    )
    .unwrap();
}

async fn run_edit(op: &dyn Operation, ctx: &OperationCtx<'_>, target: Target, root: &Path) -> String {
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
    std::fs::read_to_string(root.join("src/lib.rs")).unwrap()
}

fn calc_selection() -> Target {
    Target::Selection {
        file: Path::new("src/lib.rs").to_path_buf(),
        range: Range::new(Position::new(1, 4), Position::new(1, 13)),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches rust-analyzer; run with --ignored"]
async fn extract_variable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_calc(root);
    let Some(session) = session_for(root).await else {
        return;
    };
    let project = project(root);
    let ctx = OperationCtx { project: &project, session };

    let after = run_edit(op("extract-variable").as_ref(), &ctx, calc_selection(), root).await;
    assert!(after.contains("let "), "introduced a variable: {after}");
    assert!(after.contains("= 1 + 2 + 3"), "bound the expression: {after}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches rust-analyzer; run with --ignored"]
async fn extract_constant() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_calc(root);
    let Some(session) = session_for(root).await else {
        return;
    };
    let project = project(root);
    let ctx = OperationCtx { project: &project, session };

    let after = run_edit(op("extract-constant").as_ref(), &ctx, calc_selection(), root).await;
    assert!(after.contains("const "), "introduced a constant: {after}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches rust-analyzer; run with --ignored"]
async fn extract_function() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_calc(root);
    let Some(session) = session_for(root).await else {
        return;
    };
    let project = project(root);
    let ctx = OperationCtx { project: &project, session };

    let after = run_edit(op("extract-function").as_ref(), &ctx, calc_selection(), root).await;
    assert!(after.contains("fn fun_name"), "extracted a function: {after}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches rust-analyzer; run with --ignored"]
async fn inline_local_variable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // `x` is declared on line 1 at column 8.
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn g() -> i32 {\n    let x = 5;\n    x + 1\n}\n",
    )
    .unwrap();
    let Some(session) = session_for(root).await else {
        return;
    };
    let project = project(root);
    let ctx = OperationCtx { project: &project, session };

    let after = run_edit(
        op("inline").as_ref(),
        &ctx,
        Target::Position {
            file: Path::new("src/lib.rs").to_path_buf(),
            position: Position::new(1, 8),
        },
        root,
    )
    .await;
    assert!(!after.contains("let x = 5"), "removed the declaration: {after}");
    assert!(after.contains("5 + 1"), "inlined the value: {after}");
}
