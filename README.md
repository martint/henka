# Henka

A multi-tenant [MCP](https://modelcontextprotocol.io) server that performs structured,
semantics-aware code refactorings — rename, extract, inline, organize imports, and more — on
behalf of an automated agent. One server hosts many projects; refactorings are contributed per
language, with **Java** supported first.

The full product specification is in [`docs/SPEC.md`](docs/SPEC.md).

## Building

A full build is more than `cargo build`: the Java provider also needs a
[jdtls](https://github.com/eclipse-jdtls/eclipse.jdt.ls) distribution and a small
OSGi delegate-command bundle compiled against it. One command does everything:

```sh
cargo xtask build     # fetch jdtls if missing, build the bundle, then cargo build --release
```

The release binary lands at `target/release/henka-server`. Sub-steps are
available on their own:

```sh
cargo xtask jdtls     # (re)fetch the jdtls distribution into .cache/jdtls
cargo xtask bundle    # recompile the delegate-command bundle
```

## Status

Early development. See `docs/SPEC.md` for the product requirements and capabilities that lead the
build.
