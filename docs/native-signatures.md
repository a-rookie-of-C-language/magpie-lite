# Native thinking and part signatures

This contract covers Anthropic thinking signatures and Gemini thoughtSignature
values in static JSON/history and SSE. It is separate from opaque sealed
reasoning; see docs/sealed-reasoning.md. Reference comparison: the gateway IR
KThinkStart, KSig and KTextSig events and native encoders in
D:\code\magpie\internal\gateway\{ir,anthropic,gemini}.go.

## Native-only provenance

Decoded signatures carry their source protocol. Anthropic signatures render only
in Anthropic thinking blocks; Gemini signatures render only in Gemini thinking,
text or function-call parts. Public reasoning text can still translate across
protocols. Signature bytes never become public text, tool arguments or prose-loop
input. Manually constructed legacy IR parts without provenance retain their
existing signature behavior; adapters tag decoded thinking/text/tool signatures.

Anthropic SSE preserves initial thinking text/signatures, signature_delta
fragments, signed-only blocks and adjacent thinking block boundaries. Deltas
for the wrong, closed or previous-message block are ignored. An unsigned empty
thinking start is metadata and does not fabricate an output thinking block.

Gemini accepts camelCase and snake_case signature fields. JSON replies and
history retain signed-only text/thought parts as well as text, thinking and
function-call signatures. SSE emits text/thinking signatures as native empty-text
parts (with thought=true for thinking), while tool signatures are accumulated on
the matching pending function call. This is semantic preservation, not a claim
that stream chunks or part boundaries are byte-for-byte identical upstream.
Image signatures retain the existing native-image path, not a duplicate event.

## Stream commitment and privacy

A nonempty signature satisfies first-content and commits an upstream attempt,
even when it has no public text or is suppressed for a foreign client. A later
upstream rate limit cannot replay that committed attempt on another key/provider.
Unsigned opening metadata alone remains eligible for opening-error retry.
Signatures are not answers for the existing bounded reasoning-only re-ask policy.
Request statistics and persisted history retain usage/metadata, not signatures,
conversation text, gateway tokens or provider keys.

## Reproducible evidence

- crates/core/tests/signatures.rs: native JSON/history/SSE round trips,
  fragmented signatures, adjacent signed-only thinking blocks, foreign protocol
  isolation, incorrect/closed tool and thinking indexes, message reset and
  content-versus-answer classification.
- crates/gateway/tests/signatures.rs: 32 JSON/SSE requests covering four clients,
  two native upstreams and direct/group routes, including history, signature
  counts, downstream usage and accounting; 16 signature-only opening-error
  cases prohibit hidden retries; 16 unsigned-metadata cases retry a different
  key. Temporary storage and loopback HTTP only; no paid vendor traffic or
  changes to installed Agent configuration.

Run cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings
and cargo fmt --all -- --check from the repository root.

Fixtures do not prove live vendor compatibility, model-specific signature
validation, cross-provider signature portability, cryptographic validity or
byte-for-byte signed transcript compatibility. No such portability is attempted.
