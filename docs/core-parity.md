# Core parity tracker

Updated: 2026-10-20. Reference checkout: `D:\code\magpie` at `79160455`
(30 commits past the prior baseline `b641868e`); `origin/main`
triaged through `d9c6a60d` (fifth pass, PII-only, nothing to port);
implementation: `D:\code\magpie-lite`.

Target: multiple coding agents sharing one Rust backend, provider adapters for different services, and an interface managing providers, models, routing and usage. The reference rust-core-refactor-plan.md, rust-core-contracts.md and rust-core-admin-api.md are proposals with broader acceptance gates. This tracker does not claim G1–G16 completion or full Magpie product parity.

| Area | Current evidence | Remaining work |
|---|---|---|
| Shared Rust gateway | Tool-returned image ownership and four-client/four-upstream translation with a 64-request real loopback JSON/SSE direct/group matrix (docs/tool-transcripts.md); Chat-bound split/parallel/interrupted/orphan tool-history repair with request-local ID normalization (docs/tool-transcripts.md); core wire codecs; real local HTTP/SSE terminal/error/EOF and after-header disconnect fixtures across four adapters; incremental 32 MiB raw-event bounds, BOM/CR/UTF-8 handling, opening event/metadata-byte budgets, early stream-error key/fallback/group retry, no ordinary fallback after text/reasoning/tool output, configurable per-attempt first-content/read-idle deadlines with timeout fallback, group non-final first-content switching, request-wide Responses opening release, downstream pre-header/mid-answer SSE comments with safe fallback, protocol error conversion, bounded upstream-silence windows and activity-driven cadence, bounded per-channel text/reasoning repetition protection with no answer/tool replay and one same-candidate reasoning-only reask plus bounded Gemini empty-reply reasks with combined client/history usage, and real HTTP/1.1 before/after-header cancellation with persisted accounting; a translated stream cut mid-reply is asked of the same candidate to go on from the part of it the client has (the reference's `continuation.go`/`prefill.go`): an Anthropic Messages upstream natively via a trailing assistant message — trailing whitespace trimmed — a Chat one in its vendor's own prefill mode (DeepSeek `prefix` under /beta, Kimi `partial`, loopback/private relays by model name), a full echo of the prefilled text dropped from what the client reads (`unecho`), the tries' usage summed into the reply's, capped at `streamRetries` = 2, and never after a tool call begun, a refusal, a coded error, a group try, or a looped reasoning trip; a 400 naming a prefill ends the reply with what cut it and is remembered per provider+model in `unfit_fields`, and a Chat/Responses client's own conversation ending with the assistant's message is asked of such a model with a `Continue.` user turn after it — never before the refusal names it, never on Anthropic's own prefill (`userLast`/`continueText`, #1447) (`crates/gateway/src/continuation.rs`, `crates/gateway/tests/continuation.rs`, 12 loopback e2e); see docs/stream-lifecycle.md | Recorded real-agent fixtures, broader multimodal/tool/stream conformance, queue/slow-final-route behavior, cross-protocol search-option translation and cross-client image rendering, optional post-content progress policies, HTTP/2 cancellation conformance, the remaining provider-specific rewrites (Cline pinning, Devin's Gemini schema) and queue/slow-final-route conformance |
| Tool-selection policies | Four-dialect auto/none/required/named normalization; reasoning-effort levels (`none`/minimal→low, low/medium/high/xhigh/max, Anthropic budget↔level via the reference's 4096/12000/24000 breaks, Gemini thinkingLevel/thinkingBudget, Anthropic budgets fitted under the client's max_tokens cap) translated in all sixteen source×target directions with no silent drops; Responses/Gemini function subsets; request-local filtering; native-only restrictions; parallel-calls constraints (`parallel_tool_calls` on Chat/Responses, Anthropic `disable_parallel_tool_use` translated to Chat/Responses and rejected on Gemini rather than silently dropped); Responses namespace declarations/calls/tool choices and Responses custom (free-form) tools — decoded as grammar-described input functions, offered to foreign dialects, and restored to `custom_tool_call`/`custom_tool_call_input.delta` events from request-declared tools; custom-tool input is streamed as it is written: function-shaped `{"input":"..."}` deltas are read incrementally across escapes, surrogate pairs and fragment boundaries, deltas that complete whole JSON wait for the upstream's done event so a fuller final value can still be emitted, and `custom_tool_call_input.done` precedes the finished item; namespace declarations/calls/tool choices, streaming namespace preservation, and request-local restoration of flat `ns__tool` names back to Responses namespace calls across Chat/Anthropic/Gemini/Responses upstreams, direct/group routes, JSON/SSE responses and cross-protocol fallback (no historical-name guessing); seven loopback HTTP tests with 576 JSON/SSE direct/group client requests, native compatible fallback without key attempts/cooldowns, search/native-policy bypass regression (docs/tool-choices.md), dedicated namespace-restoration loopback tests in `crates/gateway/tests/namespaces.rs` and a gated loopback custom-input streaming test in `crates/gateway/tests/custom_stream.rs`; Chat-only tools-with-reasoning refusal asked once with `none`, a refused `none` returned as the original tools refusal without a lowest-level retry, endpoint refusals handed to a group's next member without resting the key, and the missing Responses URL named in the agent's error (`wrong_endpoint.rs`, `crates/gateway/tests/tools_effort_none.rs`) | Provider-specific search options, adaptive-only Claude thinking paths, real-Agent and live-vendor evidence |
| Agent adapters | Shared magpie-agents adapters for Claude Code, Codex, Crush, Gemini CLI, Goose, OpenCode and Zed; private staged preview, revision-checked apply, OS lock, schema-2 journaled selective field restore preserving unrelated edits, disconnect, managed-field drift and explicit recovery. Agent unit, HTTP, sandboxed child CLI and real-gateway console browser tests; Windows ACL/junction checks; recursive duplicate-key JSON/JSONC refusal, format-preserving JSON edits, compact comment-free JSON object additions that preserve single-line layout, native OpenCode/Zed JSONC, shared passive CLI/config discovery with bounded user-prefix/Node-version searches, npmrc prefix and nvm default-alias handling, and PATH/unknown metadata, native absolute config-directory overrides for Claude/Codex/OpenCode/Zed, and seven-Agent selective restore through HTTP/CLI/browser | Native project overrides, login-shell/registry PATH refresh, process/reachability and real-version compatibility evidence; compact layout preservation is intentionally limited to single-line objects without direct comments |
| Provider metadata and keys | Protected provider CRUD and console; providers.rs covers saved/live edits, secret-free capability metadata (`supported_protocols`, nullable streaming/tools/vision), protocol validation and preserve/clear semantics. provider_tools.rs covers four-protocol catalogs/probes, pagination, saved/draft/no-auth credentials and bounded read-only operations | Capability presets, specialized/stream-only probes and paid/live vendor evidence |
| Hot edits and key rotation | ConfigStore saves before publishing; requests own snapshots. Versioned key schedules ignore stale requests. providers.rs checks an old in-flight request after a smaller pool is saved; config.rs checks stale scheduler calls. Durable config revisions/ETags and optional If-Match checks reject stale provider/group writes across restarts; the revision is committed in the atomically replaced TOML document, store and HTTP tests check unchanged disk/memory and a single winner for concurrent checked edits; gateway writers also serialize through a bounded file lock. Provider/group CRUD and agent apply now have durable idempotency with 256-entry retention, token-keyed HMAC digests, restart replay, pending fail-closed behavior, and secret-free ledgers | Encrypted secret storage with recovery; durable idempotency for any future agent rotate endpoint |
| Routing and catalog | Explicit provider/model, listed bare model, default provider, and process-local group `order`/`rotate`/`usage`/`weight`/`manual` scheduling with ordered fallback; a `manual` group sends every request to its picked member alone — defaulting to its first route, surviving other routings, cleaned when it leaves the group — while the others and the rules wait; the pick changes on the group card in one click, and shows in CLI, console and `/config` snapshots. Magpie finds a group for one model id served by several providers (listed as `auto/<model as the vendors spell it>`, provider order, retired seats skipped): served and resolved by either spelling without writing anything; a group written with a found id is the user's own, deleting a found one hides it until brought back from its card, the CLI or an HTTP save; a bare model id without a provider in it names its found group too, failing over between its members (single-seat models stay their provider's); vendors spelling one model apart merge on the id they agree on (version dots, gap runs, no vendor prefix or snapshot date; variants, dated pins and releases stay their own), and each vendor is asked for the id it lists; shown with a found marker plus hidden ones last in CLI, console and `/config`; `group auto [on|off]` turns finding off and on again: off, nothing found is listed, hidden or served, while hand-made and changed groups stay, and a request for a found ref goes to its model from the first provider alone (CLI `group auto [on|off]`, console header switch, `PUT /config/found-groups {"on":...}` with revision and idempotency, all without agent reseat). Per-provider key-pool and per-group-route smooth weighted round robin with bounded (0–1000) weights, missing weights defaulting to 1, explicit group-route zero making that route fallback-only, and hourly key token-usage decay; provider/group conversation affinity (`auto`/`session`/`turn`/`off`) over provider key slots, persisted beside the config and editable through CLI, HTTP and console; config unit tests, providers.rs, groups.rs and e2e.rs. A found ref may ask for an effort on itself (`auto/<model>:<level>`, the reference `CanonicalRef` without `[1m]`): the stripped base routes, the ask rides the first try (request beats the member own :level, the group levels still bound it), failover members keep their own, unknown levels 404, and a provider own colon-model or a provider idd auto owning one stays whole (groups.rs `found_group_request_level_round_trip`) | allowance/pace policies, subscription-account affinity (including account effort/allowance reasons), richer model metadata and dynamic discovery |
group routing rules | a group's `rules` put one member first for the requests they match — by request length (`tokens`, the larger of the size estimate and what the vendor counted last), an image carried, a reasoning level (`effort` ≥ low…max, or `on` for any), the agent's UA id (`agents`), a compaction ask (`compact`), hours of the day on magpie's clock (`time.from`/`to` `HH:MM`, past-midnight windows, `days`), and the working folder the agent states (`folders`, or outside them with `exceptFolders`: `~`, `C:\x` and `/mnt/c/x` equal, `\\wsl$\d` shares, boundary matching, case-folded on Windows/macOS; an unknown folder is outside them). The matching rule's member goes first, the members the rules after it also name next (`then`), then the group's order; a `pause` rule instead leaves its member out of the group while its hours, agents or folders hold. A rule decides as a user's turn begins and the requests within it — tool results going back — stay with it, except a conversation grown past that member's window is looked at again (`grown`). A rule with an `intent` waits on what the group's `classifier` model says the turn's message is — asked once as the turn begins through lite itself, never within it or for a compaction; a failed classifier rests 30s and a turn that had an intent keeps it. Only the user's own words are asked about, not what the agent put in beside them. What was asked and said goes on the request's record (`classified`: by, intents, intent, after, cached, ms, error, previous-turn fallback); a failed classifier shows on its group card through `/stats` `classifier_status` (#1929). A group with `effort: auto` has the same classifier judge how hard the turn is — a numbered level ask alongside the intent one, once as the turn begins through lite itself, never within it or for a compaction, and only when the agent asked for reasoning; a group with no rules judges only this (bare). The turn's requests are sent at the pick, fitted for unknown levels (`xhigh`→`high`, `ultra`→`max`, lite knows no per-model levels); while the vendor's cache is warm a lower pick waits and the conversation keeps the higher one it had (`keepsEffort`: cold after 5 min without an answer, lost when the turn compacts under half). What was judged and told goes on the record (`classified`: effort, score, after_effort), and the console shows it on recent requests and `effort: auto` on the group card; CLI `--effort`, a console select, and config validation that refuses other words and demands a classifier. A route ending in `:level` fixes that effort on its member (`mock/model:high` asks for high whatever the agent and the turn pick say, fitted the turn pick's way; rules name the member with the suffix, and a listed model id keeps its own colon). A rule can set the reasoning its member is asked for (`useEffort`, fitted like the turn pick; a member fixed at an effort still sends its own, failover members are unaffected). A group can name the levels it offers (`levels`): every reasoning ask for a member is sent at the nearest offered, a tie going up, except off, which stays off; CLI `--level` repeats per level, config validation cases down and de-duplicates, and the console has no levels field yet. A manual group sends every request to its pick (rules wait, `effort: auto` still judges); the pick can be a bare model id one member has, resolved or refused at write time while a stale stored pick falls back to the first route; `group pick <id> <member>` re-points it, and `group add --pick` makes the group manual. Every upstream send goes on the record too (`tries`: the member asked, at the level the send went at); the console does not render tries yet. `crates/gateway/src/classify.rs` (`EFFORT_LEVELS`, `effort_body`, `read_effort`), `crates/gateway/src/rules.rs` (Rule/TimeWindow/clean, folder_key/folder_under/clean_folder, work_dir, compacting, agent_of UA table, rule_for with per-turn stickiness keyed by group|conversation|first-words, rule_answered), `crates/gateway/tests/classify.rs` (20 unit + 3 classifier-behavior + 3 validation + 10 loopback e2e), `crates/gateway/tests/rules.rs` (`RuleHit.pick/wanted`, `TurnRule.effort/wanted`, `fit_level`, `within_levels`, `keeps_effort`, `turn_effort`) | decision providers and decision groups, the GUI's structured rule editor and rule trace (lite edits rules as data: CLI `--rule` JSON, console JSON array, one-line summaries; the console shows the verdict on recent requests and a failed classifier on its group card) |
| Concurrency and rate limits | Per-key `max_concurrency` slots with an in-turn queue, `queue_limit`/`queue_wait_s` bounds, and a rolling `max_rpm` window, all keyed by the candidate's `who` (provider id plus a key fingerprint) as the reference's `lanes`/`rpms`; a full lane gives way to a same-provider key or same-group member with a slot free (laneMate), a fallback is never asked for want of a slot, a queue/minute refusal is a 429 with Retry-After, and dropped requests give their slot and reserved minute back; `crates/gateway/src/lanes.rs`, `crates/gateway/tests/lanes.rs` (5 loopback e2e) | Agent-visible queue/held-stream semantics, per-account (OAuth) who keys, per-key overrides of provider limits |
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

### DeepSeek's null escape in a tool pattern (`f362d755`)

DeepSeek's schema validator refuses a tool whose `pattern` holds the
ECMAScript null escape `\0`: `Invalid schema for function … is not a
'regex'`. JSON Schema's `pattern` is an ECMAScript regex, where `\0` is the
NUL character, and DeepSeek takes `\u0000` for the same character instead.
Claude Code's Artifact tool carries `^[^\0]*$` (#370).

`crates/gateway/src/deepseek_schema.rs` respells each standalone `\0` in a
tool's schema as `\u0000`, for `api.deepseek.com` alone, and a lookalike
host, every other provider and a pattern that holds none keep their own
spelling. A pair of backslashes (`\\0`) is literal and an octal escape
(`\01`) has its own meaning, so neither is touched.

The walk itself is now shared: `crates/gateway/src/tool_schema.rs` holds
the whitelist walker that the Grok rewrite above and every rewrite below
use, so a tool schema is read the same way wherever its dialect puts it.

### Kimi's untyped enum and locked sampling

Moonshot's "flavored JSON Schema" refuses a tool schema whose `enum` or
`const` names no `type` — 400 "type is not defined" (#886) — and the whole
request fails though the tool is never called. `kimi_schema.rs` writes the
type every value has (string, integer, number or boolean) into each such
schema; mixed values or a `null` among them are left as they were, and
whole numbers among fractions make the type `number`.

Since kimi-k2.5 the server locks `temperature` and `top_p` to one value, so
a client sending its own defaults (VS Code Copilot's chat sends 0.1 and 1)
is refused 400 before any token is read. For `kimi-k2.5` and later, `k3*`
and `kimi-for-coding*`, both fields are left out and the server fills in
the value the model and mode want. `kimi-k2-thinking`,
`kimi-k2-0905-preview` and `moonshot-v1-*` take a range and keep what the
client sent.

Both apply to Moonshot's and Kimi's own hosts
(`api.moonshot.ai`, `api.moonshot.cn`, `api.kimi.com`, `api.kimi.ai`) and
to nothing else; lite has no provider preset, so a relay's Kimi models keep
their own spelling.

### OpenRouter's oneOf (`toolOneOfAsAnyOf`)

OpenRouter serves some models behind xgrammar, which turns a tool schema's
`oneOf` away when its branches are not provably disjoint:
"unsupported schema keyword: oneOf with overlapping or non-provably-
disjoint branches", 400, and takes `anyOf`. `oneof_schema.rs` renames the
key on `openrouter.ai` alone, keeping its place among the schema's keys;
a schema that wrote both has the two merged into one `anyOf`, since the
union is what either said. Every other host keeps `oneOf`.

The reference also names its Cline and Kilo presets; lite has no preset
concept, so only the host OpenRouter itself is rewritten.

### A union at a tool's parameter root (`e9ae12f8`)

Codex desktop's `mcp__codex_app__automation_update` has an `anyOf` at the
root of its parameters. xAI, Anthropic behind a relay, and OpenAI-style
validators may refuse that valid JSON Schema with one of four wordings:
`tool parameter root must be an object type`, Anthropic's
`input_schema does not support oneOf, allOf, or anyOf at the top level`,
OpenAI's `schema must have type 'object' and not have ... at the top
level`, or its `schema must be a JSON Schema of 'type: "object"'`
message. Unrelated invalid-schema errors do not match.

`crates/gateway/src/object_root.rs` folds each tool parameter root to
`type: "object"`. It merges every `allOf` property and required field; for
`anyOf`/`oneOf` it keeps object branches, recursively folds nested branch
unions up to depth eight, keeps only fields every branch requires, and
represents differing branch schemas for one property as `anyOf`, with
identical schemas deduplicated. Local `$ref`s resolve through the root's
`$defs` or `definitions`. The root-only walk covers Chat, Responses
namespaces, Anthropic `input_schema` and Gemini `functionDeclarations`
without rewriting a union nested under a property.

On a 400/422 root-union refusal, the same key is asked once more folded.
A success records the provider/model/protocol in the process-local
`unfit_fields` memory, so later requests start folded; a vendor that
accepts the union receives it unchanged. xAI's own `api.x.ai` host starts
folded. Lite has no provider-preset layer, so the pre-fold is driven by
that host and the remembered refusal rather than a preset. Evidence is
the object-root unit fixtures and `crates/gateway/tests/root_union.rs`
loopback tests, not live-vendor verification.

### A huge error body does not hold the client (`7ac20247`)

`upstream::error_detail` reads a non-2xx upstream body to give the agent a
readable message, and it read the whole thing. A vendor that answers an
error with a page (or a whole reply) held the client while it was read.
The reference's `7ac20247` reads the body for why only once the try is
known to have failed; lite already reads it only on a non-2xx response, so
the remaining gap was the size.

`error_detail` now stops at 1 MiB: a `Content-Length` past it, or a stream
that outgrows it while being read, is dropped and the message is the
status alone (`upstream 400 Bad Request`). A normal error body still gives
its readable message and parsed JSON. The two call sites already run the
read under the configured header timeout, so the cap bounds the work
without changing which responses are errors.

### Grok's whole numbers in tool arguments (`grok_integral.go`)

Grok writes a whole number in a tool call's arguments with a fraction of
zeros: `"yield_time_ms":2500.0`, `"session_id":17277.0`. Codex reads those
fields as `u64`, `usize`, `i32` or `i64` even though their schemas say
`number`, and serde turns the whole call away: "invalid type: floating
point `2500.0`, expected u64" (#1431, through a relay in front of grok-4.6
and grok-4.7). Any client that types its arguments strictly does the same.

`crates/gateway/src/grok_integral.rs` drops the zero fraction of a number
with no exponent: `2500.0` becomes `2500`, `-3.00` becomes `-3`. Other
fractions (`2.5`, `2.50`), exponents (`1.0e3`) and every string, key and
other model's reply stay as they came. JSON has one kind of number, so a
client that took `2500.0` takes `2500`.

The rule is a stream of text, not a parse of each delta: a fraction of
zeros is held until the next character shows whether the number ends
there or goes on, so the arguments are the same however the deltas are
cut. A delta that is nothing but a held fraction is not sent. In
`server.rs` the rule runs on a whole decoded reply (including one replayed
as a stream) and on every streamed tool-call delta before the client
encoder. It applies when either the model the client named or the model the
route sends to is Grok's, so a group route or a relay's upstream name
counts too.

### A Chat-only relay refusing tools with `none` too (`992f288e`)

Some OpenAI-compatible relays serve a model on Chat Completions alone and
refuse function tools when `reasoning_effort` is set: "Function tools with
reasoning_effort are not supported … use /v1/responses or set
reasoning_effort to 'none'". The reference's #176 retry asks once more with
`none`. When the relay refuses `none` as well, naming the levels it does
take ("Supported values are: 'low', 'medium', …"), the reference's
`992f288e` (#2670) stopped asking the lowest level and treating that as a
new answer: the tools refusal is the endpoint refusal it is.

`crates/gateway/src/wrong_endpoint.rs` ports the reference's
`wrongEndpoint`, `asksResponses`, `toolsWithoutEffort` and
`effortLevelsNamed` matchers. `server.rs` now:

- asks a Chat route once more with `reasoning_effort: "none"` when it
  refuses tools with reasoning and names `none`;
- remembers that `none` is refused when that second answer names the
  supported levels, and hands the *original* tools refusal back as the
  answer, rather than the confusing `'none' is not supported` one;
- never asks `none` again for that provider, model and protocol;
- classifies a 400/422 saying the model is served on another endpoint as a
  shape refusal, so a group's next member is asked once and the refusing
  member does not rest;
- appends the reference's `wrongAPINote` to the agent's error when the
  upstream names Responses and this provider has no Responses URL: the
  `/v1/responses` URL to set, not only the vendor's complaint.

`crates/gateway/tests/tools_effort_none.rs` uses the issue's mock relay
shape over real loopback HTTP: a single member asked `[xhigh none]` then
`[high]` twice, a Chat client's relayed request, a group that moves on to
its second member while the first doesn't rest, and a relay that takes
`none` and answers every tools request. `model_gone.rs` and
`wrong_endpoint.rs` carry the wording unit tests.

This covers the translated Chat path, which is where Lite's provider
adapters send every request. The reference's passthrough sibling is the
same code path here, since Lite always rebuilds the upstream body from the
IR. These are local loopback fixtures, not the reporter's real relay or
live-vendor evidence.

### The error a client can act on (`97f264c7`, `writeErrorSaid`)

An agent decides what to do next from an error's *type*, not its message.
The reference gateway's `writeErrorSaid` names that type from the upstream
status in every client shape, and rewrites a conversation that no longer
fits as a 400 `context_length_exceeded`, so pi (and Codex, and Claude
Code) compact instead of stopping. `97f264c7` is the same rule reached
from the OpenAI routes: a 413 must read `request_too_large`, because that
is what pi's compactor looks for.

What lite now says, once the upstream status and body are known:

| status | client-visible type |
| -- | -- |
| 400 | `invalid_request_error` |
| 401 | `authentication_error` |
| 403 | `permission_error` |
| 404 | `not_found_error` |
| 413 | `request_too_large` |
| 429 | `rate_limit_error` |
| 529 | `overloaded_error` |
| anything else | `api_error` |

The same table drives all four client shapes — a Chat or Responses body
carries it in `error.type`, an Anthropic body in `error.error.type`, and a
Gemini body in `error.status`, where a 401 is `UNAUTHENTICATED` and a 413
`INVALID_ARGUMENT` (Gemini's own names, not the shared ones). A Chat or
Responses `code` is the status number, except that an overflow's `code`
is the string `context_length_exceeded` rather than a number, which is
what a compactor reads.

Two messages are rewritten on the way out:

- **An overflow the vendor worded.** A 400/413/422 whose body says the
  conversation is too long (the reference's `tooLongRe`, reusing
  `model_gone::context_overflowed`) leaves as a 400
  `context_length_exceeded` whatever status it arrived as. Anthropic's
  shape also gains a `prompt is too long: ` prefix, which is the phrase
  Claude Code looks for. A refusal about `max_tokens` is the reply's
  allowance and is left alone, as is a paced one.
- **A 413 body-byte limit.** The body is a request-size limit, not the
  model's context, so the message gets the reference's hint: compact the
  conversation, or start a new one, or fewer images and smaller tool
  results. This is the one that reaches agents that only read the type.

The gateway's own refusals (missing token, unknown model, malformed
request) keep the reasons they have always said; only upstream failures
go through this path.

Streamed replies say the same thing. `StreamEvent::Error` gained a
`code: Option<String>`, decoded from the vendor's own `error.code` /
`error.type` (Chat and Responses), `error.type` (Anthropic) or
`error.status` (Gemini, falling back to its numeric `code`). Each client
encoder then writes that code — or, when the message is a worded
overflow, `context_length_exceeded` as a 400 in Chat, Responses and
Anthropic and `400 INVALID_ARGUMENT` in Gemini. A `response.failed` frame
no longer reports its own envelope name (`response.failed`, `error`) as
the code; only a real vendor kind is carried, which is what a client can
act on.

`crates/gateway/tests/error_shapes.rs` is the loopback evidence: a 413
that reads `request_too_large` with the byte-limit hint in Chat, Responses
and Anthropic; a 422 worded overflow that reads 400
`context_length_exceeded`; and an Anthropic overflow prefixed with
`prompt is too long:`. `crates/core/tests/stream_accounting.rs` holds the
stream encoders to the same rule for an overflow and a rate limit.
`crates/core/src/stream_error.rs` carries the wording matcher and its unit
test. These are local loopback fixtures, not live-vendor evidence, and no
agent's compactor has been observed reacting to them.

### The window's reply room (`b749cffb`)

A reply asked for past what the context window leaves is asked again for
what the window leaves, not failed as too long. DeepSeek counts the reply
asked for (`max_tokens`) in its 1M window with the prompt: "maximum
context length is 1000000 tokens. However, you requested N tokens (P in
the messages, C in the completion)". A turn asking the model's whole 384K
on a ~620K+ conversation was refused there and read as "too long for its
model", where every other member of its group answered. An endpoint
speaking Anthropic's API words the same refusal its own way ("input length
and `max_tokens` exceed context limit: P + C > W").

`crates/gateway/src/reply_room.rs` reads the window and the prompt from
that refusal, in DeepSeek's/vLLM's words or Anthropic's, and asks the same
candidate once more for `W-P`: same provider, same model, same key, no key
cooldown and no route switch. The reply field is lowered in the protocol's
own words (`max_tokens`, `max_output_tokens`, or
`generationConfig.maxOutputTokens`), or set where the body named none; an
Anthropic thinking budget above the new cap comes down with it. Room under
8192 is left alone, so the agent is told it overflowed and compacts.
`crates/gateway/tests/reply_room.rs` is the loopback evidence, in
DeepSeek's and Anthropic's wordings and with a prompt leaving under 8K.

### Kimi `$ref` targets (`129d8b08`)

Moonshot expands a local `$ref` in a tool schema and refuses a target that
names no type ("detected infinite recursion without termination
condition"). Codex sends zod `$defs` whose `oneOf` or `allOf` targets are
untyped, so the whole request failed though the tool was never called.
`kimi_schema::kimi_tool_ref_types` gives each untyped target the type its
enum, keywords or branches imply, or every JSON Schema type when they say
nothing. A `{}` target, an `anyOf` (which Moonshot refuses a type beside),
and a `$ref` with only notes beside it are left as they were. It runs on
Kimi's hosts only, on every upstream dialect.

### Affinity written behind the answer (`a1118187`)

A conversation's answerer is in memory at once, but `affinity.json` now
follows within 2s: what is answered meanwhile goes in one write, instead
of marshalling, writing and renaming up to 512 conversations on every
answer. The gateway flushes what waits on Ctrl-C, so a restart or a
handover reads every answer.

### A member too small for the request (`0f398c48`)

A group member whose model window is known and cannot hold the request is
left out before anything is sent. Asked, it would only refuse it — and a
refusal that doesn't say why reached the agent as the vendor's error while
the next member had room, spending a request against the member's limits.

Providers now carry `contexts: { model = window_tokens }` (`--context
model=window` on `provider add`, a textarea in the console). The request is
estimated the reference's way — four bytes a token over text, tool
arguments and schemas — and `roomy` drops every route whose known window is
no larger. It leaves the order alone when a window is unknown, when only one
route is left, or when no route holds the request (the first is asked and its
overflow told to the agent, to compact). A conversation kept on a small
member by affinity moves once it outgrows it, because the affinity decision
is re-pointed as routes are dropped. It is a check before sending, not a
retry; the reply-room reask still covers the window the estimate misses.

### Left out from this refresh

`2a27fb8d` (byte-slice search lookups), `ec6fba47` (a memoized JSON member
splitter) and `9b72add5` (a Go zstd coder pool) are Go-specific
allocations: lite already decodes each client body once and has no zstd
encoder. `627e8e7d` caches Go's catalog-wide group builds; lite's groups
are a small `Vec` scanned in place. `224ea7d5` invalidates that same Go
catalog cache. `b641868e` only fixes a Go test's clock.
### Still open from this refresh

Rest granularity, sink ordering, Cline pinning and HTTP/2 transport health
(`94919109`) remain future work. RPM and concurrency queues were ported
(`crates/gateway/src/lanes.rs`), and Devin's Gemini schema sanitizer
(`1530c5ef`) rides on the Gemini protocol rather than a host
(`crates/gateway/src/gemini_schema.rs`): a list type becomes one
type/`nullable`/`anyOf`, an array with no items gets `{}`, an untyped enum
takes its values' type, and a root with no type is an object. Cline
pinning and the Qoder mover minimum (`dd70c5e6`, `d44d81d9`) do not map
onto lite: it has no presets and no plugin movers.

### Upstream refresh, second pass (`dd70c5e6..afa5f74b`)

`d5bc9f00` is ported: two keys of one provider answering one request with
the same plain 403 — one that names no quota, credit, rate, restriction or
refusal — is the request refused, not each key (`plain_forbidden` in
`model_gone.rs`, the `forbade` set in `send_with_fallback`). The pool stops
after the second alike answer so one refused request cannot walk every key
into cooldown. Lite has no account/windows rest ladder, so the commit's
first half (a plain 401/403 never infers a window rest) has nothing to
attach to: lite only cools keys down on 402/429. The remainder — GUI model
chips/filters (`084c2275`, `a77ecbf8`, `f7337836`), group window reporting
to Codex (`be6a74f1`, lite writes no per-model `context_window` into agent
configs), image-generation timeout setting (`197849ad`, no draw endpoints
in lite), plugin pick persistence (`8febe3a5`), Cline pinning (`dd70c5e6`),
the Factory mover minimum (`afa5f74b`) and the Qoder mover minimum
(`d44d81d9`) — has no lite surface: no presets, plugin movers, GUI or
drawing routes exist to port them to.

### Upstream refresh, third pass (`afa5f74b..94df2392`)

`c8639043` is ported: Command Code's Zero Data Retention refusal — the
error code `cmd_zdr_no_providers` on a 400/422 — is the request's setting,
not the model or key failing (`zdr_refused` in `model_gone.rs`, counted in
`route_retryable_status`). Lite's 422 was not route-retryable before, so a
group now asks the next member; the refusing provider's other keys are
skipped as a matter of course since 400/422 are not key-retryable either.

`94df2392` is ported: the gateway answers as an Ollama server —
`GET /api/version` (0.12.6), `GET /api/tags` (the catalog's ids as
`model`), `POST /api/show` (`model` or the pre-0.5 `name`; `capabilities`,
`general.basename`, `magpie.context_length` from the provider's known
window, 32768 without one) — so a client that takes a custom endpoint only
as Ollama (VS/VS Code Copilot BYOK) lists and chats with lite's models.
The routes sit behind the same bearer token as every other route. An
Ollama client's chat already works on `/v1/chat/completions` unchanged.

`b0b07642` (a GUI rule editor fix) maps onto lite's console only in wording: lite edits rules as a JSON array, so a bare rule is refused on save naming the rule and what it needs (matching the API's `needs a condition` / pause `hours, days or agents only` messages), and removing a rule is deleting its object from the array.

The last does not map onto lite as it stands: lite's client asks for
HTTP/1.1 (`reqwest` is built without its `http2` feature), so there is no
shared HTTP/2 connection to a vendor that can go silent. The
streaming-whole-reply retry (`df341e76`) does not apply:
lite already replays a whole upstream answer as the client's stream and
never asks that member again for the same turn. These fixtures are local
unit, loopback HTTP and workspace tests, not live-vendor or real-agent
evidence.

### Upstream refresh, fourth pass (`94df2392..79160455`)

Of the 16 commits past `94df2392`, the found-group core (`81a24cf8`)
is ported: groups listed as `auto/<model>`, a bare id reaching its
found group, vendors spelling one model apart merged on the id they
agree on, the off switch with first-seat stand-in, and the CLI, HTTP
and console switch. Its remaining bits — agent SyncCatalog rewrites,
key model lists, the GUI Routing page, agent reseat, codexcat — have
no lite surface: lite writes no agent model fields keyed by found
ids, holds no per-key model lists, and reseats nothing.

The rest has no lite surface either: Hermes `${VAR}`/`.env` key
expansion (`b9d1cd36`, Hermes agent unsupported); a console reading
the serving magpie's trace (`ef655acb`, no second process); the
TestMaxRPM clock hold (`2c3c9039`, test-only — lite's lane tests
sleep with wide margins and have never gone red); relayed Responses
item ids (`d155ab16`, lite never relays byte for byte — its encoder
assigns each item one id, as the reference's translated path does);
the rule agent picker (`e5b57794`, no GUI rule editor); ZCode's
native web search switch (`e4ac5e87`, ZCode agent unsupported); the
readme sponsor spacing (`c12ba4fb`, upstream doc-only); the Windows
updater antivirus hold (`40dee517`, no updater); Ollama decision
models through System One (`9588501c`, no decision endpoint or
Ollama presets — lite's classifiers ask as ordinary chat); the Mac
login-item note (`5e30d5b5`, desktop app surface); the plugin GitHub
list (`fb101b0d`, no plugin system); Antigravity's retired-model
display names (`16327872`, `retired.rs` is id-keyed and keeps no
vendor display names); the Codex ChatGPT-account relay retry
(`5f73537f`, no account relay path — overload retry and fallback
live in the IR routing every request takes); dsh's quarter-window
reply cap (`822f2b30`, `agents.rs` writes no per-model output caps);
RTK no-hook tooltip links (`79160455`, plugin library UI strings).

### Upstream refresh, fifth pass (`79160455..d9c6a60d`)

10 commits, all PII redaction and the local privacy-filter sidecar
(`ffa292c7` native `pf-serve`, `c0dad80d`/`07ecdda1` local-model find
and Settings Privacy UI, `3806014a` Chinese-name masking): no gateway
routing, group, effort or catalog surface, so nothing to port. Lite
has no masking pipeline, plugin system, updater or desktop Settings;
subscriptions, cloud sync and platform integrations stay out of scope.
