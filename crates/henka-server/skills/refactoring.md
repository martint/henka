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
match happens to hit in comments and unrelated code. The same goes for locating a
symbol you only know by name: call `symbol-search` instead of grepping the tree and
guessing which hit is the declaration — it resolves the name straight to a file and
range you can hand to `find-usages`, `rename`, or any other position-targeted
operation.

**Query results quote the code they point at.** Every location a query returns carries
`text`, the source line at that coordinate, so you can usually pick the hit you meant —
or confirm a rename covered what you expected — without opening a single file. Pass
`context_lines` only when the surrounding lines genuinely matter; the line itself is
already there.

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

**Prefer paths relative to the project root.** A relative `file` is resolved against
the server's own root, so it works no matter how the server mounts the tree. Reach
for an absolute path only to name a specific working copy (see below).

**Don't judge "same tree?" by the path.** The server resolves paths on its own
filesystem and often runs in its own container, so it may see your checkout under a
different path than you do — it reports `/workspaces/x` while you edit `/root/x`. A
differing root does **not** mean a separate checkout: the two are frequently the same
tree bind-mounted at two paths, so your coordinates still line up. Verify by **state,
not path**:

- Call `project_status`. It reports the `revision`, the `changed_files` set — each
  file with the git blob object id (`oid`) of its current contents — and a combined
  `digest` over that set (uncommitted edits included).
- Compare to your own checkout: `revision` against `jj log -r @` / `git rev-parse HEAD`;
  each `changed_files[].path` against `jj diff --summary` / `git status`; and, for a
  byte-exact check, each `oid` against `git hash-object --no-filters <file>` (git's own
  content id, reproducible even outside a repository). The `digest` folds the whole set
  into one id for an all-or-nothing check.
- Match ⇒ the server sees exactly your tree, edits included; use your `line`/`character`
  coordinates directly. Mismatch ⇒ it is a different working copy; use the guards below.

**When the server's copy differs.** If the state does not match — a sibling worktree /
jj workspace on another revision, or edits the server cannot see — a coordinate computed
against your copy can land on a different token. Two guards:

- On a position or selection target, pass `expect` — the identifier (or exact selected
  text) you expect there. The server checks its own copy matches before acting, failing
  loudly instead of silently mis-targeting.
- Pass an explicit `workspace` (a path the server can reach) to direct the edit at that
  working copy; the server overlays its uncommitted edits and writes the result back
  there. Every edit response echoes the `workspace` and `revision` it acted on, so you
  can confirm afterward.

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
