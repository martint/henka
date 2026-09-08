# Navigation and hierarchy queries — Spec

Change: `navigation-queries` (a sequence of commits, one per operation)

## 1. What this change is

Henka's edit side is broad — rename, the extracts, inline, organize-imports,
change-signature, move — while its query side is two operations wide: `find-usages`
(`textDocument/references`) and `symbol-search` (`workspace/symbol`). The catalog in
SPEC.md §8 already *promises* more: `go-to-definition`, `find-implementations`, and
`call-hierarchy` are listed as semantic queries but nothing implements them.

That gap costs an agent more than it looks. Every navigation question a query cannot
answer falls back to reading files: "what type is this?" becomes reading the
declaration and its imports, "who calls this?" becomes grepping a method name and
filtering the hits by hand, "what does this file contain?" becomes reading the file
in full to build an outline the language server already has. Each fallback is slower,
burns context, and — per SPEC.md §13, "Semantics over text" — answers from text what
should be answered from the compiler's view.

This change fills in the promised navigation queries and adds two more the language
servers already answer: a symbol's type/documentation summary, and a file's symbol
outline. Every one of them is a **query operation** (SPEC.md §2.4): read-only,
returning a structured result, never a workspace edit, never a preview.

It also changes what a navigation result *contains*. Measuring LSP-backed navigation
against `grep` for Claude Code found that a tool returning only a coordinate — `file`,
`line`, `character` — leaves the agent no better off than before: it has to open the
file to see what is there, while `grep` had already shown it
(`src/auth.ts:42: return validateToken(token)`). Adding source text to the response,
with the semantic backend and the matched set unchanged, moved pass@1 on rename tasks
from 0.67 to 0.83 and cut follow-up file reads from 15.2 to 3.2 per episode. Every
location a query in this change returns therefore carries the source it points at
(§3.1), and the two queries that already shipped — `find-usages` and `symbol-search`,
whose spec 0001 §3.1 now requires it too — are retrofitted to the same shape in the
first commit of this sequence, so the catalog never presents two conventions at once.

## 2. The operations, and what they are called

The requested set is nine LSP requests, two of which Henka already exposes. LSP names
requests after the editor gesture that triggers them (`hover`, `documentSymbol`);
Henka names operations after the caller's intent, which is why
`textDocument/references` is already `find-usages` rather than `find-references`. The
new ids follow the names SPEC.md §8 already fixed where it fixed them, and stay
intent-shaped where it did not:

| LSP request | Operation id | Status |
|---|---|---|
| `textDocument/definition` | `go-to-definition` | new — named by SPEC.md §8 |
| `textDocument/references` | `find-usages` | already implemented |
| `textDocument/implementation` | `find-implementations` | new — named by SPEC.md §8 |
| `textDocument/hover` | `describe-symbol` | new |
| `textDocument/documentSymbol` | `file-outline` | new |
| `workspace/symbol` | `symbol-search` | already implemented (spec 0001) |
| `textDocument/prepareCallHierarchy` | `prepare-call-hierarchy` | new |
| `callHierarchy/incomingCalls` | `incoming-calls` | new |
| `callHierarchy/outgoingCalls` | `outgoing-calls` | new |

Each new operation is offered by every language provider that already ships an
LSP-backed session — **Java**, **Rust**, and **TypeScript/JavaScript** — because each
request is standard LSP and each backing server (jdtls, rust-analyzer,
typescript-language-server) implements it. As with `symbol-search`, the `run` method is
identical in shape across the three providers: ensure the index is warm, open the
target file, issue the request, normalize the response.

Every new operation lands as its own commit, so each is independently reviewable and
revertable and no single change adds seven entries to the catalog at once.

### 2.1 `go-to-definition`

Target: a **position** (SPEC.md §2.5). No parameters. Resolves the symbol under the
position to where it is declared.

Each definition carries the source it points at (§3.1) — the declaring line, not just
its coordinate — so "where is this defined?" is usually answered without a follow-up
read of the file.

The result is a list, not a single location, even though the common case has one
element. A definition can legitimately be multiple places — a partial or overloaded
declaration, a `declare`d ambient type alongside its implementation — and a caller that
has to handle "sometimes an object, sometimes an array" is a caller that will handle it
wrong. One shape, always: `{ "count": <n>, "definitions": [...] }`.

### 2.2 `find-implementations`

Target: a **position**. No parameters. From an interface, abstract method, or trait
member, resolves the concrete implementations. Result:
`{ "count": <n>, "implementations": [...] }`.

Each implementation carries its declaring line (§3.1), which is what makes a list of
eight overrides readable: the signature beside each path is how a caller tells the one
it means from the seven it does not.

`find-implementations` is *not* a special case of `find-usages`: a reference to
`List.add` and an override of `List.add` are different questions, and only the second
one tells an agent where the behavior it is about to change actually lives.

### 2.3 `describe-symbol`

Target: a **position**. No parameters. Returns the language server's summary of the
symbol: its resolved type or signature, and its documentation where the server has it.

This is LSP's `hover`, renamed because "hover" describes a mouse gesture that has no
meaning to a caller with no cursor. What the operation is *for* is answering "what is
this thing?" without reading the declaration — the fully-resolved generic type of a
local, the selected overload's signature, the doc comment on a third-party method whose
source may not even be in the tree.

The response is `{ "text": <string>, ... }` plus the hovered range when the server
reports one. LSP allows three content shapes here (`MarkupContent`, a `MarkedString`, or
an array of `MarkedString`s, the last two either bare strings or `{language, value}`
pairs, and all three still in use across servers). All of them collapse to one markdown
string, with fenced code blocks preserved for the `{language, value}` form, so the
caller never branches on which shape its language server happens to use. A server that
has nothing to say answers `null`, which is `{ "text": "" }` — an empty answer, not an
error.

### 2.4 `file-outline`

Target: a **file** (SPEC.md §2.5) — the only new operation not anchored to a position.
No parameters. Lists the symbols declared in that file.

It answers "what is in this file?" in one structured response instead of a full read,
which is the difference between spending a few hundred tokens on a file's shape and
spending its entire length. It also produces the coordinates the position-targeted
operations need, the same service `symbol-search` performs project-wide: outline a file,
pick the member, rename it.

Each symbol carries its declaration line (§3.1). For an outline this is the only
sane choice: a class's range spans the entire file, so returning the range's text would
return the file — the very read the operation exists to avoid — while the declaration
line gives the signature, the modifiers, and the extends/implements clause in one line.

Symbols are returned **nested**, each with its `children`, because that is the structure
of the answer — a class contains its methods and fields — and flattening it throws away
the containment an agent is asking about. LSP has two response shapes here:
`DocumentSymbol[]` (hierarchical, with `detail` and `children`) and the older, flat
`SymbolInformation[]` (with `containerName`). Both are accepted; a flat response is
reported with each symbol's `children` empty rather than being reassembled into a tree
by guessing at names.

### 2.5 Call hierarchy: `prepare-call-hierarchy`, `incoming-calls`, `outgoing-calls`

SPEC.md §8 lists **call-hierarchy** as one catalog entry ("incoming/outgoing callers of
a method"). It ships as three operations, following LSP's own two-phase protocol, and the
split is deliberate rather than incidental:

- **Resolution happens once.** `prepare-call-hierarchy` turns a position into one or more
  **call hierarchy items** — the specific declarations that position resolves to. Walking
  a hierarchy means asking about the same item repeatedly, in both directions and at each
  level; re-resolving a coordinate on every step would re-do the expensive part and,
  worse, would need a fresh position for each node the caller has not read yet.
- **The item is the identity.** A position is an ambiguous handle to a method — the same
  line resolves differently across overloads — while an item is the language server's own
  unambiguous reference to one declaration. Both directions take the item, so a caller
  that resolved `foo(int)` keeps asking about `foo(int)`.

`prepare-call-hierarchy` takes a **position** and returns
`{ "count": <n>, "items": [...] }`. Each item carries the normalized fields a caller
reads — `name`, `kind`, `detail`, `file`, and both its full range and its selection
range — plus an opaque `item` value.

That `item` is the language server's raw `CallHierarchyItem`, passed through untouched,
and it exists because a normalized item is not a valid handle. Servers attach a private
`data` field to an item and require it back verbatim on the follow-up call; jdtls does
exactly this. Henka therefore hands the caller both: readable fields to decide *which*
item it wants, and the server's own token to name it again. The caller treats `item` as
opaque and passes it back unmodified.

`incoming-calls` and `outgoing-calls` target the **project** (SPEC.md §2.5) — the item
already carries the location, so no separate coordinate is meaningful — and each takes a
single required `item` parameter, the value from a `prepare-call-hierarchy` result.
`incoming-calls` answers "who calls this?"; `outgoing-calls` answers "what does this
call?".

Both return `{ "count": <n>, "calls": [...] }`. Each entry pairs the other end of the
call — `from` for an incoming call, `to` for an outgoing one, each an item in the same
normalized shape `prepare-call-hierarchy` returns, opaque `item` included so the caller
can walk one more level — with `ranges`, the call sites themselves.

Source text (§3.1) appears in both places, and the two are answering different
questions. On an item it is the **declaration line**, which says what the caller or
callee *is*. On each entry in `ranges` it is the **call-site line**, which says how it is
being called — `foo(a, b)` versus `foo(a, b, c)` is the difference between a hierarchy
walk that ends here and one that has to keep going. A caller walking a hierarchy in the
old shape would read every file it touched; with both lines present, it reads the ones it
intends to change. The direction word
is kept as the field name rather than normalized to a neutral `item`, because "the
caller" and "the callee" are not interchangeable and a caller reading a saved result
should not have to remember which request produced it.

## 3. Result normalization

Everything above is normalized the way `find-usages` and `symbol-search` already
normalize their results (SPEC.md §9): a caller reads one shape regardless of which
language server answered, never sees a raw LSP type, never sees a `file://` URI, and
gets paths relative to the project root where possible. Ranges are flattened into
`start_line` / `start_character` / `end_line` / `end_character` rather than nested
`{start: {line, character}}` objects, matching `find-usages`. Symbol kinds are lowercase
names (`"class"`, `"method"`, …), not raw LSP integers, matching `symbol-search`.

The conversions live in `henka-lsp`'s `convert` module beside `locations_to_query` and
`symbols_to_query`, so all three providers share one implementation.

### 3.1 Every location carries its source text

Spec 0001 §3.1 states the rule and the evidence behind it; it applies unchanged to every
location-bearing result in this change. In short: an entry that names a coordinate also
carries `text`, the verbatim source at that coordinate, and an optional `context_lines`
parameter widens it to `context` plus `context_start_line` when the caller wants a window
rather than a line. `context_lines` defaults to 0, a single over-long line is truncated
with an ellipsis, the text is read from the same content the query was answered against
(the working-copy overlay included), and a file that cannot be read costs the entry its
`text`, not its place in the result.

Two points are specific to this change:

- **Which range the text comes from.** For `go-to-definition`, `find-implementations`,
  and each call site in `ranges`, the range *is* the interesting line, so the text is the
  line or lines that range covers. For a `file-outline` symbol and a call hierarchy item,
  the range is a whole declaration — a class, a method body — and its text would be the
  file. Those use the **selection range**, the identifier's own line, which is also the
  coordinate a caller would hand to `rename`. The rule is: text follows the range a
  position-targeted operation would be given, never the enclosing body.
- **`describe-symbol` needs no `text`.** Its whole result is already source-derived prose;
  a line of code beside it would be redundant. It carries the hovered range so a caller
  can act on the symbol, and nothing more.

This is a deliberate deviation from LSP, which returns bare `Location`s because its
clients are editors that already have the file open. Henka's caller does not, and the
measurements above are about the caller Henka actually has.

`go-to-definition` and `find-implementations` share the location conversion with
`find-usages`, differing only in the key their list is published under. That conversion
grows one capability they need and `find-usages` does not: LSP permits these two requests
to answer with a single `Location`, an array of `Location`s, or an array of
`LocationLink`s (a shape carrying `targetUri`/`targetRange` instead of `uri`/`range`).
All three are accepted and produce the same flat list, since which one arrives is a
property of the server, not of the question asked.

## 4. Capability advertisement

A language server may legitimately refuse a request a client never said it could handle,
so each provider's `initialize` must advertise what its new operations use. Java already
advertises `implementation`, `documentSymbol` (with hierarchical support), and
`callHierarchy`; none of the three providers advertises `hover`, and Rust and
TypeScript advertise neither `implementation`, `documentSymbol`, nor `callHierarchy`.
Each operation's commit adds the capability its request needs to the providers that
lack it, so the advertisement and the operation that depends on it land together.

## 5. Errors and edge cases

- If the project's index is not yet ready, `ensure_indexed` surfaces that as a backend
  error before the request runs, consistent with every other query operation
  (SPEC.md §11).
- **An empty answer is not an error.** A position with no definition, a class with no
  implementations, a method with no callers, and a file with no symbols are all
  successful results with `count: 0`. This matters more here than for `find-usages`: "no
  implementations" is a real and useful answer, and an error would tell the caller
  nothing about whether it asked wrongly or asked correctly and got nothing.
- A missing or malformed `item` on `incoming-calls` / `outgoing-calls` **fails with a
  validation error** and runs nothing, per SPEC.md §11, matching how `symbol-search`
  treats a missing `query` rather than substituting a value the caller never asked for.
  An `item` the caller has edited is the server's problem to reject; Henka does not
  attempt to validate its internals, since their meaning is the server's alone.
- A position that resolves to nothing yields `count: 0` rather than an invalid-target
  error. Henka cannot distinguish "you pointed at whitespace" from "this symbol genuinely
  has no definition here" without duplicating the server's analysis, and guessing between
  them would report a confident error for a correct question. The existing `expect` guard
  remains the way a caller checks it aimed at the token it meant.
- **Missing source text is not a failed query.** A file that cannot be read, or a
  coordinate past its end (a stale index against a shrunk file), yields an entry without
  `text` rather than an error — and never a *wrong* line, which would be worse than none.
- These are all read-only, so the working-copy overlay (a sibling git worktree or jj
  workspace) applies to them exactly as it does to `find-usages`: results reflect the
  overlaid content, and nothing is written back.

## 6. Relationship to the existing catalog

This implements three catalog entries SPEC.md §8 already lists — `go-to-definition`,
`find-implementations`, and `call-hierarchy` (as its three constituent operations) — and
adds two entries the catalog does not yet mention, `describe-symbol` and `file-outline`,
which join the semantic-queries section. `type-hierarchy` and structural
search-and-replace remain unimplemented and are out of scope here.

The one existing behavior it does change is the result shape of `find-usages` and
`symbol-search`, which gain source text (§3.1) so that every query in the catalog answers
in one shape. That is an addition to their responses — existing fields keep their names
and meanings — but it is a change to a shipped operation, and it lands in its own commit
ahead of the new ones rather than being smuggled in beside them.

Nothing about the protocol or the target model changes. Per
SPEC.md §13 ("Operations are plugins, not protocol"), each provider opts in by adding
the new operations to its `operations()` list, and the MCP surface grows only by the
tools those operations imply.
