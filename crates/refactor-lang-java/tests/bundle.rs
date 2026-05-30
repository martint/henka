//! Validate that the delegate-command bundle loads into jdtls and its commands
//! are callable. Ignored by default (launches a JVM with the bundle).

use std::path::PathBuf;

use refactor_lang_java::{JdtlsInstall, JdtlsSession};
use serde_json::{Value, json};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

async fn session_with_bundle(root: &std::path::Path) -> JdtlsSession {
    let install = JdtlsInstall::at(repo().join(".cache/jdtls")).unwrap();
    let bundle = repo().join("jdtls-bundle/refactor-jdtls-bundle.jar");
    assert!(
        bundle.is_file(),
        "build the bundle first: jdtls-bundle/build.sh"
    );
    let session = JdtlsSession::start(&install, root, &root.join(".data"), &[bundle])
        .await
        .expect("jdtls should start with the bundle");
    session.ensure_indexed().await.unwrap();
    session
}

async fn execute(session: &JdtlsSession, command: &str, arguments: Value) -> Value {
    session
        .client()
        .request(
            "workspace/executeCommand",
            json!({ "command": command, "arguments": arguments }),
        )
        .await
        .expect("delegate command should be registered and callable")
}

/// The bundle loads and `getRefactorEdit` computes an edit (here, extract a
/// variable) — proving the delegate-command mechanism end to end.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM with the bundle; run with --ignored"]
async fn bundle_get_refactor_edit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Calc.java"),
        "public class Calc {\n    int compute() {\n        return 1 + 2 + 3;\n    }\n}\n",
    )
    .unwrap();

    let session = session_with_bundle(root).await;
    let uri = session
        .ensure_open(&PathBuf::from("Calc.java"))
        .await
        .unwrap();

    let result = execute(
        &session,
        "refactor.mcp.getRefactorEdit",
        json!([{
            "command": "extractVariable",
            "context": {
                "textDocument": { "uri": uri },
                "range": { "start": {"line": 2, "character": 15}, "end": {"line": 2, "character": 24} },
                "context": { "diagnostics": [], "only": ["refactor.extract.variable"] }
            }
        }]),
    )
    .await;

    assert!(
        result.get("edit").is_some(),
        "expected a workspace edit: {result}"
    );
    session.shutdown().await.unwrap();
}

/// The bundle's `getChangeSignatureInfo` reports the current signature and
/// `getRefactorEdit{changeSignature}` computes the edit for a new one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM with the bundle; run with --ignored"]
async fn bundle_change_signature() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Greeting.java"),
        "public class Greeting {\n    public int add(int a, int b) {\n        return a + b;\n    }\n}\n",
    )
    .unwrap();

    let session = session_with_bundle(root).await;
    let uri = session
        .ensure_open(&PathBuf::from("Greeting.java"))
        .await
        .unwrap();

    let context = json!({
        "textDocument": { "uri": uri },
        "range": { "start": {"line":1,"character":15}, "end": {"line":1,"character":15} },
        "context": { "diagnostics": [], "only": ["refactor.change.signature"] }
    });

    let info = execute(
        &session,
        "refactor.mcp.getChangeSignatureInfo",
        json!([context]),
    )
    .await;
    assert_eq!(info.get("methodName").and_then(Value::as_str), Some("add"));

    let command_arguments = json!([
        info.get("methodIdentifier").cloned().unwrap(),
        false,
        "add",
        info.get("modifier").cloned().unwrap(),
        info.get("returnType").cloned().unwrap(),
        [
            { "type": "int", "name": "b", "defaultValue": Value::Null, "originalIndex": 1 },
            { "type": "int", "name": "a", "defaultValue": Value::Null, "originalIndex": 0 }
        ],
        info.get("exceptions").cloned().unwrap(),
        false
    ]);
    let result = execute(
        &session,
        "refactor.mcp.getRefactorEdit",
        json!([{ "command": "changeSignature", "context": context, "commandArguments": command_arguments }]),
    )
    .await;

    assert!(
        result
            .get("errorMessage")
            .map(Value::is_null)
            .unwrap_or(true),
        "unexpected error: {result}"
    );
    assert!(
        result.get("edit").is_some(),
        "expected a workspace edit: {result}"
    );
    session.shutdown().await.unwrap();
}
