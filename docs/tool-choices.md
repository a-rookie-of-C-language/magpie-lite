# Tool choice and allowlists

The shared core normalizes function-only selection before each Chat, Responses,
Anthropic or Gemini encoder builds its provider request. `auto`, `none`, required
selection, and one named function are translated into the target dialect rather
than forwarding another API's raw `tool_choice` object.

- Responses `allowed_tools` filters function declarations to the selected names.
  An offered Responses hosted-search tool is removed if it is not selected.
  Native Responses encoding retains the original selector and selected search
  configuration; foreign encoding normalizes the selector after filtering.
- Gemini `allowedFunctionNames` (including snake-case aliases) filters declared
  functions. Multiple names under `ANY` remain a required subset, not `AUTO`.
  An absent or empty Gemini name list means unrestricted declared functions.
- Encoding works on a request-local clone: translating one route does not alter
  the request used by a later route or fallback.
- Required selection with no callable tool, or a named undeclared function, is
  rejected even for native requests. An empty Responses allowlist does not mean
  all functions. Error messages do not include caller-provided names or values.
- Ambiguous aliases, duplicate flat/nested selected names, malformed mode values,
  namespaced allowed functions and unsupported allowlist restrictions are
  rejected instead of silently dropping constraints.
- Gemini `VALIDATED` and native additional selector options (such as Anthropic
  `disable_parallel_tool_use`) are retained for their native protocol and rejected
  on foreign routes. No cross-protocol parallel-call policy is implemented here.

Hosted-search requests other than Responses `allowed_tools` continue to use the
existing search translator's native-option and global/function-choice contract.
Gemini function configurations are validated before that delegation: search
cannot bypass native-only `VALIDATED`/additional restrictions or malformed mode
and alias checks. `VALIDATED` permits text as well as calls, so translating it to
`ANY` would change its meaning. A compatible native fallback preserves the raw
configuration and search; an incompatible route is skipped without an HTTP call,
key attempt or cooldown. If no compatible route remains, the client gets a
sanitized HTTP 400 in its own protocol, including for a streaming request that
has not committed headers. Native search keeps its original declaration/choice
contract rather than applying function-only declaration filtering.

This change is not a general translation of provider-specific search options,
namespace tools, or other hosted built-ins. It enforces the policy carried in
provider requests, not a sandbox that executes or independently authorizes
model-generated function calls.

Evidence: `crates/core/tests/tool_choices.rs` checks all four basic modes in all
16 protocol directions, allowlist filtering/search exclusion, invalid and
nonportable selectors, request immutability, and search/native-policy bypass.
The existing search-request suite checks native configured-search and
allowed-tools preservation.

`crates/gateway/tests/tool_choices.rs` adds seven real loopback HTTP tests with
576 client requests (380 successful, 196 locally rejected), using independently
authored native payloads and replies:

- 256 basic-mode requests: four clients x four providers x four modes x
  direct/group routes x JSON/SSE.
- 64 subset/empty-auto allowlist requests, including offered-but-excluded search.
- 112 malformed or impossible selections, rejected before any key is used.
- 36 native-only choices that skip an incompatible two-key pool and use a native
  provider fallback, both directly and through a group, including Gemini search.
- 12 upstream-failure fallback requests verifying a foreign encoder's filtered
  clone does not mutate the native Responses selector or enable excluded search.
- 48 native-only policy requests with no fallback (12 native successes,
  36 foreign failures), plus 48 malformed Gemini configurations with search.

The fixtures check actual upstream auth/URL/model, selector/declaration contents,
downstream text/usage/one terminal, request/key accounting, no credential attempt
or cooldown on local conversion errors, and private-input-free stats/JSONL history.
They do not establish installed-Agent or live-vendor compatibility. Verification:

```sh
cargo test -p magpie-core --test tool_choices
cargo test -p magpie-gateway --test tool_choices
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
