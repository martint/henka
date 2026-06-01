# Deploying Henka

Henka ships as a container image that bundles the language servers it drives
(jdtls, rust-analyzer, typescript-language-server) along with the JRE and Node
they need and `git`/`jj` for worktree/workspace detection. There is nothing to
install on the host beyond a container runtime.

## Quick start with Docker Compose

```sh
cp docker-compose.yml.example docker-compose.yml
cp .env.example .env          # then edit: set HENKA_WORKSPACES_DIR
docker compose up -d
```

`docker-compose.yml` and `.env` are gitignored, so your local edits stay out of
version control.

`HENKA_WORKSPACES_DIR` is a host directory of working copies, mounted read-write
at `/workspaces`. This serves MCP over streamable HTTP at
`http://127.0.0.1:8181/mcp`. Register a working copy as a **project** by its
in-container path (e.g. `/workspaces/my-service`), then call operations against
it. A project is a repository; its git worktrees / jj workspaces can sit as
sibling directories under `/workspaces`, and Henka groups them by repository so
they share one index — the `workspace` argument on an operation then selects
which working copy an edit lands in.

A client running outside the container speaks host paths, which do not exist at
that spelling inside it. Henka bridges this with `HENKA_PATH_MAP`: `host=container`
prefix rewrites it applies to caller-supplied paths (a project root, a
`workspace`, an absolute `file`), so the client can register and target projects
by the paths it knows. The binary knows nothing of the mount layout — the compose
file owns that convention and derives the variable from `HENKA_WORKSPACES_DIR` and
the `/workspaces` mount, appending any extra rewrites you set in `HENKA_PATH_MAP`.
Running the image directly, set it yourself, e.g.
`-e HENKA_PATH_MAP=/home/me/src=/workspaces`.

Configuration knobs (environment variables, all optional):

| Variable | Default | Purpose |
|----------|---------|---------|
| `HENKA_IMAGE` | `ghcr.io/martint/henka:latest` | Image to run. |
| `HENKA_PUBLISH_ADDR` | `127.0.0.1` | Host interface Docker publishes the port on (the container always binds `0.0.0.0`). |
| `HENKA_PUBLISH_PORT` | `8181` | Host port. |
| `HENKA_WORKSPACES_DIR` | _(required)_ | Host directory of working copies, mounted read-write at `/workspaces`. The compose file also uses it to build the path-translation map. |
| `HENKA_PATH_MAP` | _(none)_ | Extra `host=container` prefix rewrites (comma-separated) for additional mounts, appended to the `HENKA_WORKSPACES_DIR`→`/workspaces` rewrite the compose file derives. |
| `HENKA_DATA_DIR` | `henka-data` (named volume) | Where the registry and indexes persist. |
| `HENKA_LOG` | `info` | Log filter (`tracing` env-filter syntax). |

All persistent state lives under `/data` (the image sets `HENKA_DATA=/data`):
the project registry at `/data/projects.toml` and the warm per-repository
indexes under `/data/workspaces`. Mounting `/data` to a host directory or volume
is all it takes to persist everything across restarts.

## Running the image directly

```sh
docker run --rm -p 127.0.0.1:8181:8181 \
  -v henka-data:/data \
  -v "$PWD/workspaces:/workspaces:rw" \
  ghcr.io/martint/henka:latest
```

The default command is `--transport http --bind 0.0.0.0:8181`. Pass extra
configuration through the environment rather than overriding it (see the
Host-header guard below).

## Networking and the Host-header guard

The streamable-HTTP transport rejects `Host` headers outside the loopback set
(`localhost`, `127.0.0.1`, `::1`) as a DNS-rebinding guard. A client that
reaches the server under another name — for instance a containerized MCP client
connecting to the host as `host.docker.internal` — must have that host allowed.
Set `HENKA_MCP_ALLOWED_HOST` (space-separated for several) in `.env` or the
container environment:

```sh
docker run -e HENKA_MCP_ALLOWED_HOST=host.docker.internal ... \
  ghcr.io/martint/henka:latest
```

(The equivalent CLI flag is `--allowed-host`, repeatable; the environment
variable is the same allowlist and is the easier knob in a container.)

## Security

**Henka is unauthenticated.** Anyone who can reach the port can operate on every
registered project and apply edits to the mounted repositories. Keep the
published port on loopback (the default), and if you need it reachable over a
network, put a reverse proxy in front that terminates authentication and
forwards to Henka on loopback. Do not bind it to a public interface directly.

## Building the image locally

```sh
docker build -t henka:dev .
```

The build fetches the language-server distributions, so it needs network access
and takes a few minutes the first time. Pin versions with build args:
`--build-arg JDTLS_VERSION=…`, `--build-arg RUST_ANALYZER_VERSION=…`,
`--build-arg JJ_VERSION=…`.

## Publishing

Pushing a `v*.*.*` tag triggers `.github/workflows/release.yml`, which builds a
multi-arch (amd64 + arm64) image and publishes it to
`ghcr.io/<owner>/henka` with `:<version>`, `:<major>.<minor>`, and `:latest`
tags. The workflow can also be run on demand from the Actions tab (it publishes
a `sha-…` tag).
