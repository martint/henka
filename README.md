# Henka

Structured, semantics-aware code refactorings for AI agents, spoken over [MCP](https://modelcontextprotocol.io) — real refactorings computed by the language's own toolchain, not text munging.

- **Refactorings, not string edits.** Rename, extract, inline, change-signature, organize-imports, find-usages — computed by the language's real compiler view (Eclipse JDT for Java), so they hold across files and overloads. Every edit comes back as a diff you can preview before anything touches disk.
- **One server, many projects.** Henka is multi-tenant: register repositories and operate on them in place. Operations are contributed per language — **Java** (via Eclipse JDT), **Rust** (via rust-analyzer), and **TypeScript/JavaScript** (via typescript-language-server) — and the language servers are bundled, so there's nothing extra to install.
- **Worktree- and workspace-aware.** One warm index per repository, shared across git worktrees and `jj` workspaces. A refactoring lands in whichever working copy you name; the others are untouched.

The full specification is in [`docs/SPEC.md`](docs/SPEC.md).

## Build and run

Build the release binary (this also bundles the language servers it drives — fetching rust-analyzer, the Java language server, and typescript-language-server, and building the Java delegate-command bundle):

```sh
cargo xtask build          # -> target/release/henka
```

Run it over **stdio** for a single local client (the default):

```sh
henka
```

…or over **streamable HTTP** to host it for one or more clients at `/mcp`:

```sh
henka --transport http --bind 127.0.0.1:8181
```

Flags (each has effect only where noted):

| Flag | Purpose |
|------|---------|
| `--transport stdio\|http` | How clients connect. Default `stdio`. |
| `--bind <addr>` | Address for `--transport http`. Default `127.0.0.1:8181`. |
| `--config <path>` | Project registry file. Default `$XDG_CONFIG_HOME/henka/projects.toml`. |
| `--allowed-host <host>` | Extra `Host` value accepted over HTTP, beyond the loopback defaults. Repeatable. |

Environment mirrors and discovery: `HENKA_CONFIG` (registry path), `JDTLS_HOME` / `HENKA_JDTLS_BUNDLE` (Java language server + bundle), `JAVA_HOME` (JVM to launch it with), `HENKA_LOG` (log filter; logs go to stderr).

**The HTTP transport is unauthenticated.** Binding beyond loopback (e.g. `--bind 0.0.0.0:8181`) exposes every registered project to anyone who can reach the port — **wrong for anything shared**. Keep it on loopback, or front it with a reverse proxy that terminates auth.

## How a refactoring works

A client registers a project — a local source tree, typically a `jj` or `git` repository — and Henka detects its languages and persists the registration. Source is never copied or moved; Henka operates on the tree in place.

Each operation is one MCP tool. A call names the `project`, a **target** (a file with a position, a selection, or the whole file), and any operation-specific parameters. Edit operations default to a **preview**: they return the diff each file would receive and touch nothing. Pass `dry_run=false` to apply.

When a project spans several working copies, a call may also name a `workspace` (a git worktree or `jj` workspace path, or it's inferred from an absolute `file`). Henka keeps **one warm index per repository**, overlays that working copy's content onto it, computes the refactoring, and writes the result into that working copy — so a dozen worktrees share one index instead of each paying a cold re-analysis.

## Using it with agents (MCP)

Henka exposes a handful of tenancy tools — `register_project`, `unregister_project`, `list_projects`, `project_status`, `list_operations` — plus one tool per operation, routed to the backend for the target file's language. Java offers `rename`, `find-usages`, `change-signature`, `extract-variable`, `extract-constant`, `extract-field`, `extract-method`, `inline`, and `organize-imports`; Rust offers `rename`, `find-usages`, `extract-variable`, `extract-constant`, `extract-function`, and `inline`; TypeScript/JavaScript offers `rename`, `find-usages`, `extract-constant`, and `extract-function`.

Wire it into [Claude Code](https://claude.com/claude-code) over **stdio** (no network, no auth surface):

```sh
claude mcp add henka -- /abs/path/to/target/release/henka
```

…or over **HTTP**. Note the streamable-HTTP transport rejects non-loopback `Host` headers as a DNS-rebinding guard, so a client reaching the server under another name — e.g. from a container as `host.docker.internal` — needs that host allowed:

```sh
henka --transport http --bind 0.0.0.0:8181 --allowed-host host.docker.internal
claude mcp add --transport http henka http://host.docker.internal:8181/mcp
```

Then point the agent at a registered project and ask for a rename, an extract, or a usage search; previews come back as diffs, so the agent can look before it applies.

## Architecture

A Cargo workspace of focused crates:

| Crate | Purpose |
|-------|---------|
| `henka-core` | Language-agnostic core: the project registry, the operation and workspace-edit model, and VCS / repository identity. |
| `henka-lsp` | A minimal LSP client plus the reusable `LspSession` (open/index/overlay/sync) every backend shares. |
| `henka-lang-java` | The Java provider: launches and drives Eclipse JDT LS (`jdtls`) and contributes the Java operations. |
| `henka-lang-rust` | The Rust provider: launches and drives rust-analyzer and contributes the Rust operations. |
| `henka-lang-ts` | The TypeScript/JavaScript provider: launches and drives typescript-language-server (serving both languages) and contributes their operations. |
| `henka-server` | The MCP server binary: the dynamic tool catalog, request dispatch, and the stdio / HTTP transports. |
| `xtask` | Build automation, invoked as `cargo xtask`. |

The Java backend additionally relies on a small OSGi **delegate-command bundle** (`jdtls-bundle/`) compiled against jdtls, which unlocks parameterized refactorings like change-signature.

## Develop

```sh
cargo xtask build          # full build: jdtls (if missing) + bundle + release binary
cargo test                 # unit and mock-backed tests
cargo test -p henka-lang-java -- --ignored   # integration tests that launch a real jdtls
```

`cargo xtask jdtls` and `cargo xtask bundle` run the jdtls fetch and the bundle compile on their own.

## License

Apache-2.0.
