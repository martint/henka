# Refactor — Product Specification

## 1. What Refactor is

Refactor is a server that performs **structured, semantics-aware code refactorings** on behalf
of an automated agent. It speaks the Model Context Protocol (MCP), so any MCP-capable client —
an AI coding assistant, an editor, a script — can ask it to rename a symbol, extract a method,
inline a variable, and so on, and get back a precise set of edits.

It is **multi-tenant**: one running server hosts many independent projects at once and keeps each
project's analysis separate. It is **language-extensible**: refactorings are contributed per
language, with **Java** supported first. And it is **safe by construction**: every refactoring can
be previewed as a diff before anything touches disk.

Refactor does not replace version control, a build system, or an editor. It is the component that
answers one question well: *"apply this refactoring to this code, correctly, across every file it
affects."*

## 2. The vocabulary

### 2.1 Server

A single long-running process that hosts every project and exposes the MCP surface (§3). One
server can front any number of projects; clients address a specific one by id on each call.

### 2.2 Project (tenant)

A *project* is the unit of "a codebase I want to refactor." It pins:

- an **id** — a short slug the client uses to address it;
- a **root** — a local filesystem path to an existing source tree (typically a `jj` or `git`
  repository);
- one or more **languages**, detected from the source tree.

Projects are registered explicitly and persist across server restarts. Registering a project does
not copy or move its source; the server operates on the tree in place.

### 2.3 Language provider

A *language provider* supplies the semantic understanding for one language and contributes that
language's refactorings to the catalog. Java is the first provider. Adding a language is adding a
provider; it does not change the protocol or the other languages.

### 2.4 Refactoring

A *refactoring* is a single named transformation — `rename`, `extract-method`,
`extract-variable`, `extract-constant`, `inline`, `organize-imports`, and more over time. Each
refactoring is a self-contained unit that declares:

- the **languages** it applies to,
- the **target** it operates on (§2.5),
- its **parameters** (e.g. the new name for a `rename`).

The catalog is open-ended: a refactoring exists for a language only because that language's
provider offers it. `organize-imports` is meaningful for Java and is offered there; a language for
which it makes no sense simply never lists it.

### 2.5 Target

The *target* tells a refactoring where to act. Depending on the refactoring it is one of:

- a **position** — a file plus a line/column (e.g. the identifier to rename);
- a **selection** — a file plus a range (e.g. the expression to extract);
- a **file** — the whole file (e.g. organize its imports).

### 2.6 Workspace edit

The result of a refactoring is a *workspace edit*: an ordered set of text changes across one or
more files. It is the single currency every refactoring returns, regardless of language, so
previews and applies behave identically everywhere.

### 2.7 Preview (dry run)

Any refactoring can be run as a *preview*. A preview computes the full workspace edit and returns
it as a unified diff **without modifying any file**. Applying the same refactoring without preview
writes the edit to the working tree in place.

### 2.8 Index

To refactor correctly, a provider builds a semantic *index* of the project (types, symbols,
references). The index is expensive to build and is kept warm between calls. It is **VCS-aware**
(§9): the server tracks the project's current revision so that switching branches reuses a warm
index and ordinary edits update it incrementally rather than rebuilding it.

## 3. Application surface

The server is reached entirely through MCP. There are two kinds of surface: **tools** (actions)
and a **skill resource** (guidance).

### 3.1 Tools

- **Tenancy** — `register_project`, `unregister_project`, `list_projects`, `project_status`.
- **Discovery** — `list_refactorings`, which reports the refactorings available for a project,
  each with its parameters and target kind.
- **Refactorings** — one tool per refactoring in the catalog (e.g. `rename`, `extract-method`).
  Every refactoring tool takes the project, the target, the refactoring's parameters, and a
  `dry_run` flag. The set of refactoring tools reflects the live catalog: a project's language
  determines which ones are usable.

### 3.2 The skill resource

The server publishes a **skill** as an MCP resource at `skill://refactor/refactoring`. It is a
written workflow that teaches an agent how to use the server: register a project, discover which
refactorings apply, preview with `dry_run`, then apply. Clients are pointed to it from the
server's own instructions, and may read it before doing refactoring work.

## 4. Managing projects

A client registers a project by giving a root path and (optionally) an id; the server detects the
languages present and confirms the registration. `list_projects` enumerates what is registered;
`project_status` reports a project's languages and whether its semantic backend is ready, warming,
or unavailable. `unregister_project` forgets a project; it never deletes source.

## 5. Discovering refactorings

`list_refactorings` is the contract between client and catalog. For a given project it returns
every applicable refactoring with a human title, the target kind it expects, and a schema for its
parameters. A client should consult it rather than assume a fixed menu, because the catalog grows
and varies by language.

## 6. Performing a refactoring

A refactoring call names the project, the target, and the parameters. The server resolves the
project, ensures its index is ready, computes the workspace edit, and — unless previewing — writes
it to the working tree. The response always includes a summary of what changed (the files touched)
and the diff.

## 7. Preview vs apply

Preview is the default posture for anything uncertain. `dry_run: true` guarantees **no file is
modified** and returns the diff the apply *would* produce. The same call with `dry_run: false`
applies it. There is no separate "commit" step inside the server — applying writes directly to the
working tree, and the project's own version control governs history.

## 8. The refactoring catalog (initial, Java)

The first catalog targets the refactorings developers reach for most, modeled on IntelliJ's:

- **rename** — rename a symbol and update every reference across the project.
- **extract-variable / extract-constant / extract-field** — lift a selected expression into a new
  local, constant, or field.
- **extract-method** — turn a selected statement range into a new method, capturing parameters and
  return value.
- **inline** — replace a variable or method with its definition.
- **organize-imports** — sort and prune a file's imports.

Heavier refactorings (**change-signature**, **move**) follow once the catalog above is solid.

## 9. Indexing and VCS-awareness

Refactoring quality depends on an accurate, current index, and rebuilding it is the dominant cost.
The server therefore tracks each project's version-control state:

- It identifies the project's **current revision** (a `jj` change or a `git` commit/branch).
- It keeps a **warm index per revision**, so switching to a branch seen before reactivates that
  index instead of rebuilding from scratch.
- It turns an ordinary working-copy or branch change into an **incremental update** of the index —
  only the changed files are reanalyzed.

This is a performance capability, not a behavioral one: results are identical with or without it,
but a warm, incrementally-maintained index makes refactorings on large projects fast.

## 10. Languages and extensibility

Java is first; the design assumes more. A new language arrives as a provider that contributes its
own semantic understanding and its own refactorings. Existing languages, the protocol, and the
client experience are unaffected. Two languages may both offer `rename` while differing entirely
in what else they offer.

## 11. Errors and safety

- A refactoring that cannot be performed safely (an invalid target, a name collision, an
  ambiguous selection) **fails with a clear reason and changes nothing**.
- Preview never writes; apply writes the complete edit or, on failure, leaves the tree untouched
  as far as possible.
- Requests against an unknown project, an unsupported refactoring, or a not-yet-ready index return
  explicit, distinguishable errors rather than silent no-ops.

## 12. Configuration & operation

- The server runs over **stdio** for local/single-client use and over **streamable HTTP** as a
  hosted, multi-client service. The same projects and catalog are available on either.
- Project registrations persist, so a restarted server restores its tenants.
- **Java support requires** a Java runtime (JDK 25+) and a Java semantic engine available to the
  server; when either is missing, affected projects report their backend as unavailable with a
  clear message instead of failing opaquely.

## 13. Design principles

- **Preview before harm.** Every refactoring can be seen as a diff before it touches disk. The
  default answer to "what will this do?" is the exact edit, not a description of it.
- **Refactorings are plugins, not protocol.** The catalog is open and language-scoped. Adding a
  refactoring or a language never changes the surface other languages present.
- **Correct across the whole project, or not at all.** A refactoring updates every affected file
  or fails cleanly. It never does half the job.
- **The index serves speed, never correctness.** Caching and incremental updates change how fast
  an answer comes, never what the answer is.
- **The server refactors; it does not version.** Applying edits to the working tree is the
  server's job; recording history is the project's version control's job.
