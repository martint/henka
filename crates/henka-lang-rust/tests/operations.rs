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
use henka_core::{EditApplier, Language, LanguageSession, Position, Project};
use henka_lang_rust::analyzer::{RaSession, locate};
use henka_lang_rust::operations::{FindUsagesOp, RenameOp};
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
