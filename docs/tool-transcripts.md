# Tool transcripts and returned images

Updated: 2026-10-12.

Reference: `D:\code\magpie\internal\gateway\chat.go` (`pairToolMessages`)
and `internal/gateway/ir.go` (`joinSplitCalls`). Lite normalizes messages only
when encoding a request for a Chat upstream. It does not mutate the shared IR
or change the other upstream encoders. Unlike the reference's raw Chat relay,
Lite's Chat client path also passes through this encoder.

## Chat-bound history repair

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

### History repair evidence and limits

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
compatibility evidence. History repair for non-Chat upstreams, provider-specific
continuation and broader multimodal/tool conformance remain separate work.

## Tool-returned images across all four upstreams

Reference: `internal/gateway/ir.go` (`toolOutput`),
`chat.go` (`pairToolMessages`), `anthropic.go`, `responses.go`,
`gemini.go` and `tool_images_test.go` in the reference checkout.
Implementation: `crates/core/src/tool_output.rs` and the four request codecs.

- The shared IR owns images on a tool-result part, not as independent user
  attachments. All four client decoders retain supported text and image blocks.
  Repeated encoding is deterministic and does not mutate that IR.
- Anthropic emits images inside `tool_result.content`; Responses inside
  `function_call_output.output`; Gemini inside `functionResponse.parts`.
  The original call/result association is retained. Text-only results keep
  their existing wire representation.
- Chat emits string-only tool answers. A note says that the tool returned
  images; source labels contain the tool name and final wire call ID. Images
  follow as user content only after all pending parallel answers are emitted,
  in call order. An immediately following user message retains its content
  after these image blocks; otherwise a separate user message is added.
- Chat duplicate answers remain first-wins, including their images. Orphan and
  late results retain images as user content, without inventing a tool call or
  replacing an already emitted interrupted marker. This is still Chat-only
  history repair, not a repair guarantee for other upstreams.
- Supported images are nonempty inline base64 with an image MIME type or image
  URLs. Gemini accepts the existing camelCase/snake_case attachment fields.
  Additional non-thought Gemini tool text is retained. No image URLs are
  fetched or validated against remote content; MIME hints for URL images are
  not a file-type detection guarantee.
- File-ID-only images, empty payloads and nonimage data URLs do not become
  fabricated images. Unknown typed content blocks, audio/PDF/file transfer and
  general structured tool payload translation are outside this contract.
  Responses plain strings and untyped JSON outputs retain the previous text
  behavior; this does not promise preservation of unknown blocks mixed into a
  recognized text/image block array. Existing Gemini nonimage attachment
  behavior is not expanded by this feature.

### Image evidence and limits

`crates/core/tests/tool_images.rs` has 10 regressions, including a manually
built four-client by four-upstream codec matrix, parallel results, duplicate and
late answers, image ownership, deterministic/immutable encoding, Gemini extra
text and unsupported-file bounds. The original five-test batch failed on image
loss/JSON stringification before implementation.

`crates/gateway/tests/tool_images.rs` sends **64 real loopback HTTP requests**:
four clients by four upstreams by direct/group routes by JSON/SSE replies. Two
parallel tools return one inline image and one URL image in reverse completion
order. Strict mocks inspect native fields, complete call/result IDs, image
count and nesting, exact text-only Chat tool messages and labelled user blocks.
Independent downstream assertions verify exact answer text, one terminal event,
reported input/output usage (including explicitly requested Chat SSE usage),
and response type. Accounting checks requested/resolved models, key attempts,
stream flags, persisted records, and absence of private fixture text, image
payloads/URLs and credentials in history/statistics. Servers use ephemeral
loopback ports and are aborted when fixtures drop; history lives in tempdirs.

A controlled omission experiment disabled tool-image encoding, observed the
HTTP matrix fail on missing native image fields, then restored the exact source
and reran the matrix successfully. This establishes sensitivity to actual
image loss, not vendor compatibility. These tests do not run installed Agents,
resolve file IDs, fetch images or call live/paid providers.

Reproduce with:

~~~powershell
cargo test -p magpie-core --test tool_images --test chat_tool_pairing
cargo test -p magpie-gateway --test tool_images --test chat_tool_pairing
~~~
