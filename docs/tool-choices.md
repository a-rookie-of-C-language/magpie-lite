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
This change is not a general translation of provider-specific search options,
namespace tools, or other hosted built-ins.

Evidence: `crates/core/tests/tool_choices.rs` checks all four basic modes in all
16 protocol directions, allowlist filtering/search exclusion, invalid and
nonportable selectors, and request immutability. The existing search-request
suite checks native configured-search and allowed-tools preservation. These are
core codec tests, not new real-agent or live-vendor compatibility evidence.
