//! Integration tests for the Java operations, exercised against a real jdtls.
//!
//! Ignored by default (they launch a JVM). Run with:
//!
//! ```text
//! cargo test -p henka-lang-java -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use henka_core::operation::{Operation, OperationCtx, OperationOutcome, OperationRequest, Target};
use henka_core::{EditApplier, Language, LanguageSession, Position, Project, Range};
use henka_lang_java::operations::{ChangeSignatureOp, CodeActionOp, FindUsagesOp, RenameOp};
use henka_lang_java::{JdtlsInstall, JdtlsSession};
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
    // Load the delegate-command bundle when built, so parameterized refactorings
    // (change-signature) are available.
    let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("jdtls-bundle/henka-jdtls-bundle.jar");
    let bundles: Vec<PathBuf> = bundle.is_file().then_some(bundle).into_iter().collect();
    let session = JdtlsSession::start(&install, root, &data, &bundles)
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
        henka_core::OperationOutcome::Edit(edit) => edit,
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
        henka_core::OperationOutcome::Query(value) => value,
        _ => panic!("find-usages should produce a query result"),
    };

    let count = value.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
    assert!(count >= 1, "expected at least one usage, got: {value}");
}

/// Find a code-action operation by id from the Java set.
fn op(id: &str) -> std::sync::Arc<dyn Operation> {
    CodeActionOp::java_set()
        .into_iter()
        .find(|o| o.descriptor().id == id)
        .unwrap_or_else(|| panic!("operation `{id}` not found"))
}

/// Run an edit operation and apply its result to the working tree.
async fn run_and_apply(
    operation: &dyn Operation,
    ctx: &OperationCtx<'_>,
    req: OperationRequest,
    root: &Path,
) {
    let outcome = operation
        .run(ctx, &req)
        .await
        .expect("operation should succeed");
    let edit = match outcome {
        OperationOutcome::Edit(edit) => edit,
        _ => panic!("expected an edit"),
    };
    assert!(!edit.is_empty(), "expected a non-empty edit");
    EditApplier::apply(&edit, root).expect("edit should apply");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn extract_variable_into_local() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Calc.java"),
        "public class Calc {\n    int compute() {\n        return 1 + 2 + 3;\n    }\n}\n",
    )
    .unwrap();

    let session = session_for(root).await;
    let project = project(root);
    let ctx = OperationCtx {
        project: &project,
        session,
    };

    // Select `1 + 2 + 3` on line 2 (cols 15..24).
    let selection = || Target::Selection {
        file: PathBuf::from("Calc.java"),
        range: Range::new(Position::new(2, 15), Position::new(2, 24)),
    };

    run_and_apply(
        op("extract-variable").as_ref(),
        &ctx,
        OperationRequest {
            target: selection(),
            params: json!({}),
        },
        root,
    )
    .await;
    let after = std::fs::read_to_string(root.join("Calc.java")).unwrap();
    assert!(
        after.contains("= 1 + 2 + 3"),
        "extracted a variable: {after}"
    );
    assert!(after.contains("return i"), "uses the variable: {after}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM with the bundle; run with --ignored"]
async fn change_signature_reorders_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Greeting.java"),
        "public class Greeting {\n    public int add(int a, int b) {\n        return a + b;\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Main.java"),
        "public class Main {\n    void run() {\n        System.out.println(new Greeting().add(1, 2));\n    }\n}\n",
    )
    .unwrap();

    let session = session_for(root).await;
    let project = project(root);
    let ctx = OperationCtx {
        project: &project,
        session,
    };

    // Method `add` is at line 1, char 15. Reorder params to (b, a).
    let req = OperationRequest {
        target: Target::Position {
            file: PathBuf::from("Greeting.java"),
            position: Position::new(1, 15),
        },
        params: json!({
            "parameters": [
                { "type": "int", "name": "b", "original_index": 1 },
                { "type": "int", "name": "a", "original_index": 0 }
            ]
        }),
    };

    let outcome = ChangeSignatureOp
        .run(&ctx, &req)
        .await
        .expect("change-signature should succeed");
    let edit = match outcome {
        OperationOutcome::Edit(edit) => edit,
        _ => panic!("expected an edit"),
    };
    assert!(!edit.is_empty(), "expected a non-empty edit");
    EditApplier::apply(&edit, root).expect("edit should apply");

    let greeting = std::fs::read_to_string(root.join("Greeting.java")).unwrap();
    let main = std::fs::read_to_string(root.join("Main.java")).unwrap();
    assert!(
        greeting.contains("add(int b, int a)"),
        "declaration reordered: {greeting}"
    );
    assert!(main.contains("add(2, 1)"), "call site reordered: {main}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn organize_imports_removes_unused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Imp.java"),
        "import java.util.List;\nimport java.util.ArrayList;\n\npublic class Imp {\n    List<String> x;\n}\n",
    )
    .unwrap();

    let session = session_for(root).await;
    let project = project(root);
    let ctx = OperationCtx {
        project: &project,
        session,
    };

    run_and_apply(
        op("organize-imports").as_ref(),
        &ctx,
        OperationRequest {
            target: Target::File {
                file: PathBuf::from("Imp.java"),
            },
            params: json!({}),
        },
        root,
    )
    .await;

    let after = std::fs::read_to_string(root.join("Imp.java")).unwrap();
    assert!(
        after.contains("import java.util.List;"),
        "kept used import: {after}"
    );
    assert!(
        !after.contains("ArrayList"),
        "removed unused import: {after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn inline_local_variable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Inl.java"),
        "public class Inl {\n    int f() {\n        int x = 5;\n        return x + 1;\n    }\n}\n",
    )
    .unwrap();

    let session = session_for(root).await;
    let project = project(root);
    let ctx = OperationCtx {
        project: &project,
        session,
    };

    // Position on `x` in its declaration (line 2, col 12).
    run_and_apply(
        op("inline").as_ref(),
        &ctx,
        OperationRequest {
            target: Target::Position {
                file: PathBuf::from("Inl.java"),
                position: Position::new(2, 12),
            },
            params: json!({}),
        },
        root,
    )
    .await;

    let after = std::fs::read_to_string(root.join("Inl.java")).unwrap();
    assert!(
        !after.contains("int x = 5"),
        "removed the declaration: {after}"
    );
    assert!(after.contains("return 5 + 1"), "inlined the value: {after}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn inline_method() {
    // jdtls offers two `refactor.inline` actions at a method — "Inline Method"
    // and "Make Static" — so this exercises that `inline` selects the actual
    // inline refactoring rather than a neighbour sharing the kind.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Inl.java"),
        "public class Inl {\n    int f() {\n        return g() + 1;\n    }\n    int g() {\n        return 5;\n    }\n}\n",
    )
    .unwrap();

    let session = session_for(root).await;
    let project = project(root);
    let ctx = OperationCtx {
        project: &project,
        session,
    };

    // Position on `g` in its declaration (line 4, col 8).
    run_and_apply(
        op("inline").as_ref(),
        &ctx,
        OperationRequest {
            target: Target::Position {
                file: PathBuf::from("Inl.java"),
                position: Position::new(4, 8),
            },
            params: json!({}),
        },
        root,
    )
    .await;

    let after = std::fs::read_to_string(root.join("Inl.java")).unwrap();
    assert!(
        after.contains("return 5 + 1"),
        "inlined the method body at the call site: {after}"
    );
    assert!(
        !after.contains("int g()"),
        "removed the inlined method: {after}"
    );
}
