//! Integration test for the jdtls lifecycle.
//!
//! Ignored by default because it launches a real JVM running Eclipse JDT LS.
//! Run with a jdtls distribution present (e.g. `scripts/fetch-jdtls.sh`):
//!
//! ```text
//! cargo test -p refactor-lang-java -- --ignored
//! ```

use std::path::PathBuf;

use refactor_lang_java::{JdtlsInstall, JdtlsSession};

/// The repo's default jdtls cache, relative to this crate.
fn jdtls_home() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".cache/jdtls")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real jdtls JVM; run with --ignored"]
async fn initializes_and_opens_a_file() {
    let install = JdtlsInstall::at(jdtls_home()).expect("a jdtls distribution under .cache/jdtls");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/main/java")).unwrap();
    std::fs::write(
        root.join("src/main/java/Main.java"),
        "public class Main {\n    public static void main(String[] args) {\n        System.out.println(\"hi\");\n    }\n}\n",
    )
    .unwrap();

    let data = root.join("data");
    let session = JdtlsSession::start(&install, root, &data, &[])
        .await
        .expect("jdtls should initialize");

    let uri = session
        .ensure_open(&PathBuf::from("src/main/java/Main.java"))
        .await
        .expect("opening a document should succeed");
    assert!(uri.starts_with("file://"));
    assert!(uri.ends_with("Main.java"));

    session.shutdown().await.unwrap();
}
