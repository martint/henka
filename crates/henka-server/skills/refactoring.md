# Code operations workflow

This server applies structured, semantics-aware operations to registered projects. It is
multi-tenant: one server hosts many projects, and every operation names the project it targets.

Operations come in two kinds:

- **Edits** — refactorings (rename, extract, inline, organize-imports) and structural
  search-and-replace. These change code and support a preview.
- **Queries** — read-only semantic navigation: find-usages, go-to-definition,
  find-implementations, call/type hierarchy, symbol search.

**Prefer a semantic query over text search.** "Where is this used?" answered by `find-usages`
reflects the compiler's view — the right symbol, the right overload — not whatever a string match
happens to hit in comments and unrelated code.

## 1. Register the project

- Call `register_project` with the absolute `root` path of the source tree (optionally an `id`).
- The server detects the languages present and returns the project's `id` and `languages`.
- `list_projects` shows what is already registered; registration persists across restarts.

## 2. Discover what you can do

Operations are language-scoped and pluggable — do not assume a fixed menu.

- Call `list_operations` with the project `id`. Each entry has a title, its **kind** (edit or
  query), the **target** it expects (a position, a selection, a whole file, or the project), and a
  schema for its parameters.

## 3. Targets

- A **position** target needs `file`, `line`, and `character` (0-based, UTF-16) — e.g. the
  identifier to rename or find usages of.
- A **selection** target needs `file` plus `start_line`/`start_character` and
  `end_line`/`end_character` — e.g. the expression or statements to extract.
- A **file** target needs only `file` — e.g. organize imports.
- A **project** target needs no location — e.g. a workspace-wide symbol search.

Paths are relative to the project root unless absolute.

**Guard your coordinates.** A `line`/`character` you computed against your own copy of a file can
land on a different token in the server's copy if its checkout is on another revision. On a
position or selection target, pass `expect` — the identifier (or exact selected text) you expect
there — and the server verifies its own copy matches before acting, failing loudly instead of
silently mis-targeting. If a guard ever fails, `project_status` reports the revision and branch the
server reads the project at, so you can tell whether its checkout has drifted from yours.

## 4. Running an operation

- **Queries** return their structured result directly.
- **Edits** default to a **preview**: calling with no `dry_run` (or `dry_run: true`) returns the
  exact unified diff and **touches no files**. Inspect it, then repeat with `dry_run: false` to
  write the changes to the working tree in place.

## 5. Safety

- An operation that cannot be performed safely (invalid target, name collision, ambiguous
  selection) fails with a clear reason and changes nothing.
- The server applies edits to the working tree; it does not commit. The project's own version
  control governs history — review and commit the changes yourself.
