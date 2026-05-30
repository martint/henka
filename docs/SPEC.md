# Henka — Product Specification

## 1. What Henka is

Henka is a server that performs **structured, semantics-aware operations on code** on behalf
of an automated agent. It speaks the Model Context Protocol (MCP), so any MCP-capable client — an
AI coding assistant, an editor, a script — can ask it to do two kinds of thing:

- **Edit** the code through refactorings — rename a symbol, extract a method, inline a variable,
  organize imports — and through **structural search-and-replace**, getting back a precise set of
  edits.
- **Query** the code semantically — find every usage of a symbol, jump to a definition, list
  implementations of an interface, walk a call or type hierarchy — getting back precise,
  structured answers.

The query side matters as much as the edit side: it lets an agent work from the program's actual
semantics instead of resorting to brute-force text search. "Where is this method used?" is
answered by the compiler's view of the code, not by grepping a string that also matches comments,
unrelated identifiers, and the wrong overload.

Henka is **multi-tenant**: one running server hosts many independent projects at once and keeps
each project's analysis separate. It is **language-extensible**: operations are contributed per
language, with **Java** supported first. And it is **safe by construction**: every edit can be
previewed as a diff before anything touches disk.

Henka does not replace version control, a build system, or an editor. It is the component that
answers one question well: *"operate on this code, correctly, using its real semantics."*

## 2. The vocabulary

### 2.1 Server

A single long-running process that hosts every project and exposes the MCP surface (§3). One
server can front any number of projects; clients address a specific one by id on each call.

### 2.2 Project (tenant)

A *project* is the unit of "a codebase I want to work on." It pins:

- an **id** — a short slug the client uses to address it;
- a **root** — a local filesystem path to an existing source tree (typically a `jj` or `git`
  repository);
- one or more **languages**, detected from the source tree.

Projects are registered explicitly and persist across server restarts. Registering a project does
not copy or move its source; the server operates on the tree in place.

### 2.3 Language provider

A *language provider* supplies the semantic understanding for one language and contributes that
language's operations to the catalog. Java is the first provider. Adding a language is adding a
provider; it does not change the protocol or the other languages.

### 2.4 Operation

An *operation* is a single named action on code. Every operation declares the **languages** it
applies to, the **target** it acts on (§2.5), and its **parameters**. Operations come in two
kinds by their outcome:

- An **edit operation** produces a *workspace edit* (§2.6) and supports preview (§2.7). The
  **refactorings** (`rename`, `extract-method`, `inline`, `organize-imports`, …) and **structural
  search-and-replace** are edit operations.
- A **query operation** produces a *structured result* and never changes code. **Find-usages**,
  **go-to-definition**, **find-implementations**, **call hierarchy**, **type hierarchy**, and
  **symbol search** are query operations.

The catalog is open-ended and language-scoped: an operation exists for a language only because
that language's provider offers it. `organize-imports` is meaningful for Java and is offered
there; a language for which it makes no sense simply never lists it.

### 2.5 Target

The *target* tells an operation where to act. Depending on the operation it is one of:

- a **position** — a file plus a line/column (e.g. the identifier to rename, or to find usages
  of);
- a **selection** — a file plus a range (e.g. the expression to extract);
- a **file** — a whole file (e.g. organize its imports);
- the **project** — no specific location (e.g. a workspace-wide symbol search or a structural
  search across the tree).

### 2.6 Workspace edit

The outcome of an edit operation is a *workspace edit*: an ordered set of text changes across one
or more files. It is the single currency every edit operation returns, regardless of language, so
previews and applies behave identically everywhere.

### 2.7 Preview (dry run)

Any edit operation can be run as a *preview*. A preview computes the full workspace edit and
returns it as a unified diff **without modifying any file**. Applying the same operation without
preview writes the edit to the working tree in place. Query operations are always read-only, so
preview does not apply to them.

### 2.8 Index

To answer correctly, a provider builds a semantic *index* of the project (types, symbols,
references). The index is expensive to build and is kept warm between calls. It is **VCS-aware**
(§9): the server tracks the project's current revision so that switching branches reuses a warm
index and ordinary edits update it incrementally rather than rebuilding it.

## 3. Application surface

The server is reached entirely through MCP. There are two kinds of surface: **tools** (actions)
and a **skill resource** (guidance).

### 3.1 Tools

- **Tenancy** — `register_project`, `unregister_project`, `list_projects`, `project_status`.
- **Discovery** — `list_operations`, which reports the operations available for a project, each
  with its kind (edit or query), the target kind it expects, and a schema for its parameters.
- **Operations** — one tool per operation in the catalog (e.g. `rename`, `extract-method`,
  `find-usages`, `structural-replace`). Every operation tool takes the project, the target, and
  the operation's parameters; edit operations additionally take a `dry_run` flag. The set of
  operation tools reflects the live catalog.

### 3.2 The skill resource

The server publishes a **skill** as an MCP resource at `skill://henka/refactoring`. It is a
written workflow that teaches an agent how to use the server: register a project, discover which
operations apply, prefer a semantic query over a text search, and preview edits with `dry_run`
before applying. Clients are pointed to it from the server's own instructions.

## 4. Managing projects

A client registers a project by giving a root path and (optionally) an id; the server detects the
languages present and confirms the registration. `list_projects` enumerates what is registered;
`project_status` reports a project's languages and whether its semantic backend is ready, warming,
or unavailable. `unregister_project` forgets a project; it never deletes source.

## 5. Discovering operations

`list_operations` is the contract between client and catalog. For a given project it returns every
applicable operation with a human title, its kind (edit or query), the target kind it expects, and
a schema for its parameters. A client should consult it rather than assume a fixed menu, because
the catalog grows and varies by language.

## 6. Performing an operation

An operation call names the project, the target, and the parameters. The server resolves the
project, ensures its index is ready, and runs the operation. A query operation returns its
structured result. An edit operation computes the workspace edit and — unless previewing — writes
it to the working tree; the response includes a summary of what changed and the diff.

## 7. Preview vs apply

Preview is the default posture for any uncertain edit. `dry_run: true` guarantees **no file is
modified** and returns the diff the apply *would* produce. The same call with `dry_run: false`
applies it. There is no separate "commit" step inside the server — applying writes directly to the
working tree, and the project's own version control governs history.

## 8. The operation catalog (initial, Java)

Modeled on the actions a developer reaches for in IntelliJ.

**Refactorings (edit):**

- **rename** — rename a symbol and update every reference across the project.
- **extract-variable / extract-constant / extract-field** — lift a selected expression into a new
  local, constant, or field.
- **extract-method** — turn a selected statement range into a new method, capturing parameters and
  return value.
- **inline** — replace a variable or method with its definition.
- **organize-imports** — sort and prune a file's imports.
- **change-signature** — rename a method, change its return type or visibility, and reorder, add,
  remove, or retype its parameters, updating every call site.

Workspace edits may include **file operations** (create/rename/delete) in addition to text
changes, so a refactoring that also moves or renames a file is applied as one unit.

**Move** (a class to another package, a member to another type) is recognized but not yet offered
as an operation: its destinations resolve only against a fully imported build project, so it
awaits that path rather than shipping partial results.

**Structural search-and-replace (edit):** match code by syntactic/semantic *shape* — not raw
text — across the project, optionally rewriting each match to a new shape. Comments and string
literals that merely contain the same characters do not match.

**Semantic queries (read):**

- **find-usages** — every reference to a symbol, with location and context.
- **go-to-definition** / **find-implementations** — resolve a symbol to where it is defined or
  concretely implemented.
- **call-hierarchy** — incoming/outgoing callers of a method.
- **type-hierarchy** — supertypes and subtypes of a type.
- **symbol-search** — find symbols by name or pattern across the project.

These let an agent navigate by the program's real structure instead of by text matching.

## 9. Indexing and VCS-awareness

Answer quality depends on an accurate, current index, and rebuilding it is the dominant cost. The
server therefore tracks each project's version-control state:

- It identifies the project's **current revision** (a `jj` change or a `git` commit/branch).
- It keeps a **warm index per revision**, so switching to a branch seen before reactivates that
  index instead of rebuilding from scratch.
- It turns an ordinary working-copy or branch change into an **incremental update** of the index —
  only the changed files are reanalyzed.

This is a performance capability, not a behavioral one: results are identical with or without it,
but a warm, incrementally-maintained index makes operations on large projects fast.

## 10. Languages and extensibility

Java is first; the design assumes more. A new language arrives as a provider that contributes its
own semantic understanding and its own operations. Existing languages, the protocol, and the
client experience are unaffected. Two languages may both offer `rename` while differing entirely
in what else they offer.

## 11. Errors and safety

- An operation that cannot be performed safely (an invalid target, a name collision, an ambiguous
  selection) **fails with a clear reason and changes nothing**.
- Preview never writes; apply writes the complete edit or, on failure, leaves the tree untouched
  as far as possible.
- Requests against an unknown project, an unsupported operation, or a not-yet-ready index return
  explicit, distinguishable errors rather than silent no-ops.

## 12. Configuration & operation

- The server runs over **stdio** for local/single-client use and over **streamable HTTP** as a
  hosted, multi-client service. The same projects and catalog are available on either.
- Project registrations persist, so a restarted server restores its tenants.
- **Java support requires** a Java runtime (JDK 25+) and a Java semantic engine available to the
  server; when either is missing, affected projects report their backend as unavailable with a
  clear message instead of failing opaquely.

## 13. Design principles

- **Semantics over text.** The server exists so an agent can act on what the code *means* — its
  symbols, types, and references — instead of guessing from text. A semantic query is always
  preferable to a grep.
- **Preview before harm.** Every edit can be seen as a diff before it touches disk. The default
  answer to "what will this do?" is the exact edit, not a description of it.
- **Operations are plugins, not protocol.** The catalog is open and language-scoped. Adding an
  operation or a language never changes the surface other languages present.
- **Correct across the whole project, or not at all.** An edit updates every affected file or
  fails cleanly. It never does half the job.
- **The index serves speed, never correctness.** Caching and incremental updates change how fast
  an answer comes, never what the answer is.
- **The server operates; it does not version.** Applying edits to the working tree is the server's
  job; recording history is the project's version control's job.
