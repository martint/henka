# Symbol search — Spec

Change: `symbol-search` (jj `uuywzvtm`)

## 1. What this change is

Every operation in the catalog up to this point is targeted by a **coordinate** — a file plus a
position, a selection, or a whole file (SPEC.md §2.5). A caller holding only a symbol's *name* has
no coordinate to hand over: it has to fall back to a text search of the tree and guess which hit is
the declaration, exactly the kind of guesswork Henka exists to replace (SPEC.md §13, "Semantics
over text").

This change closes that gap by exposing each language server's `workspace/symbol` request as a new
**query operation**, `symbol-search`. It resolves a name (or partial name) directly to the file and
range of every matching symbol, so a caller can go straight from "I know this is called `Foo`" to
a coordinate the position-targeted operations (`find-usages`, `rename`, …) can then act on.

## 2. The operation

`symbol-search` is a query operation (SPEC.md §2.4) with:

- **target**: the project (SPEC.md §2.5) — it is not anchored to any file.
- **parameters**: a required `query` string, the partial or full symbol name to search for, and an
  optional `limit` bounding how many matches are returned.
- **result**: a structured list of matching symbols, never a workspace edit.

`query` is required the same way `RenameOp`'s `new_name` is: a call that omits it fails with a
clear validation error rather than running. An empty string is not a safe substitute — jdtls turns
it into a wildcard matching the entire workspace index, while rust-analyzer returns nothing for the
same input, so silently defaulting would make "the caller forgot the parameter" behave differently,
and expensively, per language.

`limit` exists because this is a prefix/fuzzy query rather than a resolved-symbol lookup like
`find-usages`: a short query on a large project can match far more symbols than are useful in an
agent's context window, and the language servers don't cap consistently (rust-analyzer caps
internally, jdtls does not). `limit` defaults to a fixed value when omitted.

It is offered by every language provider that already ships an LSP-backed session — **Java**,
**Rust**, and **TypeScript** — since `workspace/symbol` is a standard LSP request each of their
backing servers already implements. The operation's `run` method is identical in shape across all
three providers: ensure the session's index is warm, issue `workspace/symbol` with the query, and
normalize the response.

## 3. Result shape

The result is normalized the same way `find-usages` already normalizes locations (SPEC.md §9): a
caller reads one shape regardless of which language server answered, and never handles a raw LSP
type or a `file://` URI.

Each matched symbol is reported as:

- `name` — the symbol's name.
- `kind` — the symbol's kind as a lowercase name (`"class"`, `"method"`, `"field"`, …), not the raw
  LSP integer. The operation's whole premise is that a caller holding only a name can't tell which
  hit is the one it wants — `["Foo", "Foo", "Foo"]` across three files is disambiguated by kind, and
  a bare LSP enum value doesn't do that unless the caller has the spec memorized. Unmapped values
  fall back to a stringified form rather than failing.
- `file` — the path to the declaring file, made relative to the project root where possible.
- `start_line` / `start_character` / `end_line` / `end_character` — the symbol's range.
- `container_name` — the enclosing symbol's name, when the language server provides one (e.g. the
  class containing a method).
- `text` — the source line the symbol is declared on, verbatim (§3.1).

The overall response is `{ "count": <n>, "symbols": [...] }`, matching the `{ "count", "usages" }`
convention `find-usages` already established. An empty or null LSP response yields `count: 0` and
an empty list rather than an error. When the match count exceeds `limit`, the response also carries
`"truncated": true` and `count` still reports the *total* number matched, not the number returned —
the caller is told honestly that it needs to narrow the query, rather than silently being handed a
partial answer that looks complete.

### 3.1 Every match carries its source text

A coordinate alone is not a useful answer to a language model. Measuring LSP-backed navigation
against `grep` for Claude Code found the gap was not in the semantics but in the payload: a tool
that returned only `file`, `line`, and `character` forced the agent to open the file to see what
was actually there, while `grep` had already shown it —
`src/auth.ts:42: return validateToken(token)`. Adding the source text to the response, with the
semantic backend and the matched set unchanged, moved pass@1 on rename tasks from 0.67 to 0.83 and
cut follow-up file reads from 15.2 to 3.2 per episode.

So `text` is not an optional nicety on this operation; it is most of its value. A caller asking
"where is `Foo`?" across a large project gets back a list it must *choose* from, and choosing
between five `Foo`s by path and line number alone is precisely the guesswork that sends it back to
reading files. With the declaring line in hand — `public final class Foo implements Bar {` — the
choice is usually made without another call.

The rules:

- `text` is the **declaration's own line**, taken from the symbol's range, not the whole
  declaration. A range that spans several lines (a multi-line signature) contributes all the lines
  it covers, joined by newlines; nothing beyond the range's lines is included.
- An optional `context_lines` parameter widens the window. When it is greater than zero the entry
  also carries `context` — the window as one string — and `context_start_line`, the zero-based line
  the window begins at, so the caller can still map any line it reads back to a coordinate. It
  defaults to **0**: the declaring line alone is what earned the accuracy gain, and a default
  window would multiply the cost of a 200-match response for a benefit the caller can ask for.
  It is capped, so a large `limit` and a large window cannot combine into an unbounded response.
- A single line longer than a fixed character cap is truncated with a trailing ellipsis. A minified
  or generated file should cost one line's worth of context, not a screenful.
- The text is read from **the same content the search was answered against** — the working-copy
  overlay included, when the request named a workspace whose edits are overlaid on the base index
  (SPEC.md §9). Reading it from the base checkout instead would hand the caller a line that does
  not match the coordinate beside it, which is exactly the mis-resolution the `expect` guard exists
  to catch.
- If the file cannot be read, `text` is **omitted** and the match is still returned. The location is
  the answer; the text is context for it, and losing the context is not losing the answer.

This departs from LSP, whose `SymbolInformation` carries no source text and whose clients are
editors that already have the buffer open. Henka's caller has no buffer, so the departure is the
point.

This normalization lives in `henka-lsp`'s `convert` module (`symbols_to_query`), alongside
`locations_to_query`, so every LSP-backed provider shares one implementation instead of each
reimplementing URI-to-path conversion, range flattening, and source-text extraction.

## 4. Errors and edge cases

- If the project's index is not yet ready, `ensure_indexed` surfaces that as a backend error before
  the search runs, consistent with every other query operation (SPEC.md §11).
- A query that matches nothing is not an error: it is a successful result with `count: 0`.
- A match whose file cannot be read loses its `text`, not its place in the result (§3.1).
- A missing `query` parameter **fails with a validation error** and runs nothing, per SPEC.md §11
  ("An operation that cannot be performed safely... fails with a clear reason and changes
  nothing") — matching `RenameOp`'s treatment of a missing `new_name` rather than silently
  substituting a value the caller never asked for.
- LSP 3.17 allows a `workspace/symbol` response to return a `WorkspaceSymbol` whose `location` is
  `{ uri }` with no range (the client is expected to resolve it later via
  `workspaceSymbol/resolve`). Henka does not advertise `workspace.symbol.resolveSupport`, so a
  spec-abiding server should not send that reduced form — but if one does, a symbol missing a
  range is dropped from the result rather than failing the whole search.

## 5. Relationship to the existing catalog

This adds one entry to the semantic-queries section of the catalog (SPEC.md §8): **symbol-search**
— find symbols by name or pattern across the project. It does not change the protocol, the target
model, or any other operation; per SPEC.md §13 ("Operations are plugins, not protocol"), each
provider opts in by adding `SymbolSearchOp` to its `operations()` list, and nothing about the MCP
surface itself changes.
