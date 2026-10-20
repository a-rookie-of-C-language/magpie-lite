# Codex tool-search relay

Updated: 2026-10-20.

Reference: `D:\code\magpie\internal\gateway\toolsearch.go`,
`internal/gateway/responses.go` (`callTo`, `searchFound`, `searchTidy`,
`searchCall`, `itemPrefix`), `internal/gateway/toolsearch_test.go` and
`internal/gateway/search_call_ids_test.go`.
Lite port: `crates/core/src/responses.rs`,
`crates/gateway/tests/tool_search.rs`.

Codex runs its deferred tool discovery itself (`"execution": "client"`).
Few Responses APIs outside the ChatGPT backend know the `tool_search`
declaration, `tool_search_call`, or `tool_search_output`, so the request
goes out with the search as a plain function, its calls and results as a
function's, and the tools a search found in the tools; a call the model
makes to it comes back to Codex as the `tool_search_call` Codex runs.
Lite never serves the ChatGPT backend, so every Responses upstream gets
the function form on every route.

## What lite does

- Decodes a `"tool_search"` declaration with missing or `"client"`
  execution as a plain non-custom function tool. Other executions are
  skipped.
- Decodes a `"tool_search_call"` history item as an assistant call to
  `tool_search`, and a `"tool_search_output"` item as its result with the
  text `"These tools are now available to call: ..."` (or
  `"No tools matched the search."`). Tools the output found are offered
  under flat `ns__name` names with `defer_loading` dropped; unparseable
  or duplicate found entries are skipped, never request errors, and
  declarations win on name collision.
- Encodes a model call to `tool_search` back to the client as
  `tool_search_call` with a `tsc_` id, `"execution": "client"`, and
  object `arguments` (no `name`). Both non-streaming and streaming
  encoders do this; streaming deltas stay function-shaped, as the
  relayed upstream stream keeps them, and no
  `function_call_arguments.done` is emitted for the search.
- Upstream requests always carry the function form: history and
  declarations go out as ordinary functions, so no ChatGPT-backend
  detection, gateway-executed search, or describe-via-seer path exists.

## Evidence and limits

- `crates/core/src/responses.rs` `tool_search_tests`: declaration/call/
  output decode with found-tool registration and flattening, `tsc_`
  non-stream encode with object arguments, and streamed added/done shape
  without a function done event.
- `crates/gateway/tests/tool_search.rs`: a Responses client with a
  `tool_search` declaration and a search turn in history, against Chat,
  Anthropic, Gemini, and Responses native upstreams, JSON and SSE. Each
  upstream mock asserts the search went as a function, the call/result
  as a function's, the found namespace tool without `defer_loading`,
  and no `tool_search_call`/`tool_search_output`/`execution` leakage;
  each downstream asserts the model's search call returns as
  `tool_search_call` with a `tsc_` id and object arguments.
- History `tsc_`/`fc_`/`ctc_` id normalization from other vendors is
  intentionally not ported: lite mints fresh `tsc_` ids on encode and
  never rewrites client history ids.
