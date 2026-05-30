//! Integration tests for worktree/workspace-aware refactoring against a real
//! jdtls: one shared index per repository, with each working copy's content
//! overlaid per request and the edit retargeted onto that working copy.
//!
//! Ignored by default (they launch a JVM) and additionally skip when the
//! required VCS tool is unavailable. Run with:
//!
//! ```text
//! cargo test -p henka-lang-java --test workspaces -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use henka_core::operation::{Operation, OperationCtx, OperationOutcome, OperationRequest, Target};
use henka_core::{
    AppliedEdit, EditApplier, Language, LanguageSession, Position, Project, Range,
    working_copy_delta,
};
use henka_lang_java::operations::{CodeActionOp, RenameOp};
use henka_lang_java::{JdtlsInstall, JdtlsSession};
use serde_json::{Value, json};

fn jdtls_home() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".cache/jdtls")
}

/// Start a jdtls session rooted at `root`, mirroring the production provider.
async fn start_session(root: &Path) -> Arc<dyn LanguageSession> {
    let install = JdtlsInstall::at(jdtls_home()).expect("a jdtls distribution under .cache/jdtls");
    let data = root.join(".data");
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

/// A `Calc.java` whose `compute` returns the given three-term expression.
fn calc_source(expr: &str) -> String {
    format!("public class Calc {{\n    int compute() {{\n        return {expr};\n    }}\n}}\n")
}

/// The whole pipeline the server runs for one request: serialize, overlay the
/// working copy's delta onto the shared index, run the operation, restore the
/// overlay, retarget the edit onto the working copy, and apply it there.
async fn refactor_in_workspace(
    session: &Arc<dyn LanguageSession>,
    project: &Project,
    workspace: &Path,
    op: &dyn Operation,
    target: Target,
    params: Value,
) -> AppliedEdit {
    let _guard = session.begin_request().await;
    let delta = working_copy_delta(workspace);
    session
        .overlay_workspace(workspace, &delta)
        .await
        .expect("overlay");
    let ctx = OperationCtx {
        project,
        session: Arc::clone(session),
    };
    let outcome = op.run(&ctx, &OperationRequest { target, params }).await;
    session.restore_overlay().await;

    let mut edit = match outcome.expect("operation should succeed") {
        OperationOutcome::Edit(edit) => edit,
        _ => panic!("expected an edit"),
    };
    if let Some(root) = session.root() {
        edit.retarget(root, workspace);
    }
    EditApplier::apply(&edit, workspace).expect("edit should apply")
}

/// Run a git command; returns false if git is unavailable or the command fails.
fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Initialize a git repo at `root` and commit whatever it currently contains.
/// Returns false (so the test can skip) if git is unavailable.
fn git_init_commit(root: &Path) -> bool {
    git(root, &["init", "-q"])
        && git(root, &["config", "user.email", "t@t"])
        && git(root, &["config", "user.name", "t"])
        && git(root, &["add", "."])
        && git(root, &["commit", "-q", "-m", "base"])
}

/// The standard extract-variable request on the `1 + 2 + 3`-style expression at
/// line 2, columns 15..24 of `Calc.java`.
fn extract_variable_target() -> Target {
    Target::Selection {
        file: PathBuf::from("Calc.java"),
        range: Range::new(Position::new(2, 15), Position::new(2, 24)),
    }
}

fn extract_variable_op() -> Arc<dyn Operation> {
    CodeActionOp::java_set()
        .into_iter()
        .find(|o| o.descriptor().id == "extract-variable")
        .expect("extract-variable op")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn git_worktree_edit_lands_in_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::write(
        main.join("Greeting.java"),
        "public class Greeting {\n    public String greet() {\n        return \"hi\";\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        main.join("Main.java"),
        "public class Main {\n    void run() {\n        System.out.println(new Greeting().greet());\n    }\n}\n",
    )
    .unwrap();
    if !git_init_commit(&main) {
        return;
    }
    let wt = dir.path().join("wt");
    if !git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]) {
        return;
    }

    // One session, rooted at the main checkout; serve the (clean) worktree.
    let session = start_session(&main).await;
    let project = project(&main);
    refactor_in_workspace(
        &session,
        &project,
        &wt,
        &RenameOp,
        Target::Position {
            file: PathBuf::from("Greeting.java"),
            position: Position::new(1, 18),
        },
        json!({ "new_name": "greeting" }),
    )
    .await;

    // The edit landed in the worktree, across both files…
    let wt_greeting = std::fs::read_to_string(wt.join("Greeting.java")).unwrap();
    let wt_main = std::fs::read_to_string(wt.join("Main.java")).unwrap();
    assert!(wt_greeting.contains("String greeting()"), "{wt_greeting}");
    assert!(wt_main.contains(".greeting()"), "{wt_main}");

    // …and the main checkout was left untouched.
    let main_greeting = std::fs::read_to_string(main.join("Greeting.java")).unwrap();
    assert!(main_greeting.contains("String greet()"), "{main_greeting}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn git_dirty_worktree_overlays_modified_file() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::write(main.join("Calc.java"), calc_source("1 + 2 + 3")).unwrap();
    if !git_init_commit(&main) {
        return;
    }
    let wt = dir.path().join("wt");
    if !git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]) {
        return;
    }
    // Diverge the worktree's working copy from the indexed (main) content.
    std::fs::write(wt.join("Calc.java"), calc_source("4 + 5 + 6")).unwrap();

    let session = start_session(&main).await;
    let project = project(&main);
    refactor_in_workspace(
        &session,
        &project,
        &wt,
        extract_variable_op().as_ref(),
        extract_variable_target(),
        json!({}),
    )
    .await;

    // The extracted initializer must reflect the worktree's content, not main's.
    let after = std::fs::read_to_string(wt.join("Calc.java")).unwrap();
    assert!(after.contains("= 4 + 5 + 6"), "overlay reflected worktree: {after}");
    let main_after = std::fs::read_to_string(main.join("Calc.java")).unwrap();
    assert!(main_after.contains("return 1 + 2 + 3"), "main untouched: {main_after}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn overlay_is_restored_between_workspaces() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::write(main.join("Calc.java"), calc_source("1 + 2 + 3")).unwrap();
    if !git_init_commit(&main) {
        return;
    }
    let wt = dir.path().join("wt");
    if !git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]) {
        return;
    }
    std::fs::write(wt.join("Calc.java"), calc_source("4 + 5 + 6")).unwrap();

    let session = start_session(&main).await;
    let project = project(&main);

    // First serve the dirty worktree (overlays 4 + 5 + 6)…
    refactor_in_workspace(
        &session,
        &project,
        &wt,
        extract_variable_op().as_ref(),
        extract_variable_target(),
        json!({}),
    )
    .await;

    // …then serve the base checkout: it must see base content, not the overlay.
    refactor_in_workspace(
        &session,
        &project,
        &main,
        extract_variable_op().as_ref(),
        extract_variable_target(),
        json!({}),
    )
    .await;

    let main_after = std::fs::read_to_string(main.join("Calc.java")).unwrap();
    assert!(
        main_after.contains("= 1 + 2 + 3"),
        "base saw base content after restore: {main_after}"
    );
}

/// Run a jj command with a throwaway user config; false on any failure.
fn jj(cfg: &Path, dir: &Path, args: &[&str]) -> bool {
    Command::new("jj")
        .env("JJ_CONFIG", cfg)
        .args(args)
        .current_dir(dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn jj_workspace_overlays_modified_file() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("jjconfig.toml");
    std::fs::write(&cfg, "[user]\nname = \"t\"\nemail = \"t@t\"\n").unwrap();

    let main = dir.path().join("main");
    if !jj(&cfg, dir.path(), &["git", "init", main.to_str().unwrap()]) {
        return;
    }
    std::fs::write(main.join("Calc.java"), calc_source("1 + 2 + 3")).unwrap();
    // Commit the base so the new workspace shares it, leaving main's checkout
    // with the base content on disk.
    if !jj(&cfg, &main, &["commit", "-m", "base"]) {
        return;
    }
    let ws = dir.path().join("ws");
    if !jj(&cfg, &main, &["workspace", "add", ws.to_str().unwrap()]) {
        return;
    }
    // Diverge the workspace's working copy.
    std::fs::write(ws.join("Calc.java"), calc_source("4 + 5 + 6")).unwrap();

    // Skip if jj didn't report the working-copy change (version/config quirk):
    // the overlay would be empty and the test would prove nothing.
    if !working_copy_delta(&ws).contains(&PathBuf::from("Calc.java")) {
        return;
    }

    let session = start_session(&main).await;
    let project = project(&main);
    refactor_in_workspace(
        &session,
        &project,
        &ws,
        extract_variable_op().as_ref(),
        extract_variable_target(),
        json!({}),
    )
    .await;

    let after = std::fs::read_to_string(ws.join("Calc.java")).unwrap();
    assert!(after.contains("= 4 + 5 + 6"), "overlay reflected workspace: {after}");
}
