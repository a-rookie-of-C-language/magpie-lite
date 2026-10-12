# Native sealed reasoning

Opaque reasoning is retained only for its native protocol:

- Anthropic: complete redacted_thinking blocks with a string data field.
- Responses: complete reasoning items with nonempty string encrypted_content.

The IR preserves the whole JSON value, including unknown fields. This is JSON
semantic preservation, not byte-for-byte whitespace or object-key ordering.
Request history and nonstreaming replies emit the original value only when the
target protocol matches. Foreign protocols never receive the ciphertext or its
native reasoning ID. Only Responses summary entries of type summary_text become
public reasoning; unknown summary blocks remain native.

## Streaming and retry

Anthropic emits each redacted block once, in order, with its own start/stop pair.
Responses preserves reasoning-item boundaries and IDs, completing the matching
open summary item with its sealed value rather than duplicating it. Items without
public summaries still survive; a missing ID does not duplicate an open item.

A reasoning-item start is bounded opening metadata, not first content. A sealed
item is content and commits the attempt, even if the client encoder omits it.
Errors after sealed content cannot retry another key, provider or group route.
Opaque values do not enter the prose loop guard and are not visible answer text;
the existing bounded reasoning-only re-ask policy remains applicable.

## Evidence

- Eleven core regressions cover native history/replies, foreign isolation,
  malformed shapes, unknown summaries, repeated blocks and SSE item boundaries.
- Two real loopback HTTP tests exercise 32 successful combinations: four client
  protocols, two native upstream protocols, direct/group routes and JSON/SSE.
- Another 16 HTTP cases send opaque content then an upstream error, with two
  distinct keys and configured backup routes; no retry or fallback occurs.
- Fixtures check sanitized accounting/history and use temporary storage. They
  do not call paid services or modify installed Agent configuration.

Run from the repository root:

```powershell
cargo test -p magpie-core --test sealed_reasoning
cargo test -p magpie-gateway --test sealed_reasoning
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

This does not implement general Anthropic signature_delta or Gemini thought
signature streaming parity, nor prove compatibility with real vendor services.
