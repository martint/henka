# Code operations workflow

This server applies structured, semantics-aware operations to source trees. It is
multi-tenant: one server hosts many projects, and every operation names the project
it targets.

**Reach for these operations first, not last.** A rename, extract, inline, or
change-signature here updates every reference through the compiler's eyes — across
files and overloads — where a hand-edit or a text search would miss some and break
others. When the task is "rename this symbol", "find where this is used", "extract
this expression / these statements", "inline this", or "change this signature", do
it through Henka rather than editing call sites by hand. And **prefer a semantic
query over text search**: "where is this used?" answered by `find-usages` reflects
the compiler's view — the right symbol, the right overload — not whatever a string
match happens to hit in comments and unrelated code.

Operations come in two kinds:

- **Edits** — refactorings (rename, extract, inline, organize-imports) and structural
  search-and-replace. These change code and support a preview.
- **Queries** — read-only semantic navigation: find-usages, go-to-definition,
  find-implementations, call/type hierarchy, symbol search.

## 1. Find or register the project

- `list_projects` shows what is registered, each with its `id` and `root`. Working
  copies under the server's workspaces mount are auto-registered, so the project
  you want is usually already there — start here, and address it by its `id`.
- If it is not listed, call `register_project` with the source tree's `root` path.
  This is safe to just do: it never copies, moves, or changes source, and it
  persists across restarts. **Do not ask the user for permission to register** — it
  is read-only bookkeeping, not an edit.
- The server resolves paths on **its own** filesystem. When it runs in a container,
  your host path may be mounted under a different prefix; a configured path map
  rewrites it for you (see §3), and a failed registration says where the server
  looked and suggests any mounted working copy of the same name.

## 2. Discover what you can do

Operations are language-scoped and pluggable — do not assume a fixed menu.

- Call `list_operations` with the project `id`. Each entry has a title, its **kind**
  (edit or query), the **target** it expects (a position, a selection, a whole file,
  or the project), and a schema for its parameters.

## 3. Targets and the container boundary

- A **position** target needs `file`, `line`, and `character` (0-based, UTF-16) —
  e.g. the identifier to rename or find usages of.
- A **selection** target needs `file` plus `start_line`/`start_character` and
  `end_line`/`end_character` — e.g. the expression or statements to extract.
- A **file** target needs only `file` — e.g. organize imports.
- A **project** target needs no location — e.g. a workspace-wide symbol search.

Paths are relative to the project root unless absolute.

**A mounted path is the same files — trust it.** When the server runs in a
container, it reads your working copy through a bind mount, so the root it reports
(e.g. `/workspaces/my-project`) and your host path name the **same tree on disk**,
at the same revision. `register_project` tells you when it rewrote your path, and
`project_status` reports the host path it corresponds to under `host_path`. A
rewritten path is not a separate checkout — your `line`/`character` coordinates line
up, so use them directly; no extra verification is needed.

**A different working copy can drift — guard that case.** The one situation where
the server's view can differ from yours is a genuinely separate working copy: a
sibling git worktree / jj workspace sitting on another revision. There, a coordinate
you computed against your copy can land on a different token. Two guards:

- On a position or selection target, pass `expect` — the identifier (or exact
  selected text) you expect there. The server verifies its own copy matches before
  acting, failing loudly instead of silently mis-targeting.
- `project_status` reports the revision and branch the server reads the project at,
  so you can compare it against yours. Pass the matching `workspace` to direct an
  edit at a specific working copy.

## 4. Running an operation

- **Queries** return their structured result directly.
- **Edits** default to a **preview**: calling with no `dry_run` (or `dry_run: true`)
  returns the exact unified diff and **touches no files**. Inspect it, then repeat
  with `dry_run: false` to write the changes to the working tree in place.

## 5. Safety

- An operation that cannot be performed safely (invalid target, name collision,
  ambiguous selection) fails with a clear reason and changes nothing.
- The server applies edits to the working tree; it does not commit. The project's
  own version control governs history — review and commit the changes yourself.
