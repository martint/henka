# Cross-language dispatch — Spec

Change: `cross-language-dispatch` (the commit introducing `LanguageRoute`, ahead of
the navigation queries in spec 0002)

## 1. What this change is

SPEC.md §2.4 describes the catalog as language-scoped: an operation exists for a
language because that language's provider offers it. That is still true, but it hides
a question the catalog cannot answer on its own. Several providers register their own
operation under the *same* id — `find-usages` is offered by Java, Rust and
TypeScript — and a project can have more than one of those languages at once. An id
plus a project therefore names a *set* of operations, and something has to pick the
one that runs.

Picking wrongly is not a degraded answer, it is a broken one. Each provider's
operation reaches its backend by downcasting the session it is handed, so the Java
`find-usages` paired with a TypeScript session fails inside the operation rather than
at the boundary. And project-targeted queries (SPEC.md §2.5) name no file at all, so
there is nothing in the request to pick by.

This change states the rules: how a request is routed to the language that serves it,
what happens when one backend serves several languages, how the answers of several
backends combine, and what a request that cannot be placed does instead of running.

## 2. Routing precedence

For each call, the project's languages that register an operation under the requested
id are its **candidates**, in the order the project reports them. The language that
serves the call is the first of these that applies:

1. **The target's file.** A position, selection, or file target names a file, and the
   file's language owns the request. This is the common case and it is decided before
   anything else.
2. **The operation's own route.** An operation may read where a request belongs out of
   its parameters. A call-hierarchy item carries the URI of the file it was resolved
   in, and only the server that issued the item can expand it, so the item names the
   language even though the target does not.
3. **Every candidate, for a project-scoped query.** A project-targeted *query* names no
   location, so in a mixed-language project each language's server can hold part of the
   answer. `symbol-search` runs on all of them and the results are merged (§4).
4. **The first candidate**, for anything else — a project-targeted edit, where one
   language has to own the change.

A language named by rule 1 or 2 is resolved to the candidate whose **provider** serves
it, which need not be a language the project was detected to have: one TypeScript
server answers for JavaScript too, so a JavaScript file in a project detected as Java
and TypeScript is routed to the TypeScript provider rather than falling through to the
fan-out. A named language no registered provider serves is an error (§5), not a
broadening.

Routing decides the language *before* the operation is resolved, so the operation and
the session it is handed always come from the same provider.

## 3. One backend, several languages

A provider may be registered for several languages, and its session is shared across
them. A project-scoped query (rule 3) therefore asks each distinct **provider** once,
not each language: a TypeScript project holding a JavaScript config file has both
languages, but issuing the query twice to the one server that serves them would return
its matches twice and double every count in the merged result. Deduplication is by
provider identity, matching how the catalog already asks a multi-language provider for
its operations only once.

## 4. Merging several answers

The results of a project-scoped query that several backends answered are merged
structurally, because the shape belongs to the operation, not to the dispatcher. A
query result is a list of findings alongside counters and flags that describe it, so:
objects merge key by key, lists concatenate, counts add, flags or together, and any
other value keeps the first answer.

A caller's `limit` (spec 0001 §3) bounds the **merged** list, not each backend's share
of it. Every backend applies the cap to its own answer, so their concatenation can
exceed it; the merged lists are cut back to the cap the caller asked for — or, when it
asked for none, to the default the operation declares — while `count` keeps reporting
everything that matched, and `truncated` is set when the merge was shortened. This is
the same contract a single backend already honours; a second language must not be able
to return more than the maximum the tool advertises.

## 5. Requests that cannot be placed

A request that says where it belongs and cannot be sent there **fails before anything
runs**, per SPEC.md §11:

- A target file or a route naming a language no candidate's provider serves is a
  validation error naming the operation and the language.
- An opaque handle — a call-hierarchy item whose URI names no language Henka can
  place — is a validation error naming the handle.

Neither case falls back to the project-wide fan-out. A handle offered to servers that
never issued it is not a wider search: the backends that cannot read it answer with
errors, and one of those errors is enough to discard the answer the right backend gave
and break the hierarchy walk the caller is in the middle of. Refusing names the problem
where it happened.

A URI is placed by its **path**, which is not always a file path: a server issues its
own handles for sources outside the working copy — jdtls answers with
`jdt://contents/java.base/java.io/PrintStream.java?…` for a class it holds as
bytecode — keeping the source's name in the path and its own metadata in the query
string. Reading the path alone, query string and fragment stripped, keeps those handles
with the server that issued them.

## 6. Concurrency

Requests against one session are serialized by a guard the session issues, and the
guard is held for the whole request: the operation, the application of any edit it
produced, and the index synchronization that follows. Releasing it at the end of the
operation would let a second request compute edits against coordinates the first is
still changing, and let one request's synchronization close documents another's overlay
is using.

## 7. What does not change

The protocol, the target model, and the catalog are untouched: a client still names a
project, a target, and parameters, and still sees one tool per operation id. Routing is
entirely server-side — the caller never names a language — and a single-language
project behaves exactly as before, since it has one candidate and every rule above
resolves to it.
