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

The prefetch runs in the handler before downstream response headers. The
streaming deadlines below now bound a below-budget stall when enabled. They do
not prove automatic handler cancellation on a real pre-header socket disconnect,
and the gateway still emits no downstream keepalive frames.

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
upstream may again hold a handler indefinitely. Nonstreaming requests retain the
previous behavior; these settings are not a total request-duration limit.
Configuration is pinned for the request, and each fallback attempt gets a fresh
clock. The first-content deadline remains active after a metadata safety budget
commits headers, but a later timeout can no longer retry that committed response.

Timeouts are endpoint failures, internally classified as 504. They skip any
remaining credentials on that endpoint and never apply quota cooldown. Before
commit they may try an ordinary fallback or next group route; after commit they
produce one terminal protocol error, retain observed partial usage, release the
upstream and never synthesize a success marker or splice another answer. A
header/error-body timeout on the last route returns HTTP 504 in the client's
error envelope; a timeout of an HTTP-200 SSE response uses the existing client
stream-error encoder (the transport status is already 200).

Timers are cooperative and enforced when the handler/event stream is polled;
they are not a background cancellation service and cannot interrupt blocking
CPU work or a downstream that never polls again. Idle time is transport silence,
not a post-content meaningful-progress or loop-detection policy: an endless
heartbeat stream after content can remain live. The reference group-specific
FirstToken policy, keepalives and slow-final-route exceptions are not replicated
by these gateway-wide safety settings.

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
in the client protocol. No new public configuration or management API is added.

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

One final record is written for each tracked request. After downstream headers,
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
discarded Chat model metadata.

`server::tests::cancelled_pending_fallback_accounts_for_last_attempt_before_headers`
explicitly aborts the handler task while the fallback waits for HTTP headers.
It exercises ordinary/group routing and streaming/nonstreaming attempts with
persisted accounting. **It does not establish that a real downstream socket
disconnect automatically cancels an Axum handler before its response headers.**
That real-HTTP pre-header cancellation behavior still needs separate work.

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

Group-specific timeout overrides, downstream keepalives, real pre-header socket
disconnect handling, post-content progress/loop detection and broader tool/
multimodal conformance remain open.
