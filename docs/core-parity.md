# Core parity tracker

Updated: 2026-10-19. Reference checkout: `D:\code\magpie` at `5774c790`
(1,198 commits past the prior baseline `dde3a31a`); implementation:
`D:\code\magpie-lite`.

Target: multiple coding agents sharing one Rust backend, provider adapters for different services, and an interface managing providers, models, routing and usage. The reference rust-core-refactor-plan.md, rust-core-contracts.md and rust-core-admin-api.md are proposals with broader acceptance gates. This tracker does not claim G1–G16 completion or full Magpie product parity.

| Area | Current evidence | Remaining work |
|---|---|---|
| Shared Rust gateway | Tool-returned image ownership and four-client/four-upstream translation with a 64-request real loopback JSON/SSE direct/group matrix (docs/tool-transcripts.md); Chat-bound split/parallel/interrupted/orphan tool-history repair with request-local ID normalization (docs/tool-transcripts.md); core wire codecs; real local HTTP/SSE terminal/error/EOF and after-header disconnect fixtures across four adapters; incremental 32 MiB raw-event bounds, BOM/CR/UTF-8 handling, opening event/metadata-byte budgets, early stream-error key/fallback/group retry, no ordinary fallback after text/reasoning/tool output, configurable per-attempt first-content/read-idle deadlines with timeout fallback, group non-final first-content switching, request-wide Responses opening release, downstream pre-header/mid-answer SSE comments with safe fallback, protocol error conversion, bounded upstream-silence windows and activity-driven cadence, bounded per-channel text/reasoning repetition protection with no answer/tool replay and one same-candidate reasoning-only reask plus bounded Gemini empty-reply reasks with combined client/history usage, and real HTTP/1.1 before/after-header cancellation with persisted accounting; see docs/stream-lifecycle.md | Recorded real-agent fixtures, broader multimodal/tool/stream conformance, queue/slow-final-route behavior, cross-protocol search-option translation and cross-client image rendering, provider-specific prefill continuation, optional post-content progress policies, HTTP/2 cancellation conformance, provider-specific tool-schema rewrites, and RPM/concurrency queues |
| Tool-selection policies | Four-dialect auto/none/required/named normalization; reasoning-effort levels (`none`/minimal→low, low/medium/high/xhigh/max, Anthropic budget↔level via the reference's 4096/12000/24000 breaks, Gemini thinkingLevel/thinkingBudget, Anthropic budgets fitted under the client's max_tokens cap) translated in all sixteen source×target directions with no silent drops; Responses/Gemini function subsets; request-local filtering; native-only restrictions; parallel-calls constraints (`parallel_tool_calls` on Chat/Responses, Anthropic `disable_parallel_tool_use` translated to Chat/Responses and rejected on Gemini rather than silently dropped); Responses namespace declarations/calls/tool choices and Responses custom (free-form) tools — decoded as grammar-described input functions, offered to foreign dialects, and restored to `custom_tool_call`/`custom_tool_call_input.delta` events from request-declared tools; namespace declarations/calls/tool choices, streaming namespace preservation, and request-local restoration of flat `ns__tool` names back to Responses namespace calls across Chat/Anthropic/Gemini/Responses upstreams, direct/group routes, JSON/SSE responses and cross-protocol fallback (no historical-name guessing); seven loopback HTTP tests with 576 JSON/SSE direct/group client requests, native compatible fallback without key attempts/cooldowns, search/native-policy bypass regression (docs/tool-choices.md), and dedicated namespace-restoration loopback tests in `crates/gateway/tests/namespaces.rs` | Provider-specific search options, adaptive-only Claude thinking paths, real-Agent and live-vendor evidence |
| Agent adapters | Shared magpie-agents adapters for Claude Code, Codex, Crush, Gemini CLI, Goose, OpenCode and Zed; private staged preview, revision-checked apply, OS lock, schema-2 journaled selective field restore preserving unrelated edits, disconnect, managed-field drift and explicit recovery. Agent unit, HTTP, sandboxed child CLI and real-gateway console browser tests; Windows ACL/junction checks; recursive duplicate-key JSON/JSONC refusal, format-preserving JSON edits, compact comment-free JSON object additions that preserve single-line layout, native OpenCode/Zed JSONC, shared passive CLI/config discovery with bounded user-prefix/Node-version searches, npmrc prefix and nvm default-alias handling, and PATH/unknown metadata, native absolute config-directory overrides for Claude/Codex/OpenCode/Zed, and seven-Agent selective restore through HTTP/CLI/browser | Native project overrides, login-shell/registry PATH refresh, process/reachability and real-version compatibility evidence; compact layout preservation is intentionally limited to single-line objects without direct comments |
| Provider metadata and keys | Protected provider CRUD and console; providers.rs covers saved/live edits, secret-free capability metadata (`supported_protocols`, nullable streaming/tools/vision), protocol validation and preserve/clear semantics. provider_tools.rs covers four-protocol catalogs/probes, pagination, saved/draft/no-auth credentials and bounded read-only operations | Capability presets, specialized/stream-only probes and paid/live vendor evidence |
| Hot edits and key rotation | ConfigStore saves before publishing; requests own snapshots. Versioned key schedules ignore stale requests. providers.rs checks an old in-flight request after a smaller pool is saved; config.rs checks stale scheduler calls. Durable config revisions/ETags and optional If-Match checks reject stale provider/group writes across restarts; the revision is committed in the atomically replaced TOML document, store and HTTP tests check unchanged disk/memory and a single winner for concurrent checked edits; gateway writers also serialize through a bounded file lock. Provider/group CRUD and agent apply now have durable idempotency with 256-entry retention, token-keyed HMAC digests, restart replay, pending fail-closed behavior, and secret-free ledgers | Encrypted secret storage with recovery; durable idempotency for any future agent rotate endpoint |
| Routing and catalog | Explicit provider/model, listed bare model, default provider, and process-local group `order`/`rotate`/`usage`/`weight` scheduling with ordered fallback; per-provider key-pool and per-group-route smooth weighted round robin with bounded (0–1000) weights, missing weights defaulting to 1, explicit group-route zero making that route fallback-only, and hourly key token-usage decay; provider/group conversation affinity (`auto`/`session`/`turn`/`off`) over provider key slots, persisted beside the config and editable through CLI, HTTP and console; config unit tests, providers.rs, groups.rs and e2e.rs | Allowance/pace policies, subscription-account affinity (including account effort/allowance reasons), richer model metadata and dynamic discovery |
| Usage | Cumulative and partial stream usage, last-attempt route accounting before await, cancelled fallback JSONL persistence/reload, sanitized JSONL history, recent stats, bounded Unix-millisecond `from`/`to` filters for recent requests and aggregates, key slots/cooldowns and token totals; usage.rs and e2e.rs | Cost/pricing/quota, discarded-attempt billing/unknown-usage policies and persisted aggregates |
| Interface | Dependency-free protected console for Agent lifecycle, providers, routing groups, catalog and stats; browser exercised CRUD, draft/saved discovery and probes, errors, key/list clearing, busy controls, delete protection, Escape/focus and 390px layout; stats API and console controls support bounded recent-history filtering with automatic refresh preservation, validation and clear; tests/browser/agents.cjs exercises all seven adapters, restoration/unrelated-edit preservation/drift/duplicate-JSON refusal/preview/recovery/session/busy flows plus passive seven-Agent CLI paths and text-only long-path/unknown warnings | Replacement interface clients |
| Management security and persistence | Loopback server, bearer/header auth, atomic TOML save, secret-free metadata responses, external-edit conflict detection, durable provider/group revision checks, and durable provider/group idempotency with restart replay, 256-entry retention, token-keyed HMAC digests, pending fail-closed behavior, and a secret-free ledger | Separate agent/admin scopes, versioned API, encrypted secret store, schema migration and broader durable operations (agent operations now have revision/preview and journaled recovery); durable idempotency for agent apply/rotate |

Provider workflows were compared with upstream internal/gui/providers.go and docs/subsystems/providers-accounts.md: ordinary API providers support lifecycle editing, protocol choice, model selection and connection checks. Lite now covers lifecycle editing, manual/discovered model lists and selected-protocol generation checks on saved or unsaved drafts. The new provider-tools suite uses local HTTP fixtures and checks time/byte/model/page/key bounds, no redirects, sanitized errors, unchanged config/statistics and untouched live rotation/cooldowns. Browser automation exercised both saved-key reuse and unsaved drafts; this is not evidence of paid live vendor compatibility.

Console lifecycle evidence is reproducible with tests/browser/agents.cjs using a
real child gateway, temporary Agent home and no inference traffic. The browser
suite passed on Windows/Edge and automatically checked 390px overflow/focus and
control sizes. Recovery is a synthetic journal fixture; some error/busy/session
responses are injected. Captured screenshots are not evidence of manual visual
inspection or real Agent-version compatibility.

Next implementation sequence:

1. Continue agent lifecycle parity: native config-location/project semantics; passive config/CLI discovery is now shared by the library, CLI, backend and console without executing Agent binaries. Format-preserving JSON/JSONC edits, compact layout preservation, and selective field restoration now share the backend/CLI/console preview, journal, apply and recovery path; see docs/agent-lifecycle.md for exact limits. Native OpenCode JSONC is exercised through HTTP, sandboxed CLI and real-gateway console browser tests; Zed JSONC and all JSON adapters have lifecycle format regressions.
2. Broaden routing and protocol fixture coverage across the existing four adapters; the group `order`/`rotate`/`usage`/`weight` scheduler, per-provider key-pool `order`/`rotate`/`usage`/`weight` scheduling, and key-slot conversation affinity are now implemented. Subscription-account affinity and allowance-driven release reasons remain unimplemented because lite has no subscription accounts.
3. Usage/history management and stronger management API concurrency/authentication contracts.

Subscriptions, plugin hosting, cloud sync and upstream platform integrations are outside this initial core scope. A green workspace suite supports only the behaviors it exercises; it does not prove those wider proposal gates.

Native configuration directory overrides are compared with the reference's
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `OPENCODE_CONFIG_DIR` and
`MAGPIE_ZED_CONFIG_DIR` handling. CLI coverage verifies that absolute overrides
select the files used by preview without writing them; project-local overrides
remain intentionally unsupported.

Passive Agent discovery was compared with upstream `internal/agent/agent.go`
(`Detected`) and `internal/proc/path_other.go`/`path_windows.go`. Lite reports
config/file/launcher evidence, a bounded captured-PATH membership result and
fallback user installation locations, npmrc prefixes and nvm default aliases, but
deliberately does not claim the reference's login-shell/registry PATH refresh,
Agent-version probes, process detection or project/native-config override parity.
Unit/lifecycle/HTTP/child-CLI/real-gateway browser fixtures cover the implemented
contract; no real installed Agent was executed to prove version compatibility.

Npmrc/nvm discovery was additionally compared with upstream
`internal/proc/nodever_other.go`: alias/default is a file, explicit NVM_DIR
replaces the home root, and missing/system/cyclic defaults retain installed-version
fallbacks. Bounded alias-chain/LTS parsing, refreshable npmrc prefixes and canonical
sandbox confinement have Windows unit fixtures and Linux/WSL fixtures (including
real lts/* filenames and escaping symlinks). An authenticated HTTP test refreshes
both inputs in one live manager and verifies unchanged config, statistics and
preview revision. These fixtures do not execute Node or any Agent launcher and
do not prove real terminal PATH equivalence.

Unix lifecycle recovery is additionally covered under WSL/Arch Linux: snapshots
canonicalize permission bits while accepting legacy schema-1/2 regular-file stat
modes without rewriting journals on read. Seven-Agent fixtures exercise exact
and selective restoration (including user chmod changes); pending-operation
fixtures check partial publication, originally missing files and fail-closed
permission drift. A child CLI restores legacy journals without gateway config.
These synthetic recovery fixtures do not establish real process-crash durability
or installed Agent-version compatibility.

Streaming fixtures are additionally compared with upstream `stream_error_open_test.go`
and `canceled_usage_test.go`. Held-open HTTP-200 rate-limit events can now retry
before content, across keys, fallback providers and groups; invalid requests and
committed text/reasoning/tool calls do not replay. Tests distinguish real
after-header HTTP disconnects from explicit pending-handler task cancellation.
Separate real HTTP/1.1 pre-header socket fixtures now prove automatic cancellation
in the current Axum serving path, with both streaming deadlines disabled.
See `docs/stream-lifecycle.md` for exact completion, cooldown, accounting and
remaining timeout-policy/vendor-compatibility limits.

The shared SSE reader was additionally compared with upstream `internal/gateway/sse.go`
(32 MiB scanner-line bound). Lite now enforces a 32 MiB aggregate raw-event bound
while consuming borrowed chunks incrementally, supports fragmented BOM/CRLF/CR
and UTF-8, and separately bounds retained opening metadata. Core boundary fixtures
and real held-open HTTP oversize fixtures verify failover before content, no
replay after content and resource release. The event bound is stricter than the
reference for aggregate multiline events; it is neither a total request-memory
cap nor a timeout. See `docs/stream-lifecycle.md` for the exact accounting.

Streaming now has independent root TOML first-content (default 10 minutes) and
read-idle (default 5 minutes) limits; zero disables each. Real local HTTP fixtures
cover opening/after-content stalls across all four-by-four protocol pairs,
ordinary/group timeout fallback, ignored-heartbeat and committed-metadata cases,
header/error-body stalls, paced output, reasoning/tools and disable semantics.
These are gateway-wide safety bounds; the separate Group.FirstToken slow-start
switching policy is now implemented as described below. Downstream SSE
comment keepalive and Responses opening release are documented separately.
See `docs/stream-lifecycle.md`.

Real pre-header cancellation is now verified without a new production mechanism:
five `socket_disconnect_*` tests exercise 160 local HTTP/1.1 cases across all four
client/upstream dialects, stream/nonstream waits, opening prefetch, HTTP error
bodies and ordinary/group fallback. Closing only the client socket releases all
attempted upstreams and writes one persisted last-attempt record, preserving
observed usage but never abandoned-attempt usage. This closes the prior
HTTP/1.1 evidence gap, not HTTP/2, TLS/proxy or live-Agent compatibility gaps.
See `docs/stream-lifecycle.md` for the exact fixture scope.

Streaming Responses clients now have a request-wide opening hold budget
(default four minutes, zero disables), independent of SSE comments. Real buffered
opening events can be published before content, committing the selected attempt;
headers/comments/usage-only input do not fabricate metadata. The budget survives
key/provider/group fallback and does not reset first-content/read-idle limits.
Nine local HTTP fixtures and paused-clock unit tests cover the release contract.
This follows the opening-release subset of reference `internal/gateway/fallback.go`,
not queue, slow-final-route or real-Agent conformance.
See `docs/stream-lifecycle.md`.

Group first-content switching now follows the reference's non-final streaming
Group.FirstToken policy, with a disabled-by-default millisecond group budget.
A single per-attempt deadline covers headers and held opening; slow routes skip
remaining credentials without cooldown. The final scheduled route is exempt,
not exempt from global stream limits. Content or released metadata commits the
candidate and prevents replay; known HTTP errors stop this opening policy.
TOML, snapshot, management API, CLI and console share the field. Thirteen local
HTTP tests plus paused-clock/API/child-CLI/browser coverage exercise the contract,
including scheduled order, pinned edits and persisted cancellation. Queue,
slow-final-route UI, post-content progress policies and live-Agent/HTTP2 evidence
remain open; see docs/stream-lifecycle.md.


Stream repetition protection now follows the reference's 256-unit/24-distinct
window and two 4096-byte run-on units, on independent text/reasoning channels
across all four protocols. The root `stream_loop_guard` setting defaults on and
can be disabled; tools/metadata/nonstreaming replies are excluded. Triggering
content is preserved, then a secret-free terminal protocol error releases the
upstream without key/provider/group replay. Seven bounded-memory/threshold unit
tests, config/codec tests and eight real-HTTP fixture tests cover aliases,
normal-code/tool negatives, disable/pinning, EOF, fallback plus keepalive,
selected-route partial usage, durable history reload and socket cancellation.
The policy is heuristic, with documented Unicode/key differences and possible
false positives; it is not semantic progress detection. Reasoning-only loops now
re-ask the same provider/model/key slot once after a one-second pause, preserving
reasoning and one logical client response. Client/history usage is summed without
recounting cumulative frames. The new attempt observes header/prefetch deadlines,
read-idle and keepalive policies; failures do not rotate or fall back. Local
HTTP/socket fixtures cover repeated loops, early stream/HTTP errors, attempt
counters, header timeouts and pause/header-wait cancellation. Gemini empty replies
now re-ask the same candidate once while held, at most twice after ordinary stream
commitment (once for groups), before returning one error on exhaustion. Empty
retries keep emitted reasoning and combine same-candidate usage, but never replay
a committed answer through another route. Local fixtures cover all four clients,
streaming/nonstreaming fallback, independent reasoning-loop budgets, HTTP errors,
EOF/JSON distinctions, pinned edits and cancellation. At that increment, image/search classification prevented retries without their
stream IR. The later safety and image increments below add refusal routing and
native Gemini image rendering. The later search increment adds result IR; subscription adapters and
provider-specific prefill remain gaps. See
`docs/stream-lifecycle.md` for the contract and remaining queue/HTTP2/Agent gaps.

## Safety-refusal increment (2026-10-12)

Distinct content-filter stops, held refusal routing without key retry/cooldown,
sanitized HTTP 400 exhaustion and post-commit stream errors are now covered by
four-protocol loopback tests. Malformed nonstream success decoding returns 502.
See `docs/stream-lifecycle.md` for the routing allowlist, usage rules and limits.
This closes the prompt-feedback refusal-routing gap, not broader Gemini
image/search stream rendering or reference-length reasoning holding.


## Native Gemini image increment (2026-10-12)

The stream IR now carries image parts. Native Gemini streaming and nonstreaming
replies preserve final inline/file images, MIME types, thought signatures and
text/tool ordering while suppressing thought/draft images and empty attachments.
Images satisfy first-content deadlines and count as answers for refusal/re-ask
policies. Core and loopback HTTP/socket tests cover final/draft separation,
snake-case fields, empty-text parts, pending tools, deadlines, refusal/loop
no-replay behavior, usage and sanitized history on client disconnect.

This is native Gemini output support, not cross-client multimodal parity:
Chat, Anthropic and Responses stream encoders do not render image events;
the search-result increment below is separate. CodeAssist subscription wrappers and real-provider/Agent
compatibility evidence remain open.


## Upstream search-result increment (2026-10-12)

Gemini grounding and Responses web_search_call search actions now become
search parts/events in the shared IR, rather than disappearing or becoming
agent-executed function calls. Anthropic replies expose paired server search
call/result blocks; Gemini and Responses clients receive native result shapes.
Native Gemini grounding metadata retains attribution, citation offsets and chunk
indices. Responses native search actions survive round trips.

Core and loopback HTTP/socket fixtures cover streaming/nonstreaming translation,
valid/empty metadata, single Gemini emission, Responses output ordering and IDs,
first-content deadlines, refusal and reasoning-loop no-replay behavior, observed
usage and query/source-free history on disconnect. Optional opening usage from
Responses-compatible upstreams is retained when reported; missing usage is not
invented. These are synthetic fixtures, not paid-provider or real-Agent evidence.

The later increments below add built-in search requests, Anthropic result
ingress and cross-protocol citation retention. Responses open_page/find
actions (a shared gap with the reference) and Chat search presentation remain
gaps; the repeated/cumulative Gemini grounding contract below supersedes the
earlier single-emission limitation.
Search results do not become agent-executed function calls, and vendor-encrypted
search content is not copied across providers; native Anthropic history is the
exception described below.


## Built-in web-search requests (2026-10-12)

All four request decoders retain provider-executed search separately from agent
function declarations. Anthropic/Responses web_search tools, Chat
web_search_options (or a bare compatibility tool marker), and Gemini
Google Search tools can coexist with functions. A function named web_search is
not itself a hosted-search request. Duplicate/malformed search definitions fail
at decode time.

Native routes retain search options and original tool choices, including Gemini
function allowlists. Bare search intent translates between all four dialects;
Anthropic uses web_search_20250305 with max_uses=5, and Responses requests search
sources. Cross-protocol domain/location/budget/context policies and unsupported
forced choices return an encoding error instead of silently losing restrictions.
Core request encoders now return Result<Value>.

Anthropic/Responses NONE disables all tools and omits hosted search. Chat/Gemini
NONE disables only functions: hosted search remains enabled; cross-protocol
routes omit function declarations to preserve that distinction. Function-only
required cannot become a global required choice, and neither Chat tool_choice
nor Gemini functionCallingConfig can force hosted search.

The gateway uses a web plugin only for the exact openrouter.ai Chat host and
omits the OpenAI sources include only for the exact api.x.ai Responses host.
Configured Chat search options cannot be silently converted to plugin options.
Encoding failures skip incompatible routes without sending HTTP or recording a
credential failure/cooldown; a compatible configured fallback may proceed. An
exhausted routing plan returns HTTP 400 with a configuration-free error message.

Evidence is local core fixtures, host-specific gateway unit tests and real
loopback HTTP/SSE fixtures, not paid/live-provider or real-agent conformance.
Remaining work includes configured cross-protocol option conversion, Responses
open_page/find and Chat search-result presentation.
Anthropic search-result ingress is described below.


## Anthropic hosted-search ingress and lifecycle (2026-10-12)

Anthropic server_tool_use named web_search and web_search_tool_result now enter
IR as distinct search parts, never ordinary function calls. Keeping each native
block separate preserves call IDs, tool_use_id links, interleaved text, encrypted
results, result order and native tool errors in replies and Anthropic history.
Result queries are associated by ID, not by the most recent call. Only completed
array-valued result blocks produce public Responses search actions or Gemini
grounding. Call-only and native tool-error blocks do not fabricate successful
foreign search results. Other server tools and unopened input_json_delta indices
are not exposed as client functions; a real function named web_search is unchanged.
Anthropic pause_turn semantics are not yet preserved (the reference also maps
it to a plain end-turn); native text citations now are, as described in the
2026-10-18 cross-protocol citation increment below.

Streaming search start immediately counts as answer/content activity, before
input fragments or results arrive. Search fragments accumulate separately from
client function arguments; a nonempty whole input object takes precedence over
subsequent fragments. Retained serialized search blocks, argument bytes, pending
IDs and queries share a 32 MiB budget and at most 128 pending calls/queries.
Consumed results release retained query state. Invalid argument JSON/non-object
inputs, reused open search indices, duplicate pending IDs, results preceding
call closure, budget exhaustion and message termination with an unclosed search
produce one static error without exposing argument fragments. Error state drops
retained search/function state and ignores later frames until a new message_start.

Gemini JSON encoding aggregates all completed search parts instead of selecting
the first. Queries/chunks retain result order; merged native support chunk indices
are rebased. A single native grounding object remains unchanged. Non-array native
metadata conflicts keep the first value; this is not a general merger of arbitrary
provider attribution documents. Gemini SSE emits a grounding chunk for each
completed search. Repeated/cumulative upstream Gemini grounding updates now have
a separate stream-update contract, described below.

Evidence: core tests first reproduced dropped second-search results, accepted
non-object inputs and fabricated completion for unclosed searches. Core fixtures
cover native JSON/history and SSE round trips, interleaving, query-to-ID association,
malformed fragments, whole-input precedence, error/reset behavior, exact-byte and
pending-count budgets, budget release, and Gemini support-index rebasing.
Eight local HTTP/socket tests exercise all four clients, JSON/SSE native/cross
output, search-start deadlines including groups, key retry before search only,
after-start idle/error/refusal/reasoning-loop no-replay, invalid fragments,
usage/history privacy and client cancellation. The existing search-request HTTP
test now also enables Anthropic search end to end. These fixtures do not establish
paid/live Anthropic or actual-agent conformance. The original reference decoder
ignores hosted-search argument fragments; preserving complete inbound search
results through IR extends that behavior rather than copying a completed reference
implementation.


## Gemini repeated/cumulative grounding updates (2026-10-12)

The reference CodeAssist stream decoder uses a one-time searched flag. Lite's
Gemini decoder previously inherited that limitation: subsequent sources and
citation/attribution-only updates disappeared. It now retains bounded per-stream
state instead of consuming a single search event. The first public result emits
Search; later changes emit SearchUpdate with only new public queries and URLs,
and the exact native grounding object attached separately. Query strings and
source URLs are deduplicated within and across updates, preserving their first
appearance order. Exact consecutive native objects are suppressed; changed
objects, including snake_case fields, source-title corrections and native-only
citation/attribution updates, reach Gemini clients intact. Native chunk indices
are not rebased or interpreted as a synthetic combined document during streaming.

Foreign Anthropic/Responses clients get synthetic search presentations for new
public items, not repeated cumulative snapshots; native-only updates create no
empty completed search. Chat still omits search presentation. URL deduplication
means a later title correction is native-only. There is no claim that Gemini
metadata identifies individual hosted-search executions or links each query to a
particular source. Independent updates and cumulative updates are both accepted
without guessing which native metadata model a vendor uses. Nonstream merging
of distinct completed search parts retains its separate contract above.

Only the latest native object and the seen public query/URL sets are retained.
Their serialized native size plus UTF-8 query/URL bytes may not exceed 32 MiB;
there may be at most 4096 unique public queries and URLs in total. An overflow
clears retained state, emits one static error, and suppresses later frames even
if the overflowing frame also reports a normal finish. Attribution-only metadata
is not an answer and cannot itself satisfy first-content deadlines. It remains
subject to the gateway's bounded opening-event/metadata commitment policy.

Evidence: two core regressions failed before the change (later public results
and citation-only metadata were lost). Core tests cover cumulative/independent
updates, duplicate public items, exact consecutive native repeat suppression,
metadata-only forwarding, exact-byte/one-byte-over budgets, 4096-item exhaustion,
replacement rather than unbounded native-version retention, pre-search metadata,
and terminal overflow. Five loopback HTTP/socket fixtures cover all four clients,
ordinary/group routes, exact native updates, foreign deduplication, sanitized
persisted usage/history, no retries after a committed-search overflow, safe
first-content fallback before an answer, opening byte-budget commitment, and
client cancellation after updates. These are local fixtures, not paid/live
Gemini, CodeAssist subscription or actual-agent conformance evidence.

Native opaque reasoning was compared with the reference gateway sealed IR and
Anthropic/Responses adapters. Lite now retains complete native JSON values for
redacted thinking and encrypted reasoning in request history, ordinary replies
and SSE, with Responses item boundaries and native IDs. Eleven core regressions
and two loopback HTTP tests cover 32 successful client/upstream/direct/group/
JSON/SSE combinations and 16 errors after opaque content; the latter do not
retry keys or switch fallback routes. Unknown native fields remain native, while
only public summary_text crosses protocols. These fixtures do not establish
live vendor compatibility or broader thinking-signature parity. See
`sealed-reasoning.md` for the exact contract.

Native thinking signatures were compared with upstream gateway KThinkStart,
KSig and KTextSig events and the Anthropic/Gemini encoders. Lite now preserves
Anthropic initial/delta signatures and adjacent thinking blocks, Gemini
thinking/text/function-call signatures and signed-only parts in JSON/history
and SSE, with decoded signature provenance preventing foreign-protocol leaks.
See docs/native-signatures.md and the core/gateway signatures suites for exact
coverage, stream commitment/retry semantics and privacy evidence. Real vendor
and model-specific signed transcript acceptance remain unverified.

## Anthropic context-window stop parity (2026-10-18)

The reference gateway maps Anthropic's `model_context_window_exceeded` stop
reason to its length class, beside `max_tokens`. Lite's decoder treated it as
end-turn, losing the distinction that the reply stopped because the model ran
out of context before finishing. `StopReason::from_anthropic` now maps it to
`MaxTokens`, so every client dialect reports a length stop.

Anthropic `pause_turn` has no reference mapping and is still not represented.

## Reasoning-token usage across dialects (2026-10-18)

The reference gateway carries a separate reasoning counter beside output
tokens: Chat's `completion_tokens_details.reasoning_tokens`, Responses'
`output_tokens_details.reasoning_tokens`, Gemini's `thoughtsTokenCount`, and
Claude's subscription `output_tokens_details.thinking_tokens`. Lite's IR usage
had only input/output/cache counters, so a Gemini upstream's thinking spend
disappeared for foreign clients. `Usage` now carries `reasoning`; Chat,
Responses and Gemini decoders read their native fields, and Chat and Responses
encoders emit them back. Gemini's own encoder still writes only its native
usage fields, which do not include a reasoning count.

Anthropic API Messages usage carries no reasoning split, so it is absent
there; Claude subscription thinking tokens are not part of the API core.

## Cross-protocol citation retention (2026-10-18)

Anthropic text-block citations and Responses output-text annotations were
decoded as bare text and dropped. IR text parts now carry a citations list,
and Anthropic citations_delta stream events and Responses
response.output_text.annotation.added events decode into it. Encoders emit
them back natively (Anthropic citations_delta beside an open text block;
Responses annotation events and final annotations), and translate across
dialects: Anthropic citations become Responses url_citation annotations with
url, title and offsets mapped when present; Responses annotations pass
through Anthropic streams unchanged. Chat and Gemini have no per-text citation
carrier and deliberately ignore the events, as they do for native search
presentation. Citations are not answer prose for loop-guard or reask
accounting. Whole-reply stream replay re-emits decoded citations.

## Context-overflow recognition and mid-reply refusal parity (2026-10-18)

The reference gateway recognizes a conversation too long for the model by
wording (`tooLongRe` / `overflowed`) and keeps it with its route: no other
member fits it either, and the agent should compact. Lite's routing had no
overflow notion — such a 400 could still be read as an unserved model or
unknown shape. `contextOverflowed` now ports the wording table (400/413/422,
never 429, never `max_tokens` allowances or paced traffic) and short-circuits
route retry before every other classifier. The token-estimate half of
`tooLongUnsaid` stays unimplemented, as before.

Mid-reply failures — a 200 stream that then emits an error event — already
carry the vendor's raw error body beside its derived status, so refusal
wording falls over and overflow wording stays after headers commit; this
semantic is now proven by tests rather than only by reading the code.

## Tool strict passthrough (2026-10-18)

The reference gateway carries a tool's `strict` flag from Chat's
`function.strict` and Responses' tool field into its IR, and a Responses
upstream says it back verbatim: left out or forced false, the ChatGPT
backend holds the tool to strict mode's rules anyway and refuses a schema
with a lookaround pattern (MiniMax Code's path, #383). Lite's tool IR had
no strict field and its Responses encoder hardcoded `false`. `Tool` now
carries `strict` (Chat and Responses decode it, Anthropic and Gemini have
no such field), and the Responses encoder emits what the client asked,
defaulting to `false` exactly as before.

## Refused optional fields (2026-10-18)

The reference gateway asks an upstream again without an optional request
field the vendor names in a 400/422 refusal, then remembers the refusal for
that provider/model so later requests leave it out from the start (#393,
#1117). Lite now ports that: its optional-field table covers `store`,
`metadata`, `service_tier`, `prompt_cache_key`, `prompt_cache_retention`,
`safety_identifier`, `stream_options`, `parallel_tool_calls`, `verbosity`,
`thinking` and `enable_thinking`; a named refusal is read from structured
`extra_forbidden`/`param` shapes and the reference's free-text wordings,
including a proxy's embedded JSON error. The field is retried once on the
same credential and, on success, remembered in-process per provider, model
and protocol. Invalid-value refusals and fields every API requires are not
dropped; Anthropic's 400 shape is not treated as this refusal.

Lite's chat decode keeps only Lite IR fields, so client `store`/`metadata`/
`service_tier`/etc. top-level fields are dropped before this layer — the
effective surface today is `parallel_tool_calls` (Chat/Responses, only when
tools are present) and the Responses upstream's hardcoded `store: false`.

## Gemini pending-call pairing and turn shapes (2026-10-18)

The reference gateway answers a Gemini history's function call the client
never answered — an interrupted turn leaves its call pending — with a
synthetic result, because Google's APIs refuse every later turn over the
one (#1560), and it keeps what the user said out of a turn holding tool
responses, which Vertex answers "Requests ending with a model turn are not
supported" (#1533). Lite's Gemini encoder now does both: pending calls get
the reference's note in a user turn before what followed, user text beside
tool results gets a turn of its own, and a history ending with the model's
turn is closed with a "Continue." user turn. An answered history is built
as before; Antigravity's Claude tool_result shape has no Lite surface yet.

## Anthropic-safe tool call ids (2026-10-18)

Anthropic Messages accepts `tool_use.id` and `tool_result.tool_use_id` only
in `[A-Za-z0-9_-]`. A conversation moved from another vendor can contain an
id such as Devin's `Bash:0#...`, which Claude Code drops as unparsable
(#1304). Lite now gives every unsafe id a stateless `mp_` + base64url form
before it crosses into Anthropic and restores the original id on the way
back. Safe ids pass byte for byte, while ids already beginning with `mp_`
are encoded to keep the namespace unambiguous. Request history, non-stream
responses and streamed tool-call starts use the same mapping, so a call and
its result continue to pair.

This ports the reference gateway's `6c099edd` behavior. Real Anthropic API
acceptance remains covered by request-shape tests rather than a live call.

## Upstream refresh, 2026-10-19

The reference checkout moved from `dde3a31a` to `5774c790` (1,198 commits,
1,065 changed files). GUI, plugin hosting, subscription accounts, cloud sync
and backup account for most of it and stay outside lite's scope. The core
gateway and provider changes below are generic and do not depend on a
subscription-account model.

### Left out: assistant turns that say nothing (`ed0335f7`)

An agent keeps the turn a user cut off before it said anything — only its
thinking, an empty text, nothing — and sends it again on every later turn.
DeepSeek, Kimi and the like turn the whole request away over it:
"Invalid assistant message: content or tool_calls must be set" (#2599).
`crates/core/src/chat/silent.rs` now drops such an assistant turn wherever
it stood and joins the user messages it stood between, because some chat
templates refuse two user messages in a row. A prefill (`prefix`/`partial`),
a refusal, an audio reply, a tool call and a non-text part all keep the turn.
A request with no silent turn comes back unchanged. This runs on the Chat
encoder, so it covers both translated and Chat-native requests.

### Empty Chat tool-call ids (`a69cd070`)

A streamed call the upstream left without an id reached the agent as `""`
and a strict upstream then refused every later turn (WorkBuddy AI's 400
code 11133, #2611). `tool_pairing.rs` now gives each such call an id derived
from its message position, call index, name and arguments — never random, so
the next turn sends the same history and the prompt cache still holds. A
tool result that names no id answers the earliest such call in order.
Non-empty ids keep the existing remapping, including the duplicate-id
refusal.

### DeepSeek's reply limit (`62020b9c`)

DeepSeek's Chat API has no `max_completion_tokens` and ignores one, so a
client asking OpenAI's way got DeepSeek's 8K default (#2355). Lite dropped
the field at Chat decode before this change, so the fix is in two places:
`chat::decode_request` now reads it beside `max_tokens` (an explicit
`max_tokens` wins), and `upstream.rs` writes `max_tokens` for
`api.deepseek.com` Chat. OpenAI, Azure and lookalike hosts keep their own
field, and a body that already names `max_tokens` is left alone.

### A relay's own input cap (`7fefbc61`)

`Input token exceed the limit` is a cap on one request's input, not an
unknown shape. `model_gone.rs` now classifies it as too-long so a routing
group can hand the turn to a member with room.

### A bare trace id is a shape refusal (`75035fa2`)

Command Code's Provider API turns a request away with
`{"message":"invalid request error trace_id: <hex>","type":"invalid_request_error"}`
— a 400 (a 422 before) with no code and no param (#2313). The key and the
account are fine, and another member may take the same turn, so
`model_gone.rs` now reads it as a shape refusal: the group moves on and the
member doesn't rest. A provider's name in front and a wrapped copy read the
same; an overflow that names the context length before the trace id, a
message that says more after it, and a 200 do not.

### A Chat tool in another API's shape (`72d6103e`)

A Chat tool is `{"type":"function","function":{name,…}}`. Some clients send
theirs as MCP's `{"name","description","inputSchema"}`, Anthropic's
`input_schema`, a bare `{"name","parameters"}` or Responses' flat
`{"type":"function","name,…}` (#2622). `chat::decode_request` now shapes
those before reading, so a strict Chat upstream receives a function tool
with its name instead of a tool with none. A tool that already has its
function object, a hosted tool, a nameless one and a request with no tools
are unchanged; other fields stay beside the function, and `parameters` wins
over `inputSchema`/`input_schema`.

### A Grok tool pattern Rust's regex can't read (`97b4174a`)

xAI turns away a tool whose schema `pattern` Rust's regex syntax can't
parse: `…pattern: "^[^\\s[\\]]+$" is not a "regex"` (#2317). The pattern is
valid ECMAScript, which is what JSON Schema's `pattern` is — there a `[`
inside a character class is a plain character — but in Rust's syntax it
opens a nested class that never closes; `&&`, `--` and `~~` inside a class
are set operations there too. A Claude Code session on Windows sends
`^[^\s[\]]+$` for a breakpoint's class name and selector, so a Grok model
was turned away on every turn.

`crates/gateway/src/grok_schema.rs` escapes such a character inside a
class, which is the same character to ECMAScript, and `upstream.rs` runs
it for every protocol's request body — a tool schema travels on Chat,
Anthropic, Responses and Gemini alike. It applies when the model named in
the body is Grok's (`grok-*`, `xai/grok-*`, `x-ai/grok-*`) or the provider
is xAI's own API, and every other model's tools go as they came.

The walk is the reference's whitelist: a tool is entered through
`function`, `functionDeclarations`, `parameters`, `input_schema` or a
namespace's nested `tools`, and a schema only through the keywords that
hold another schema, so sample data under `enum`, `const`, `default` or
`examples` is left alone. The reference splices each pattern into the raw
body to keep the rest byte for byte and hold a prompt cache's prefix; lite
re-encodes its body from the IR anyway, and `preserve_order` keeps the key
order, so the rewrite is in the value tree instead. Lite walks that tree
and so also reaches Gemini's `functionDeclarations` and a Responses
namespace's `tools`, which the reference's walker does not.

### Still open from this refresh

Room-aware group selection (`0f398c48`) needs per-model window metadata that
lite does not carry; it is not implemented. Rest granularity, RPM and
concurrency queues, sink ordering, Devin's Gemini schema sanitizer
(`1530c5ef`) and HTTP/2 transport health (`94919109`) remain future work.
The streaming-whole-reply retry (`df341e76`) does not apply:
lite already replays a whole upstream answer as the client's stream and
never asks that member again for the same turn. These fixtures are local
unit, loopback HTTP and workspace tests, not live-vendor or real-agent
evidence.
