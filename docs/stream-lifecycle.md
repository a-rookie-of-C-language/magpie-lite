# Stream lifecycle and failover

Reference: `D:\code\magpie\internal\gateway\stream_error_open_test.go` and
`canceled_usage_test.go`, plus `sse.go` for the reference reader and
`first_token_test.go` / `fallback.go` for first-content handling. This document
describes the Rust gateway's implemented contract, not full Magpie or live-vendor
conformance.

## Terminal events and errors

The four upstream SSE dialects share a single event reader. Explicit errors and
completion markers end the downstream body and release the upstream response;
they do not wait for the upstream server to close its connection. A terminal
error never synthesizes a successful completion marker. Data following the first
terminal event is ignored. Fragmented frames and a complete final `data:` frame
without its trailing blank separator are supported.

Chat recognizes `[DONE]`. A clean EOF after Chat `finish_reason` also completes,
but `finish_reason` alone does not cut off later usage frames. The sentinel is
not a completion marker for Anthropic, Responses or Gemini. EOF without a
protocol completion marker, or an upstream transport read failure, becomes a
sanitized stream error instead of fabricated success. Invalid JSON frames are
currently ignored; this is not a strict SSE/JSON validation contract.

## Incremental parsing and byte limits

The core parser consumes borrowed transport slices through fallible
`feed(&mut remaining)` calls, returning at most one data payload each time.
It retains only the current line and joined data, not a transport-sized copy or
a queue of every frame in a chunk. Consumed bytes are not repeatedly rescanned
or drained from the front of a growing input buffer. One chunk may contain any
number of individually valid events, even when its total exceeds the budget.

LF, CRLF and CR line endings work across arbitrary chunk boundaries. One
initial UTF-8 BOM is ignored; fragmented UTF-8 is joined before decoding, and
invalid UTF-8 is replaced. Data lines join with one newline, only one optional
space after the colon is removed, and colon-less or empty `data` fields produce
an empty payload. Comments and unknown fields do not become data.

The fixed gateway default is **32 MiB per raw event**: all nonempty lines and
their line endings count, including ignored comments/fields; the terminating
blank line does not. The reference reader has a 32 MiB scanner-line limit;
lite additionally limits the aggregate multiline event. This deliberately
rejects oversized multiline events that the reference may accept. An error is
detected while feeding bytes, without waiting for a separator or EOF, releases
the parser buffers, and cannot later flush into a successful payload. The error
contains the limit, not raw upstream data. It uses the existing 502 stream-error
path: early route/group fallback is allowed, extra keys on the same endpoint
are not consumed or cooled down, and content already emitted is never replayed.

This is a raw-event bound, **not a 32 MiB total request-memory guarantee**.
Allocator capacity, joined payload/JSON decoding, protocol state and transport
buffers add overhead. Long streams may contain indefinitely many bounded
events; byte limits are not an elapsed-time deadline.

## Failover before an answer

A successful HTTP status is not sufficient for an SSE attempt. Before handing
it to the client encoder, the gateway holds opening model/role/usage metadata
and reads until one of:

- Nonempty text or reasoning, a tool event, or a stop event commits the attempt.
- A completion marker commits even an empty answer.
- An error closes the upstream immediately and permits retry according to its
  structured status, provided the attempt has not committed.
- 64 decoded opening events, or a 64 KiB retained-metadata estimate, commit the
  attempt as buffering safety bounds. The estimate counts event storage and
  retained model-string bytes; it may overshoot by one bounded event and is not
  an allocator-exact memory cap. Errors after either bound do not retry, even if
  there has been no actual text. Wire-only metadata discarded by the decoder
  does not count toward retained metadata.

Discarded attempts' opening frames are never encoded into the selected answer.
A new attempt gets a fresh upstream decoder; the client encoder is created only
for the selected attempt. An answer is never assembled from two upstreams. In
particular reasoning/tool activity counts as committed even if a particular
client dialect buffers or hides that activity.

Prefetch withholds candidate metadata until handoff. Normally it runs before
downstream headers; a slow streaming request may send only headers/comments and
move the still-pending prefetch into the response body. HTTP commitment does not
commit a candidate. The deadlines below bound a below-budget stall when enabled.
Real HTTP/1.1 fixtures verify socket-close cancellation both before headers and
after a comment, even with both deadlines disabled (see Accounting and
cancellation).

## Streaming deadlines

Two gateway-level root TOML settings apply to streaming requests, and are exposed
in the protected secret-free `/config` snapshot:

- `stream_first_content_timeout_ms = 600000` (10 minutes by default): a fresh
  absolute budget for each concrete attempt, beginning before sending the
  upstream request. It includes HTTP response-header wait, an HTTP error-body
  read, and opening SSE. Nonempty text/reasoning or tool activity satisfies it;
  role/model/usage metadata, empty deltas, ignored chunks and comment heartbeats
  do not restart or satisfy it. A completed empty answer is still valid.
- `stream_idle_timeout_ms = 300000` (5 minutes by default): maximum wait for the
  next upstream body chunk. It applies both before and after content, separately
  from the first-content budget. Transport bytes, including comments and partial
  frames, begin a fresh interval; the first-content budget still prevents an
  endless metadata drip before an answer. Time yielding decoded frames to a
  backpressured client does not count as waiting for upstream transport.

Set either to `0` to disable that limit independently. With both disabled, an
upstream may hold a handler indefinitely while its client stays connected. Nonstreaming requests retain the
previous behavior; these settings are not a total request-duration limit.
Configuration is pinned for the request, and each fallback attempt gets a fresh
clock. The first-content deadline remains active after a metadata safety budget
commits a candidate, but a later timeout can no longer retry that response.
Sending only keepalive headers/comments does not commit a candidate.

Timeouts are endpoint failures, internally classified as 504. They skip any
remaining credentials on that endpoint and never apply quota cooldown. Before
commit they may try an ordinary fallback or next group route; after commit they
produce one terminal protocol error, retain observed partial usage, release the
upstream and never synthesize a success marker or splice another answer. A
header/error-body timeout on the last route returns HTTP 504 in the client's
error envelope if no comment has committed HTTP headers. After a comment it
becomes one protocol stream error. A timeout of an HTTP-200 SSE response uses
the existing client stream-error encoder (the transport status is already 200).

Timers are cooperative and enforced when the handler/event stream is polled;
they are not a background cancellation service and cannot interrupt blocking
CPU work or a downstream that never polls again. Idle time is transport silence,
not a post-content meaningful-progress policy: an endless
heartbeat stream after content can remain live. Group first-content switching
is a separate policy described below; disabling its cap on the final route does
not disable these gateway-wide safety settings.

## Group first-content switching

The group TOML/API field `first_token_timeout_ms` defaults to zero (disabled).
It follows the reference's `Group.FirstToken` slow-start switching policy,
using milliseconds rather than the reference's seconds. It is exposed by the
protected configuration snapshot, group POST/PUT, CLI
`group add --first-token-timeout-ms`, and console editor.

Only streaming group requests enable it, and only for non-final available
candidates in the scheduled order. Each upstream attempt starts one absolute
budget before sending; header latency and held opening prefetch share that same
deadline. Start/usage/empty text, upstream comments and downstream keepalive do
not rearm or satisfy it. When it expires, the held upstream is dropped, the
attempt is internally classified as `group_first_token_timeout`, and routing
moves to the next group candidate without consuming that route's other keys or
placing any key in quota cooldown. Subsequent attempts get their own budget.
Abandoned metadata/usage never appears in the selected answer or final record.

The last scheduled candidate (including a single-member group) is exempt from
the group cap, even when rotate/usage places an earlier configured route last.
Gateway-wide first-content/read-idle clocks still run. Known non-2xx headers
stop this opening policy before reading the error body; global clocks continue
to bound that read. Ordinary provider fallback and nonstreaming requests do not
use the group budget. In-flight requests retain their original configuration.

Prefetch returning commits the candidate: actual text/reasoning/tool output,
terminal events, opening event/metadata byte safety bounds, or a real Responses
opening event released by its request-wide hold budget. Once committed, the
group timer is discarded; a later slow chunk or error cannot trigger fallback.
Global first-content/read-idle limits remain independent. Ready transport
results are preferred over a simultaneously-ready group timer. This is
cooperative cancellation while futures are polled, not a background watchdog.

Thirteen `group_first_token_*` local HTTP tests cover the four-by-four
client/upstream matrix, headers plus opening in one budget, header cancellation,
final/single-route exemption with global bounds, reasoning/tools and metadata
commit, ordinary/nonstream/HTTP-error exclusions, multiple models on one
provider, rotate/usage ordering, pinned hot edits and real socket cancellation
with persisted last-attempt accounting. Paused-clock tests check absolute and
per-attempt budgets, zero/extreme values and ready-result precedence. API and
child-CLI tests cover persistence, editing, defaults and invalid input; the
browser fixture exercises save/reopen/reset and native field validation.
This does not implement the reference's queue or slow-final-route waiting UI,
and does not establish live-vendor, installed-Agent, or HTTP/2 cancellation
compatibility.

## Downstream keepalive

Two more root TOML settings, also pinned per request and exposed in protected
GET `/config`, control SSE comments:

- `stream_keepalive_interval_ms = 15000`: normal comment interval.
- `stream_keepalive_max_silence_ms = 300000`: stop comments after this much
  upstream transport silence. Either setting at zero disables keepalive.

Only Chat, Anthropic and Responses clients receive `: keepalive\n`, with no
extra blank separator. Gemini clients and nonstreaming requests bypass comments.
The reference excludes Gemini because some clients reject SSE comments. The
first comment can send HTTP 200/SSE headers while awaiting vendor headers or
opening metadata. It contains no candidate model, usage or answer. Key/provider/
group fallback can still choose another attempt in that same downstream stream.
A fast failure before any comment preserves the original HTTP error status and
JSON envelope. A later HTTP failure must instead use the client stream encoder,
with exactly one error and no fabricated success. Known non-2xx body reads
suppress comments, even if an earlier header-wait comment has already been sent.

An attempt starts a fresh silence window; response headers and each nonempty
transport chunk renew it, even when the bytes are ignored comments or incomplete
frames. Downstream writes do not renew upstream activity. After handoff,
upstream activity with no downstream output can send a comment after a one-second
gap (capped by the configured interval). Completely quiet streams use the normal
interval. The cap stops comments without closing the request or consuming extra
credentials. New transport activity can resume comments. Configured upstream
deadlines still independently decide whether to fail over or end the stream.

The handler owns one pinned execution future. After a comment the response body
owns that same future, including tracker, HTTP send and opening prefetch; it is
not restarted or spawned elsewhere. A watch channel reports transport activity
without retaining payloads. Once handoff completes, comments are inserted only
between complete encoder output chunks, never inside SSE lines. Biased polling
prioritizes ready reply/body output over a timer so terminal output is not delayed
by a scheduled comment. Dropping either handler or body cancels the owned work.
A heartbeat tick must not recreate an upstream header/read timeout.

These are comments, not protocol events: they do not satisfy first-content,
meaningful-progress or an Agent's event-only idle policy. Responses opening
release is a separate policy described below. Group first-content switching is
documented above; queue and slow-final-route exceptions remain unimplemented; this is not a claim of full
reference keepalive or real-Agent conformance.

## Responses opening-event release

The root TOML setting `stream_responses_hold_timeout_ms = 240000` defaults to
four minutes; `0` disables it. Old configurations inherit the default and the
secret-free configuration snapshot exposes the value. Only streaming Responses
clients enable the policy, regardless of the upstream dialect. SSE comment
keepalive may be disabled independently.

One budget starts per request and is never reset by key, provider or group
fallback. Once it expires, prefetch releases already buffered real Start metadata.
Headers, comments, unknown chunks and usage-only input cannot synthesize a Start.
A Chat assistant role/model opening is decoded once per upstream attempt. Ready
transport events take priority over the release timer, allowing a ready opening
error to retry safely before any candidate metadata is published.

Publishing opening metadata commits the selected attempt, even without answer
content: a later error is delivered once, with selected-attempt accounting and
no replay. Release does not satisfy or restart the absolute first-content
limit. An interrupted upstream read retains its remaining idle budget across
metadata replay without charging downstream backpressure or receiving a fresh
full idle interval. Dropping the downstream still cancels the owned upstream.

Nine `responses_hold_*` local HTTP tests cover four upstream dialects, shared
budgets through retries, late-error commitment, independent upstream deadlines,
missing opening metadata, ignored heartbeats, disabled/non-Responses exclusions,
ready-error precedence and real socket disconnect/persisted accounting. Paused
clock tests cover the default budget, disable/extreme values and retained idle
and first-content deadlines. This implements the request-level opening-release
subset of reference `internal/gateway/fallback.go`. Group first-content switching
is documented separately; queue, slow-final-route and post-content event-idle
policies remain outside this implementation. Local
fixtures do not establish live vendor or installed Agent compatibility.

### Retry classification

Structured numeric HTTP error codes and recognized symbolic error codes are
read from top-level errors, Responses `response.error`, and error events. Common
Anthropic/Responses/Gemini rate-limit, auth, permission, overload, invalid-request
and context-length codes are mapped to HTTP status classes. Free-form message
text is not used to infer quota or refusal categories. Unknown stream errors,
transport failures and truncated streams use 502.

- 401/402/403/429 may try the next credential in the same provider's finite key
  pool, once per slot in that request's rotated snapshot.
- 401/402/403/404/408/429 and 5xx may try a configured fallback route or the next
  group route. Endpoint failures do not spend the remaining keys on that same
  endpoint.
- 402/429 apply the existing hint-aware key cooldown, including `Retry-After`
  and supported body/reset hints; provider cooldown zero still disables it.
- Recognized invalid-request, refusal and context-length errors do not retry or
  cool down keys. Specialized reference heuristics based on prompt length,
  free-form errors, firewall responses or retired models remain unimplemented.

If all applicable candidates fail, the last attempt's error is delivered once
in the client protocol. Retry classification adds no management mutation API.

## Repetition-loop protection

Root TOML `stream_loop_guard` defaults to `true` for new and legacy files; an
explicit `false` disables it. It is exposed in protected GET `/config` and pinned
for the request. Editing the TOML and restarting is currently the user-facing
way to change it; there is no root-settings edit endpoint or console toggle.
Each upstream attempt owns a fresh guard. No background task is spawned.

The guard consumes decoded canonical `TextDelta` and `ThinkingDelta` events,
with independent state for each channel. All four upstream/client protocols use
this path, including both Responses `response.reasoning_summary_text.delta` and
`response.reasoning_text.delta` (and both Chat reasoning aliases). Start, usage,
empty deltas, SSE comments, signatures, tool starts/arguments/ends and other
metadata do not add prose units. Nonstreaming JSON replies never enter the guard.

The reference is `internal/gateway/loop_guard.go` and `loop_guard_test.go`:

- A newline, a space after a letter-bearing sentence ending in `.!?`, or a CJK
  `。！？` ends a unit. Trimmed, lowercased units lose trailing sentence punctuation.
  A unit counts only if it has a word token with a letter and no numeric
  character; standalone braces, digit/hex/version-only lines do not count.
- The rolling channel window holds **256** word-bearing units. If at most
  **24** distinct normalized keys remain, it trips. Keys retain at most the first
  **256 UTF-8 bytes**, without splitting a character. Distinct text beyond this
  prefix is intentionally not distinguished, so this is not exact semantic
  repetition detection.
- An unbroken unit is processed after **4096 bytes** (a final multibyte character
  may cross the bound). A repeated byte period of 1–64 bytes, or at most **8**
  distinct word tokens at sufficient density (`words * 32 >= bytes`), marks a
  run-on unit. **Two consecutive** marked units trip. A short natural break
  clears this run; sufficiently long repetitive line tails preserve it.
- Unit storage, keys and counts are bounded. Lite stores full bounded keys, not
  reference FNV hashes, avoiding hash-collision classification. Character
  classification uses Rust's alphabetic/numeric predicates; Unicode edge cases
  and safe UTF-8 truncation are not claimed bit-identical to Go's letter/digit
  categories and byte truncation. The thresholds and common-text behavior match.

The triggering decoded delta is forwarded **whole**, then one terminal protocol
error ends the selected stream and releases the upstream, including on a final
unterminated SSE frame at EOF. Text later inside that same delta may therefore
be visible; later decoded events are not. No successful completion marker is
fabricated. Content/reasoning has committed the candidate: the guard cannot spend
another key, switch provider/group, or splice/replay an answer. Unlike the
reference's diagnostic quote, Lite's fixed error message includes no generated
text or credentials. The final record uses `upstream_stream_error`, retains only
observed selected-attempt usage, and sanitized JSONL history survives reload.

This is a repetition heuristic, not a no-progress timeout, total-answer limit
or guarantee against false positives. It does not end a heartbeats-only stall
after content. If the first trip is reasoning-only—no answer text and no tool
call—the gateway waits about one second and re-asks the same provider/model/key
slot once, without fallback or rotation. Previously emitted reasoning is kept,
the re-ask's duplicate opening event is hidden from the client, and usage is
combined across both attempts. If the re-ask loops again or fails, the client
receives one protocol error; answer text or tool output emitted before the loop
disables the re-ask path.

Seven unit tests cover chunk boundaries, text/reasoning independence, exact
window/distinct thresholds, eviction, run-on cycles and short-break reset,
Unicode, real source/document negatives and bounded storage. Configuration tests
cover legacy defaults, explicit disable, snapshots and invalid types. Eight
`loop_guard_*` local HTTP tests cover four-by-four protocol combinations,
reasoning aliases, enable/disable, no replay after content, EOF flush, tools/code
and nonstream exclusions, pinned edits and fresh request windows. Interaction
fixtures exercise key/provider/group fallback followed by comments then a loop,
selected-attempt usage and JSONL reload without secrets/reply quotes, and actual
socket cancellation below the loop threshold without a late duplicate record.
These are synthetic local HTTP/1.1 fixtures, not paid/live-vendor, real-Agent or
HTTP/2 conformance evidence.

## Accounting and cancellation

Usage reports are cumulative: later known fields replace earlier values,
including explicit zero; missing fields retain prior known values. Each tracker
retains observed usage across response-body cancellation. Replaying prefetched
frames does not overwrite a newer usage value already observed during prefetch.

The tracker switches provider/model/key slot and clears prior attempt usage
**before** awaiting the next concrete upstream request. The final record belongs
to the selected or last attempted route, never to the abandoned primary. Only
that attempt's reported usage is retained; this is not a billing ledger summing
possibly charged discarded attempts. Unknown usage remains null, not estimated.

The current Axum HTTP/1.1 serving path drops the pending handler when its client
closes the socket. This releases the owned upstream response/send future before
downstream headers, including while awaiting upstream headers, prefetching
opening SSE, or reading a successful nonstreaming or non-2xx response body. No
extra background cancellation task is needed. These assertions cover fully
closed loopback TCP connections, not HTTP/2 stream resets, proxies, TLS or a
client that leaves its connection open but stops reading.

One final record is written for each tracked request. Before downstream headers,
dropping the handler records `client_disconnected`; it preserves usage already
observed during opening prefetch and keeps unknown usage unknown while awaiting
headers or an undecoded JSON body. Fallback selects its accounting route and
clears abandoned-attempt usage before awaiting the next upstream. After headers,
dropping the response body releases the upstream and records
`client_disconnected` with observed partial usage. JSONL stores routing/status
and counters, not credentials, prompts or generated text. Reopening history
preserves those fields.

Per-key attempt counters classify early SSE errors as failed attempts rather
than treating HTTP 200 as success. A stream accepted for handoff counts as a
successful attempt even if it fails later; the final request record still marks
that later failure. Pending-header cancellation has a final request record but
no completed per-key attempt counter for the still-pending response.

## Evidence and limits

`crates/gateway/tests/stream_lifecycle.rs` uses real local HTTP servers with
19-byte fragmentation and held-open response bodies, with 4/16 KiB chunks for
large metadata and default-limit oversize fixtures. It covers four upstream
by four client dialects for terminal/error/EOF behavior, early key failover,
exhaustion, invalid requests and no-retry after text. Additional fixtures cover
reasoning/tool commitment, bounded metadata, auth key exhaustion, every pair of
upstream/fallback dialects through ordinary and grouped routing, and cancellation
with JSONL persistence/reload. Drop guards check that all attempted response
bodies, not merely the first, are released. A BOM/CR-only fixture covers every
upstream/client pair. Default 32 MiB unterminated-frame fixtures cover four
upstream protocols, four client dialects in selected pairs, ordinary/group
fallback, no extra same-endpoint key attempts, and no retry after partial output.
Retained-metadata byte-budget tests distinguish model-bearing Start events from
discarded model metadata on Chat usage-only chunks.

`server::tests::cancelled_pending_fallback_accounts_for_last_attempt_before_headers`
is an explicit handler-abort accounting test, not a real socket test. The five
`socket_disconnect_*` integration tests now add **160 real HTTP/1.1 cases** using
the same `axum::serve(build_router(...))` path as the gateway. They send complete
requests on raw TCP sockets, confirm no response byte or completed log record
arrives, then close only the downstream socket, with both streaming deadlines
disabled. They cover all four client/upstream dialects, streaming and nonstreaming
header waits, opening SSE usage, nonstreaming JSON-body waits, non-2xx body waits
without unintended fallback, and a Chat primary failing over through ordinary/
group routing to each of the four spare dialects while awaiting spare headers. Drop guards verify all attempted upstreams release; records verify the
last model/provider/key slot, no abandoned usage, no extra key attempts, one
secret/prompt-free JSONL line and identical history after reload. Existing
production behavior passes these tests; no separate cancellation mechanism was
introduced. These tests do not establish HTTP/2 or live-Agent compatibility.

`crates/core/tests/stream_accounting.rs` covers cumulative counters and Chat /
Responses error envelope decoding. `stream_status` unit fixtures cover structured
error classification. Core parser fixtures vary every chunk size across supported
line endings, multibyte characters, BOM, empty/multiline data and exact size
limits; they also cover oversized ignored lines, sticky failure and coalesced
frames larger than the per-event budget. These are synthetic protocol fixtures,
not captured real Agent sessions or paid/live vendor compatibility evidence.
Paused-clock unit tests verify absolute first-content accounting across header
and metadata waits, empty-delta handling, independent disable semantics, read-idle
reset and downstream-backpressure exclusion. Additional real HTTP fixtures cover
all four upstream by all four client dialects for opening and post-content stalls;
ordinary/group fallback, first-content caps despite comment heartbeats and after
metadata commitment, paced completion, reasoning/tool first content, disabled
limits, and HTTP header/error-body stalls. Drop guards verify release of every
attempt, and assertions verify one error/record, selected-route usage and no extra
same-endpoint key/cooldown spending.

Thirteen `keepalive_*` real-HTTP tests add cross-dialect delayed headers and
opening metadata, finite key/provider/group retries after comments, fast/late
HTTP failures, header-timeout fallback, known-error suppression before/after
comments, unchanged first-content/read-idle deadlines, post-content silence caps,
ignored upstream heartbeats, one-second active gap, resumption after the cap,
Gemini/nonstream/zero-setting exclusions, and 48 raw-socket disconnect cases
after comments with selected-attempt JSONL persistence/reload. Existing terminal
and fallback tests also run with the default keepalive wrapper. Three paused-clock
unit tests exercise cadence, silence cap, activity resumption, error suppression,
per-attempt reset, independent disable settings and extreme numeric settings.
The policy was compared to local reference `internal/gateway/fallback.go`
(held/quiet keepalive) and `internal/gateway/gateway.go` (translated relay gap).
This evidence is local HTTP/1.1 and synthetic SSE, not a live vendor, TLS/proxy,
HTTP/2 or real-Agent validation.

Queue/slow-final-route behavior, HTTP/2 cancellation conformance and broader
tool/multimodal conformance remain open.
Post-content event-idle/meaningful-progress timeouts are also not implemented;
they are distinct from the repetition guard and transport-idle limits.
