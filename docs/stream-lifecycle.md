# Stream lifecycle and failover

Reference: `D:\code\magpie\internal\gateway\stream_error_open_test.go` and
`canceled_usage_test.go`. This document describes the Rust gateway's implemented
contract, not full Magpie or live-vendor conformance.

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

## Failover before an answer

A successful HTTP status is not sufficient for an SSE attempt. Before handing
it to the client encoder, the gateway holds opening model/role/usage metadata
and reads until one of:

- Nonempty text or reasoning, a tool event, or a stop event commits the attempt.
- A completion marker commits even an empty answer.
- An error closes the upstream immediately and permits retry according to its
  structured status, provided the attempt has not committed.
- 64 decoded opening events commit the attempt as a buffering safety bound.
  Errors after that bound do not retry, even if there has been no actual text.

Discarded attempts' opening frames are never encoded into the selected answer.
A new attempt gets a fresh upstream decoder; the client encoder is created only
for the selected attempt. An answer is never assembled from two upstreams. In
particular reasoning/tool activity counts as committed even if a particular
client dialect buffers or hides that activity.

The prefetch runs in the handler before downstream response headers. An upstream
that sends fewer than 64 metadata events and then stalls can still hold the
handler indefinitely: this stage adds neither a first-content timeout nor
keepalive frames. The event bound is not a raw-byte or parser-buffer bound.

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
19-byte fragmentation and held-open response bodies. It covers four upstream
by four client dialects for terminal/error/EOF behavior, early key failover,
exhaustion, invalid requests and no-retry after text. Additional fixtures cover
reasoning/tool commitment, bounded metadata, auth key exhaustion, every pair of
upstream/fallback dialects through ordinary and grouped routing, and cancellation
with JSONL persistence/reload. Drop guards check that all attempted response
bodies, not merely the first, are released.

`server::tests::cancelled_pending_fallback_accounts_for_last_attempt_before_headers`
explicitly aborts the handler task while the fallback waits for HTTP headers.
It exercises ordinary/group routing and streaming/nonstreaming attempts with
persisted accounting. **It does not establish that a real downstream socket
disconnect automatically cancels an Axum handler before its response headers.**
That real-HTTP pre-header cancellation behavior still needs separate work.

`crates/core/tests/stream_accounting.rs` covers cumulative counters and Chat /
Responses error envelope decoding. `stream_status` unit fixtures cover structured
error classification. These are synthetic protocol fixtures, not captured real
Agent sessions or paid/live vendor compatibility evidence. Raw SSE byte/frame
limits, first-content/idle timeout policy, real pre-header disconnect handling,
and broader tool/multimodal conformance remain open.
