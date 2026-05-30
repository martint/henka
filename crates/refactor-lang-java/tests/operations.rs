//! Integration tests for the Java operations, exercised against a real jdtls.
//!
//! Ignored by default (they launch a JVM). Run with:
//!
//! ```text
//! cargo test -p refactor-lang-java -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use refactor_core::operation::{Operation, OperationCtx, OperationRequest, Target};
use refactor_core::{EditApplier, Language, LanguageSession, Position, Project};
use refactor_lang_java::operations::{FindUsagesOp, RenameOp};
use refactor_lang_java::{JdtlsInstall, JdtlsSession};
use serde_json::json;

fn jdtls_home() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".cache/jdtls")
}

/// Create a two-file Java project where `Main` calls `Greeting.greet()`.
///
/// Flat files (a jdtls "invisible project") are used so the test runs offline,
/// without a Maven import. The operations call `ensure_indexed`, which opens the
/// project's sources so jdtls resolves them and cross-file results are complete.
fn write_project(root: &Path) {
    std::fs::write(
        root.join("Greeting.java"),
        "public class Greeting {\n    public String greet() {\n        return \"hi\";\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Main.java"),
        "public class Main {\n    public static void main(String[] args) {\n        System.out.println(new Greeting().greet());\n    }\n}\n",
    )
    .unwrap();
}

/// Path to a source file within the project.
fn src(name: &str) -> PathBuf {
    PathBuf::from(name)
}

async fn session_for(root: &Path) -> Arc<dyn LanguageSession> {
    let install = JdtlsInstall::at(jdtls_home()).expect("a jdtls distribution under .cache/jdtls");
    let data = root.join(".data");
    let session = JdtlsSession::start(&install, root, &data)
        .await
        .expect("jdtls should initialize");
    Arc::new(session)
}

fn project(root: &Path) -> Project {
    Project {
        id: "demo".into(),
        root: root.to_path_buf(),
        languages: vec![Language::Java],
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn rename_updates_references_across_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_project(root);

    let session = session_for(root).await;
    let project = project(root);
    let ctx = OperationCtx {
        project: &project,
        session,
    };

    // `greet` is at line 1, character 18 in Greeting.java.
    let req = OperationRequest {
        target: Target::Position {
            file: src("Greeting.java"),
            position: Position::new(1, 18),
        },
        params: json!({ "new_name": "greeting" }),
    };

    let outcome = RenameOp
        .run(&ctx, &req)
        .await
        .expect("rename should succeed");
    let edit = match outcome {
        refactor_core::OperationOutcome::Edit(edit) => edit,
        _ => panic!("rename should produce an edit"),
    };
    assert!(!edit.is_empty(), "rename should produce edits");

    EditApplier::apply(&edit, root).expect("edit should apply");

    let greeting = std::fs::read_to_string(root.join(src("Greeting.java"))).unwrap();
    let main = std::fs::read_to_string(root.join(src("Main.java"))).unwrap();
    assert!(
        greeting.contains("String greeting()"),
        "declaration renamed: {greeting}"
    );
    assert!(main.contains(".greeting()"), "call site renamed: {main}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn find_usages_locates_references() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_project(root);

    let session = session_for(root).await;
    let project = project(root);
    let ctx = OperationCtx {
        project: &project,
        session,
    };

    let req = OperationRequest {
        target: Target::Position {
            file: src("Greeting.java"),
            position: Position::new(1, 18),
        },
        params: json!({ "include_declaration": true }),
    };

    let outcome = FindUsagesOp
        .run(&ctx, &req)
        .await
        .expect("find-usages should succeed");
    let value = match outcome {
        refactor_core::OperationOutcome::Query(value) => value,
        _ => panic!("find-usages should produce a query result"),
    };

    let count = value.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
    assert!(count >= 1, "expected at least one usage, got: {value}");
}
