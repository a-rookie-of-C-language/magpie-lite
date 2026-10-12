# Chat-bound tool transcript repair

Updated: 2026-10-12.

Reference: `D:\code\magpie\internal\gateway\chat.go` (`pairToolMessages`)
and `internal/gateway/ir.go` (`joinSplitCalls`). Lite normalizes messages only
when encoding a request for a Chat upstream. It does not mutate the shared IR
or change the other upstream encoders. Unlike the reference's raw Chat relay,
Lite's Chat client path also passes through this encoder.

## Contract

- Adjacent assistant messages after a tool-call message form one exchange.
  Their text and calls are joined before emitting tool answers.
- Parallel answers are gathered in call order, including answers split across
  client messages. The first duplicate answer in a pending exchange wins.
- A real message boundary or end of history closes unanswered calls using an
  explicit interrupted/unavailable history marker. This does not execute a
  tool or invent successful output.
- Orphan, empty-ID and late answers become user text; existing user images are
  retained. An orphan also closes pending calls before delivering its text.
- Empty/thinking-only assistant messages do not interrupt a pending exchange,
  even after a partial answer.
- Existing nonempty IDs in clean history stay unchanged. Reused or missing
  call IDs receive deterministic request-local IDs reserved against all native
  call IDs. Results match only the current exchange's original ID; empty IDs
  are not guessed. Repeated encoding produces the same output.
- Duplicate nonempty IDs within one assistant exchange are ambiguous and fail
  locally without exposing IDs or content in the error. Existing routing may
  try a compatible fallback; otherwise the gateway returns a conversion error
  without making an inference request to that Chat route.

## Evidence and limits

`crates/core/tests/chat_tool_pairing.rs` has 15 regressions covering the above,
IR immutability and deterministic encoding. Before implementation, the initial
10-test batch had nine failures and one unchanged-history pass.

`crates/gateway/tests/chat_tool_pairing.rs` uses manually authored native client
requests and a strict local Chat HTTP upstream. Four client protocols, direct
and group routes, and JSON/SSE replies cover 16 successful requests. Another
eight requests reject ambiguous IDs without reaching the upstream. Assertions
cover usage, key attempts, history metadata and absence of private fixture
content in errors or persisted logs.

The existing Anthropic-to-Chat end-to-end fixture uses a valid call-then-result
exchange so it continues to test unchanged clean history. Malformed histories
are covered by the dedicated repair suites.

These are synthetic core/loopback fixtures, not live vendor or installed Agent
compatibility evidence. Other upstream protocols, provider-specific continuation
and broader multimodal/tool conformance remain separate work.
