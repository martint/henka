//! Henka build automation, wired in as `cargo xtask`.
//!
//! A full build is more than `cargo build`: the Java provider needs a jdtls
//! distribution and a small OSGi delegate-command bundle compiled against it.
//! Those steps already live in shell scripts; xtask sequences them with the
//! release build so a fresh checkout is one command:
//!
//! ```text
//! cargo xtask build     # fetch jdtls if missing, build the bundle, cargo build --release
//! cargo xtask jdtls     # fetch the jdtls distribution into .cache/jdtls
//! cargo xtask bundle    # recompile the delegate-command bundle only
//! ```
//!
//! It shells out to the canonical scripts rather than reimplementing them, so
//! there is one source of truth for how jdtls and the bundle are produced.

use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  build     full build: fetch jdtls if missing, build the bundle, cargo build --release
  jdtls     fetch the jdtls distribution into .cache/jdtls
  bundle    compile the jdtls delegate-command bundle
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sub = args.first().map(String::as_str).unwrap_or("build");
    let result = match sub {
        "build" => cmd_build(),
        "jdtls" => cmd_jdtls(),
        "bundle" => cmd_bundle(),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    if let Err(msg) = result {
        eprintln!("xtask: {msg}");
        std::process::exit(1);
    }
}

/// Full build. Ensures jdtls is present (fetching it, which also builds the
/// bundle), guarantees the bundle exists, then builds the release binary.
fn cmd_build() -> Result<(), String> {
    let root = workspace_root();

    if !jdtls_present(&root) {
        step("jdtls not found, fetching");
        // fetch-jdtls.sh builds the bundle as its final step, so this covers both.
        run_script(&root, "scripts/fetch-jdtls.sh", &[])?;
    } else if !bundle_present(&root) {
        step("building jdtls delegate-command bundle");
        run_script(&root, "jdtls-bundle/build.sh", &[])?;
    } else {
        step("jdtls and bundle already present, skipping");
    }

    step("building henka (release)");
    cargo_build(&root)
}

/// Fetch (or refresh) the jdtls distribution.
fn cmd_jdtls() -> Result<(), String> {
    let root = workspace_root();
    run_script(&root, "scripts/fetch-jdtls.sh", &[])
}

/// Recompile the delegate-command bundle against the local jdtls.
fn cmd_bundle() -> Result<(), String> {
    let root = workspace_root();
    if !jdtls_present(&root) {
        return Err("jdtls not found; run `cargo xtask jdtls` first".into());
    }
    run_script(&root, "jdtls-bundle/build.sh", &[])
}

/// Whether a jdtls distribution has been unpacked into the default location.
fn jdtls_present(root: &Path) -> bool {
    root.join(".cache/jdtls/plugins").is_dir()
}

/// Whether the delegate-command bundle jar has been built.
fn bundle_present(root: &Path) -> bool {
    root.join("jdtls-bundle/henka-jdtls-bundle.jar").is_file()
}

/// Build the release binary, honoring cargo's `$CARGO` so the same toolchain
/// that launched xtask is reused.
fn cargo_build(root: &Path) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    status(
        Command::new(cargo)
            .current_dir(root)
            .args(["build", "--release", "--package", "henka-server"]),
    )
}

/// Run a repo build script through bash, from the workspace root.
fn run_script(root: &Path, rel: &str, args: &[&str]) -> Result<(), String> {
    let script = root.join(rel);
    if !script.is_file() {
        return Err(format!("missing build script: {}", script.display()));
    }
    status(Command::new("bash").current_dir(root).arg(&script).args(args))
}

/// Run a command to completion, inheriting stdio so the user sees its output.
/// Maps spawn failure and any non-zero exit to an error.
fn status(cmd: &mut Command) -> Result<(), String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let st = cmd
        .status()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {}", st.code().unwrap_or(-1)))
    }
}

/// Announce a build step on stderr, kept distinct from the scripts' own output.
fn step(msg: &str) {
    eprintln!("\n▶ {msg}");
}

/// The workspace root: xtask lives at `<root>/xtask`, so its manifest dir's
/// parent is the root.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is a child of the workspace root")
        .to_path_buf()
}
