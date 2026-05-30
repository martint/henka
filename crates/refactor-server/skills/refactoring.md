# Refactoring workflow

This server applies structured, semantics-aware refactorings to registered projects. It is
multi-tenant: one server hosts many projects, and every tool call names the project it targets.

## 1. Register the project

Before refactoring, the project's source tree must be registered.

- Call `register_project` with the absolute `root` path of the source tree (optionally an `id`).
- The server detects the languages present and returns the project's `id` and `languages`.
- Use `list_projects` to see what is already registered; registration persists across restarts,
  so a project you registered earlier is likely still there.

## 2. Discover what you can do

Refactorings are language-scoped and pluggable — do not assume a fixed menu.

- Call `list_refactorings` with the project `id` to get the refactorings available for it. Each
  entry has a title, the kind of **target** it expects (a position, a selection, or a whole
  file), and a schema for its parameters.

## 3. Preview before applying

Every refactoring accepts a `dry_run` flag.

- Call the refactoring with `dry_run: true` first. The server returns the exact unified diff it
  *would* apply, and **touches no files**.
- Inspect the diff. If it is what you intended, repeat the call with `dry_run: false` to write the
  changes to the working tree in place.

## 4. Targets

- A **position** target needs `file`, `line`, and `character` (0-based) — e.g. the identifier to
  rename.
- A **selection** target needs `file` plus a start and end position — e.g. the expression or
  statements to extract.
- A **file** target needs only `file` — e.g. organize imports.

Paths are relative to the project root unless absolute.

## 5. Safety

- A refactoring that cannot be performed safely (invalid target, name collision, ambiguous
  selection) fails with a clear reason and changes nothing.
- The server applies edits to the working tree; it does not commit. The project's own version
  control governs history — review and commit the changes yourself.
