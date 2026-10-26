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
not a completion marker for Anthropic, Responses or Gemini. Anthropic/Responses
EOF without a completion marker is an error. Gemini clean EOF after an answer
completes, while an empty Gemini EOF enters the bounded re-ask policy below.
Transport read failures never become empty-success retries. Invalid Gemini JSON
data frames are explicit errors (including a misplaced Chat sentinel); other
protocols still ignore invalid JSON. This is not strict wire-schema validation.

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

- Nonempty text or reasoning, native opaque reasoning, a tool event, or a stop
  event commits the attempt. Reasoning-item-start metadata alone does not.
- A completion marker commits an empty answer for non-Gemini protocols. Gemini
  empty completions are intercepted before Stop/Done reaches the client.
- An error closes the upstream immediately and permits retry according to its
  structured status, provided the attempt has not committed.
- 64 decoded opening events, or a 64 KiB retained-metadata estimate, commit the
  attempt as buffering safety bounds. The estimate counts event storage and
  retained model-string and reasoning-item-ID bytes; it may overshoot by one bounded event and is not
  an allocator-exact memory cap. Errors after either bound do not retry, even if
  there has been no actual text. Wire-only metadata discarded by the decoder
  does not count toward retained metadata.

Discarded attempts' opening frames are never encoded into the selected answer.
A new attempt gets a fresh upstream decoder; the client encoder is created only
for the selected candidate. An answer is never assembled from different providers
or models; bounded same-candidate empty-reply and reasoning re-asks preserve
earlier reasoning while keeping one client response. In
particular reasoning/tool activity counts as committed even if a particular
client dialect buffers or hides that activity.

Prefetch withholds candidate metadata until handoff. Normally it runs before
downstream headers; a slow streaming request may send only headers/comments and
move the still-pending prefetch into the response body. HTTP commitment does not
commit a candidate. The deadlines below bound a below-budget stall when enabled.
Real HTTP/1.1 fixtures verify socket-close cancellation both before headers and
after a comment, even with all three streaming deadlines disabled (see Accounting and
cancellation).

## Streaming deadlines

Three gateway-level root TOML settings apply to streaming requests, and are exposed
in the protected secret-free `/config` snapshot:

- `stream_first_content_timeout_ms = 600000` (10 minutes by default): a fresh
  absolute budget for each concrete attempt, beginning before sending the
  upstream request. It includes HTTP response-header wait, an HTTP error-body
  read, and opening SSE. Nonempty text/reasoning, native opaque reasoning or tool
  activity satisfies it; role/model/usage and reasoning-item-start metadata,
  empty deltas, ignored chunks and comment heartbeats
  do not restart or satisfy it. Completed empty answers remain valid for
  non-Gemini protocols; empty Gemini completions use the bounded policy below.
- `stream_idle_timeout_ms = 300000` (5 minutes by default): maximum wait for the
  next upstream body chunk. It applies both before and after content, separately
  from the first-content budget. Transport bytes, including comments and partial
  frames, begin a fresh interval; the first-content budget still prevents an
  endless metadata drip before an answer. Time yielding decoded frames to a
  backpressured client does not count as waiting for upstream transport.
- `stream_progress_timeout_ms = 0` (disabled by default): maximum wait for new
  real decoded content after content has begun. It starts at the first text,
  reasoning, sealed reasoning, nonempty signature, tool, image or search event.
  Only later real content re-arms it. Start/role/model/usage/thinking-start
  metadata, empty deltas, ignored SSE comments and partial transport frames do
  not. The timer runs only while waiting for the next upstream chunk; downstream
  backpressure does not consume it. It applies only to streaming requests.

Set any to `0` to disable that limit independently. With all three disabled, an
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
The post-content progress timeout is terminal at its committed stream: it never
continues, falls back, changes keys, replays or performs a reasoning-only
re-ask, even though ordinary first-content/read-idle timeouts keep their
existing pre/post-commit routing rules.

Timers are cooperative and enforced when the handler/event stream is polled;
they are not a background cancellation service and cannot interrupt blocking
CPU work or a downstream that never polls again. Idle time is transport silence.
When the progress limit is enabled, an endless heartbeat stream after content
cannot remain live forever: only real decoded content re-arms that deadline.
Group first-content switching
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
Gateway-wide first-content/read-idle/progress clocks still run. Known non-2xx headers
stop this opening policy before reading the error body; global clocks continue
to bound that read. Ordinary provider fallback and nonstreaming requests do not
use the group budget. In-flight requests retain their original configuration.

Prefetch returning commits the candidate: actual text/reasoning/tool output,
terminal events, opening event/metadata byte safety bounds, or a real Responses
opening event released by its request-wide hold budget. Once committed, the
group timer is discarded; a later slow chunk or error cannot trigger fallback.
Global first-content/read-idle/progress limits remain independent. Ready transport
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
configured progress or an Agent's event-only idle policy. Responses opening
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

Publishing opening metadata commits the selected candidate, even without answer
content: a later error is delivered once, with selected-candidate accounting and
no ordinary fallback/replay. The reasoning-only loop re-ask below is limited to
the same candidate. Release does not start or restart the absolute first-content
limit or the post-content progress limit. An interrupted upstream read retains
its remaining idle and progress budgets across metadata replay without charging
downstream backpressure or receiving a fresh full interval. Dropping the
downstream still cancels the owned upstream.

Nine `responses_hold_*` local HTTP tests cover four upstream dialects, shared
budgets through retries, late-error commitment, independent upstream deadlines,
missing opening metadata, ignored heartbeats, disabled/non-Responses exclusions,
ready-error precedence and real socket disconnect/persisted accounting. Paused
clock tests cover the default budget, disable/extreme values and retained idle
and first-content deadlines. This implements the request-level opening-release
subset of reference `internal/gateway/fallback.go`. Group first-content switching
is documented separately; queue, slow-final-route and Agent event-idle policies
remain outside this implementation. Local
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
  cool down keys. Context-length errors are recognized by wording alone
  (`contextOverflowed` in `crates/gateway/src/model_gone.rs`, the
  reference `tooLongRe` table): a 400/413/422 whose words say the
  conversation no longer fits — never 429, a `max_tokens` allowance, or
  paced traffic ("Too many tokens, please wait") — stays with the route
  it started on. No other member fits the conversation either, so the
  backup is not asked, the agent gets the vendor's wording at its
  original status, and the member is not retired or cooled down; the
  agent should compact the conversation itself. Reference heuristics
  based on prompt-length estimation remain unimplemented; retired-model
  detection and edge-
  firewall block pages are implemented (`crates/gateway/src/model_gone.rs`,
  `crates/gateway/src/edge_block.rs`): 410/retired-wording and model-refusal
  errors let the next route try, and a firewall block page falls back at any
  status while reporting the block in plain words instead of the page. A
  model the vendor retires is additionally retired from that provider's
  catalog for a week (`crates/gateway/src/retired.rs`): it leaves
  `/v1/models` and bare-name routing until the vendor answers it again or
  the week passes; an explicit `provider/model` request is still sent.
- A request a vendor's API cannot read — xAI's 422 "unknown item type",
  OpenAI's "Unknown parameter" — is worth handing to the next route once
  (`shapeRefused` in `crates/gateway/src/model_gone.rs`): the shape, not
  the vendor, is at fault, so the member doesn't rest and the next
  request asks it first again. Missing-required-field and context-length
  errors stay non-retryable (context-length wording above).
- A vendor that refuses the conversation outright — WorkBuddy's 400
  "Illegal API invocation from an unapproved channel" — or whose
  response-protection layer is unavailable — the ChatGPT backend's 502
  "response protection is unavailable" — is also worth one next-route
  try at any status (`promptRefused` / `protectionRefused` in
  `crates/gateway/src/model_gone.rs`). Neither describes a broken
  credential or a busy endpoint, so the route never rests: the next
  request asks it first again, and a 502 protection refusal is
  explicitly not a key-cooldown busy outage. Solo, the vendor's error
  reaches the agent at its original status.
- A refusal discovered mid-reply — the route committed HTTP 200 and
  started its stream, then its error event carries the vendor's error —
  keeps the full classification: the raw error body is retained beside
  the derived status, so prompt and protection wording still falls over
  to the next route at any status, and overflow wording still stays.

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

The triggering decoded delta is forwarded **whole**, then (except for the one
reasoning-only re-ask described below) one terminal protocol error ends the selected stream and releases the upstream, including on a final
unterminated SSE frame at EOF. Text later inside that same delta may therefore
be visible; later decoded events are not. No successful completion marker is
fabricated. Content/reasoning has committed the candidate: the guard cannot spend
another key, switch provider/group, or splice/replay an answer. Unlike the
reference's diagnostic quote, Lite's fixed error message includes no generated
text or credentials. A terminal loop/failure record uses `upstream_stream_error`
and retains observed selected-candidate usage (including its one allowed re-ask).
A successful re-ask records `ok`. Sanitized JSONL history survives reload.

This is a repetition heuristic, not a no-progress timeout, total-answer limit
or guarantee against false positives. It does not end a heartbeats-only stall
after content. If the first trip is reasoning-only—no answer text and no tool
call—the gateway waits about one second and re-asks the same provider/model/key
slot once, without fallback or rotation. Previously emitted reasoning is kept,
the re-ask's duplicate opening event is hidden from the client, and usage is
combined across both attempts in the **client's usage and request history**.
Each attempt's cumulative reports replace that attempt's known counters; the
first attempt's counters are added once, preserving unknown fields and saturating
on overflow. Prefetched reports are not counted again when replayed.
If the re-ask loops again or fails, the client receives one protocol error;
answer text or tool output emitted before the loop disables the re-ask path.

The one-second pause does not consume the new attempt's first-content budget.
A fresh clock then covers response headers and opening prefetch together;
transport read-idle applies to its body. Keepalive activity resets for this
attempt and comments are suppressed once non-2xx headers are known. Completed
re-ask attempts update the same key's acceptance/error counters but never advance
key/group scheduling, set cooldowns, or spend another fallback route. Dropping
the downstream stream owns cancellation of the pause, send future and response,
with one final `client_disconnected` record and all usage already observed.

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
The `reasoning_only_loop_reasks_*` fixture checks one logical completion,
client/history usage totals, identical resent request bodies and key counters
across all 16 protocol pairs.
Additional `reasoning_reask_*` fixtures cover a second loop or early stream
error across all pairs, second-request header/opening/error-body deadlines,
grouped 401/429/500
rejections without fallback/cooldown, and real socket disconnects during the
retry pause and second-request header wait with JSONL reload and no late request
or duplicate record. These are synthetic local HTTP/1.1 fixtures, not paid/live-
vendor, real-Agent or HTTP/2 conformance evidence.

## Gemini empty replies

Reference: `D:\code\magpie\internal\gateway\gemini_empty_reply_test.go`,
`continuation.go`, `fallback.go`, and `codeassist.go` in the same local checkout.

Only Gemini upstreams use this policy; the client's wire dialect does not select
it. An empty STOP, an ordinary stop reason with only thinking, or clean EOF with
no candidate/answer is not successful completion. Nonempty ordinary text, tool
calls, non-thought image data and grounding search activity count as answers;
thinking and draft/thought images do not. MAX_TOKENS and filter finishes are
exempt. Unknown finish reasons map to filter, matching the reference; an empty
finish-reason string is not a completion marker. Prompt-feedback blocks never
trigger an empty retry, but dedicated refusal rendering is still missing (a
candidate-less block currently ends as a generic incomplete-stream error).

Before commitment, the same provider/model/key is re-asked once after one second.
Exhaustion can spend another configured key or fallback/group route. Once an
attempt commits, ordinary streams allow at most two empty re-asks with one- and
two-second pauses; group streams allow at most one. A held retry already spent on
the selected candidate counts toward that budget, and an exhausted held failure
does not gain extra retries when handed to the relay. Committed continuations
never rotate keys, fall back, or set quota cooldowns. The reasoning-only loop
re-ask has an independent one-retry budget; mixed paths remain bounded.

Empty Stop/Done markers are withheld. Thinking already emitted is preserved,
re-ask opening events are suppressed, and deferred usage is retained as bounded
counters, not an unbounded frame list. Same-candidate usage accumulates in both
client results and request history, even when the last try has no usage report.
Changing candidate discards abandoned usage, just like ordinary failover.
Streaming re-asks reuse the original configuration/conversation/key snapshot,
with fresh header/opening and read-idle clocks after each pause. Disconnecting
cancels the pause or owned upstream work; no detached retry runs later.

Nonstreaming Gemini replies use the held one-retry policy and routing rules, with
combined same-candidate usage and an HTTP 502 on exhaustion. Vendor errors,
invalid JSON, transport resets and timeouts retain their own error behavior;
they are not classified as empty replies. Nonstreaming requests still have no
new total/header/body deadline from this feature.

Eighteen new local HTTP fixtures cover the four client dialects, literal empty
STOP/thinking/no-candidate EOF, held/committed/group limits, key/provider/group
fallback in streaming and nonstreaming modes, length/filter/tool/image/search
negatives, independent loop budgets, HTTP failures without committed fallback,
config snapshots, client/history usage (including missing final usage), header
deadlines and real-socket cancellation. Updated EOF and sentinel fixtures keep
clean answered EOF separate from JSON/transport errors. Classifier unit tests
and core finish-reason tests cover bounded state and stop/filter mapping.
At that increment, image/search classification prevented replay without adding
image/search events to the stream IR. Native image and search-result support are described below. These are synthetic loopback HTTP/1.1 tests, not CodeAssist
subscription, paid/live Gemini, HTTP/2 or real-Agent conformance evidence.

## Accounting and cancellation

Usage reports are cumulative: later known fields replace earlier values,
including explicit zero; missing fields retain prior known values. Each tracker
retains observed usage across response-body cancellation. Replaying prefetched
frames does not overwrite a newer usage value already observed during prefetch.

The tracker switches provider/model/key slot and clears prior attempt usage
**before** awaiting the next concrete upstream request. The final record belongs
to the selected or last attempted route, never to the abandoned primary. Only
that attempt's reported usage is retained across ordinary fallback; this is not
a billing ledger summing possibly charged discarded candidates. Bounded
same-candidate reasoning/empty-reply re-asks are the exception: their attempts
contribute to client/history totals. Unknown usage remains null, not estimated.

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
arrives, then close only the downstream socket, with all three streaming deadlines
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
reset and downstream-backpressure exclusion. Progress-clock fixtures cover
content-only re-arming, ignored metadata/heartbeats, earliest-deadline selection,
downstream-backpressure exclusion and independent disable semantics. Additional
real HTTP fixtures cover all four upstream by all four client dialects for
opening and post-content stalls; ordinary/group fallback, first-content caps
despite comment heartbeats and after metadata commitment, paced completion,
reasoning/tool first content, disabled limits, post-content progress expiry
without continuation/fallback, progress renewal by real content, pre-content
exclusion, nonstream exclusion, socket disconnect during a pending progress
timer, and HTTP header/error-body stalls. Drop guards verify release of every
attempt, and assertions verify one error/record, selected-route usage and no extra
same-endpoint key/cooldown spending.

Thirteen `keepalive_*` real-HTTP tests add cross-dialect delayed headers and
opening metadata, finite key/provider/group retries after comments, fast/late
HTTP failures, header-timeout fallback, known-error suppression before/after
comments, unchanged first-content/read-idle/progress deadlines, post-content silence caps,
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
tool/multimodal conformance remain open. The configurable progress timeout is a
gateway-local decoded-content policy, not reference event-idle parity; Agent
event-idle policies and queue/slow-final-route behavior remain distinct and
unimplemented.

## Safety filtering and refusal routing

Chat `content_filter`, Anthropic `refusal`, Responses
`incomplete_details.reason: content_filter`, and Gemini safety finishes preserve
a distinct content-filter stop rather than a normal stop or token limit. Responses
filter/length streams finish with `response.incomplete`. Gemini prompt-feedback
blocks can omit model/candidates; feedback with candidate content preserves it.

An explicit policy-error envelope or an allowlisted filter with no answer, while
the response is still held, skips the remaining keys for that route and tries only
configured fallback/group routes. It does not trigger empty-reply recovery,
credential retry, or key cooldown. Exhaustion returns HTTP 400 with the sanitized
message `safety filter: upstream refused the request`; terminal history uses
`refusal`. Reported usage is retained even if ordinary reply fields are missing.
Successful fallback retains only the successful attempt's usage, as with ordinary
fallback. After answer output, a filter preserves the partial answer and filtered
finish, without replay. If reasoning or released metadata has already committed
the stream, an answerless refusal becomes one stream error, not another route.

The routing allowlist is `refusal`, `content_filter`,
`content_policy_violation`, `SAFETY`, `PROHIBITED_CONTENT`, `BLOCKLIST`,
`SPII`, and `IMAGE_SAFETY`. Other Gemini filter reasons remain filtered codec
results but do not automatically trigger refusal routing.
`BLOCK_REASON_UNSPECIFIED` is not a prompt block and does not hide a candidate
refusal or disable ordinary empty-reply recovery. Explicit policy error codes
are classified independently; arbitrary messages mentioning safety are not.

Nonstream success bodies are decoded inside routing; malformed replies now
return HTTP 502 instead of a fabricated empty HTTP 200. Loopback fixtures cover
all four upstream/client protocols, same-frame content/filter, prompt feedback,
policy-error envelopes, usage, fallback, and post-commit behavior. This is not
live-vendor validation. Lite still commits reasoning earlier than the reference;
longer reasoning holding, broader multimodal rendering and HTTP/2 evidence remain
open. Usage arriving after a terminal refusal is not guaranteed to be read.


## Native Gemini image content

`StreamEvent::Image` carries an IR image part. Gemini reply decoding preserves
nonempty final inline data or file references, including MIME type and thought
signature (camelCase and snake_case inputs). Thought/draft images and empty
attachments do not become visible answers. Empty text in an image part does not
mask its attachment. Native Gemini encoding flushes any pending tool before the
image, retaining the upstream text/tool/image order. Nonstreaming replies apply
the same final-versus-draft filtering.

The shared content/answer predicates count images for first-content timing,
stream commitment and no-replay decisions. An image followed by a refusal or
reasoning loop cannot be retried through another key/provider/group or re-asked
as a reasoning-only answer. Loopback tests cover both first-content deadlines,
post-image safety/loop termination, socket cancellation and retained usage.
Request history stores neither image data nor thought signatures. The existing
32 MiB raw SSE-event bound still applies; this does not introduce image fetching.

Only native Gemini stream output renders image events in this increment. Chat,
Anthropic and Responses stream encoders omit them, so image-only replies to
those clients do not gain an image presentation. Search-result support is described
below; subscription wrappers and paid/live multimodal compatibility remain unverified.


## Upstream web-search result contract (2026-10-12)

A nonempty Gemini grounding query/source or Responses search action becomes a
search IR event. It is answer content: it satisfies first-content clocks and
prevents unsafe fallback/re-ask after output, without changing the stop reason
to client tool use. Empty/malformed public grounding does not count as an answer
or prevent later valid search updates. Responses open_page/find actions
are not translated as searches.

Anthropic encoders close pending blocks and emit paired server search call and
result blocks; their public sources have empty encrypted_content, never foreign
vendor ciphertext. Gemini encoders flush pending tools before grounding metadata
and preserve original native attribution/citation metadata. Responses encoders
close pending content, allocate distinct output indices/IDs and retain completed
search items in the final response output. Chat encoders omit search events.
Queries and source URLs are not persisted in sanitized request history.

Cross-protocol citation retention is now implemented: Anthropic
`citations_delta` events and Responses `response.output_text.annotation.added`
events enter the IR and translate between the two dialects. See
docs/core-parity.md for the exact contract; Chat and Gemini remain
presentation-free for citations as they are for searches.

Fixtures cover both upstreams and all three supported output protocols in JSON
and SSE, plus deadlines, refusal/loop no-replay and socket cancellation. The
Responses disconnect fixture includes optional opening usage from a compatible
upstream; ordinary missing/null opening usage produces no fabricated counters.
Cross-protocol configured search-option translation, Responses open_page/find,
cross-provider citations, Chat presentation, subscription
wrappers and live-provider/real-Agent conformance remain outside this increment.


## Built-in search request routing (2026-10-12)

Search availability is carried in the request IR independently of function
calls and survives streaming/nonstreaming requests and native Gemini empty-reply
reasks. Native options/choices are preserved. Bare intent can cross protocols;
configured restrictions or forced choices that cannot be represented fail
before upstream HTTP. The route loop may try a compatible configured fallback,
including group routes, but does not retry keys or apply credential cooldown for
this local encoding error. With no compatible remaining route the result is
HTTP 400. Reask encoding errors propagate without restarting route fallback.

Global NONE (Anthropic/Responses) suppresses search; function-only NONE
(Chat/Gemini) does not. Translating function-only NONE omits function declarations
while retaining the explicitly requested hosted search. OpenRouter/xAI adapters
are exact-host checks; their behavior and native/cross-protocol choices are
covered by local tests. Request history excludes private search policy values.
These fixtures do not establish paid/live search or real-agent compatibility;
see docs/core-parity.md for option-conversion and result-ingress limitations.


## Anthropic search starts commit replay safety (2026-10-12)

An Anthropic hosted-search content_block_start produces SearchStarted immediately.
It is answer/content for the shared stream clock, group first-content commitment,
refusal policy and reasoning-only reask guards. Encoders do not manufacture an
empty completed search for this signal: native call output waits for its closed
input block, and foreign result output waits for a successful public result block.
Thus a paid search already started cannot be replayed through another key,
provider, group route or reasoning-only reask merely because no result has arrived.
It satisfies first-content limits but not future read-idle activity: an upstream
stall after start ends with one idle error, not a new attempt.

Search arguments are bounded independently of ordinary function fragments;
malformed/incomplete lifecycle or retained-state overflow emits one static error
and suppresses later success. Native search errors remain native data, not fabricated
successful foreign grounding. All observed token usage remains on the selected
attempt. Client disconnect releases the owned upstream and persists one sanitized
record without query, URL or encrypted result content. Local fixtures prove these
properties for the four client dialects and ordinary/group routes, not live vendors,
HTTP/2, TLS/proxies or real Agents. See docs/core-parity.md for budgets, block
preservation and remaining Anthropic citation and pause_turn limitations.
Repeated Gemini grounding updates are described below.


## Gemini grounding updates and opening commitment (2026-10-12)

SearchUpdate distinguishes later search metadata from a new completed search.
Its public query/hits are incremental; its optional native grounding object is
an exact upstream update. New public items count as answer/content and disable
replay. Native citation/attribution-only updates do neither. Gemini encoders
forward changed native objects intact, while foreign encoders omit native-only
updates and synthesize results only for new public items. Identical consecutive
native objects and already-seen public queries/URLs do not emit duplicate results.
The decoder bounds retained native metadata and public sets to 32 MiB and 4096
unique public items; overflow is a static terminal error, never a success.

Opening prefetch must classify native-only SearchUpdate as metadata rather than
commit on the mere presence of this event. This permits ordinary and group
first-content timeout fallback before any public answer. Buffered native update
payloads also count toward the existing 64 KiB opening metadata budget (as well
as the 64-event cap); crossing an opening budget intentionally commits the
attempt to avoid unbounded buffering, but does not satisfy its first-content
clock. Thus a large metadata-only update can end with a first-content error
without replaying another key/route. This differs from a small metadata-only
opening which may safely switch routes before client output.

Local HTTP/socket fixtures verify these cases for all four client dialects,
ordinary/group fallback, no success/retry after an answer's state overflow,
usage 7/3 and sanitized persisted history, and release after client disconnect.
Core tests also verify native-only updates do not manufacture foreign searches
and malformed/empty public metadata does not consume later valid results.
No paid/live vendor, HTTP/2 or real-agent compatibility is implied.

Native opaque reasoning commits an attempt even when the client protocol cannot
render it. See `sealed-reasoning.md` for passthrough and privacy guarantees.

## Native signature content

Nonempty native signatures commit an attempt and satisfy first-content, even
when a foreign client has no rendering for them. They are not public answer
text and do not enter prose-loop detection. Unsigned thinking starts remain
metadata. See docs/native-signatures.md for native-only mappings, error/retry
fixtures and the separate bounded reasoning-only re-ask policy.
