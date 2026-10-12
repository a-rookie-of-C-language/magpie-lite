# Core parity tracker

Updated: 2026-10-12. Reference checkout: `D:\code\magpie`; implementation: `D:\code\magpie-lite`.

Target: multiple coding agents sharing one Rust backend, provider adapters for different services, and an interface managing providers, models, routing and usage. The reference rust-core-refactor-plan.md, rust-core-contracts.md and rust-core-admin-api.md are proposals with broader acceptance gates. This tracker does not claim G1–G16 completion or full Magpie product parity.

| Area | Current evidence | Remaining work |
|---|---|---|
| Shared Rust gateway | Core wire codecs; real local HTTP/SSE terminal/error/EOF and after-header disconnect fixtures across four adapters; incremental 32 MiB raw-event bounds, BOM/CR/UTF-8 handling, opening event/metadata-byte budgets, early stream-error key/fallback/group retry, no ordinary fallback after text/reasoning/tool output, configurable per-attempt first-content/read-idle deadlines with timeout fallback, group non-final first-content switching, request-wide Responses opening release, downstream pre-header/mid-answer SSE comments with safe fallback, protocol error conversion, bounded upstream-silence windows and activity-driven cadence, bounded per-channel text/reasoning repetition protection with no answer/tool replay and one same-candidate reasoning-only reask plus bounded Gemini empty-reply reasks with combined client/history usage, and real HTTP/1.1 before/after-header cancellation with persisted accounting; see docs/stream-lifecycle.md | Recorded real-agent fixtures, broader multimodal/tool/stream conformance, queue/slow-final-route behavior, search-result stream IR and cross-client image rendering, provider-specific prefill continuation, optional post-content progress policies, and HTTP/2 cancellation conformance |
| Agent adapters | Shared magpie-agents adapters for Claude Code, Codex, Crush, Gemini CLI, Goose, OpenCode and Zed; private staged preview, revision-checked apply, OS lock, schema-2 journaled selective field restore preserving unrelated edits, disconnect, managed-field drift and explicit recovery. Agent unit, HTTP, sandboxed child CLI and real-gateway console browser tests; Windows ACL/junction checks; recursive duplicate-key JSON/JSONC refusal, format-preserving JSON edits, compact comment-free JSON object additions that preserve single-line layout, native OpenCode/Zed JSONC, shared passive CLI/config discovery with bounded user-prefix/Node-version searches, npmrc prefix and nvm default-alias handling, and PATH/unknown metadata, native absolute config-directory overrides for Claude/Codex/OpenCode/Zed, and seven-Agent selective restore through HTTP/CLI/browser | Native project overrides, login-shell/registry PATH refresh, process/reachability and real-version compatibility evidence; compact layout preservation is intentionally limited to single-line objects without direct comments |
| Provider metadata and keys | Protected provider CRUD and console; providers.rs covers saved/live edits, secret-free capability metadata (`supported_protocols`, nullable streaming/tools/vision), protocol validation and preserve/clear semantics. provider_tools.rs covers four-protocol catalogs/probes, pagination, saved/draft/no-auth credentials and bounded read-only operations | Capability presets, specialized/stream-only probes and paid/live vendor evidence |
| Hot edits and key rotation | ConfigStore saves before publishing; requests own snapshots. Versioned key schedules ignore stale requests. providers.rs checks an old in-flight request after a smaller pool is saved; config.rs checks stale scheduler calls. Durable config revisions/ETags and optional If-Match checks reject stale provider/group writes across restarts; the revision is committed in the atomically replaced TOML document, store and HTTP tests check unchanged disk/memory and a single winner for concurrent checked edits; gateway writers also serialize through a bounded file lock. Provider/group CRUD and agent apply now have durable idempotency with 256-entry retention, token-keyed HMAC digests, restart replay, pending fail-closed behavior, and secret-free ledgers | Encrypted secret storage with recovery; durable idempotency for any future agent rotate endpoint |
| Routing and catalog | Explicit provider/model, listed bare model, default provider, and process-local group `order`/`rotate`/`usage` scheduling with ordered fallback; config unit tests, groups.rs and e2e.rs | Allowance/weight/pace policies, conversation affinity, richer model metadata and dynamic discovery |
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
2. Broaden routing and protocol fixture coverage across the existing four adapters; the initial group `order`/`rotate`/`usage` scheduler is now implemented.
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
native Gemini image rendering; search IR, subscription adapters and
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
search-result IR, CodeAssist subscription wrappers and real-provider/Agent
compatibility evidence remain open.
