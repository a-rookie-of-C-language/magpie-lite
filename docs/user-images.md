# User-sent images across all four upstreams

Updated: 2026-10-20.

Reference: `D:\code\magpie\internal\gateway\attachments_test.go`
(`TestGeminiSnakeCaseImageReachesUpstream`, image parts reaching a Chat
upstream), `anthropic_image_source_test.go` (base64/data-URL/url variants)
and `image_input.go` (current vs historical vs tool images). Lite carries
user images on the shared IR (`PartKind::Image`, inline base64 or URL), not
beside tool results. All four client decoders retain supported text and
image blocks; all four upstream encoders render them back in the dialect's
native shape. Repeated encoding is deterministic and does not mutate the IR.

## Wire contract

- Chat: `image_url` blocks, inline as `data:<media>;base64,<data>` URLs,
  otherwise the plain URL; decoded back by the same rule.
- Anthropic: `image` blocks with `base64` (`media_type` + `data`) or `url`
  sources; no other source variant is fabricated.
- Responses: `input_image` blocks with a plain `image_url` string (a `data:`
  URL for inline images); the `url` field spelling is also accepted.
- Gemini: `inlineData` (`mimeType` + `data`) for inline bytes, `fileData`
  (`mimeType` + `fileUri`) for URLs; camelCase and snake_case spellings are
  accepted on decode, camelCase is emitted.
- File-ID-only images, empty payloads and nonimage data URLs do not become
  fabricated images. No image URLs are fetched or validated against remote
  content.

## Image evidence and limits

`crates/gateway/tests/user_images.rs` sends **64 real loopback HTTP
requests**: four clients by four upstreams by direct/group routes by
JSON/SSE replies. One user turn carries text plus one inline PNG and one
URL image; strict mocks check native wire fields (block types, sources,
`inlineData`/`fileData` shapes, message roles/counts), exact text
preservation, independent downstream answer/usage/terminal assertions, and
that private image bytes/URLs/text never leak into the downstream wire.

These are synthetic core/loopback fixtures, not live vendor or installed
Agent compatibility evidence. Text-only model gating (rejecting current-turn
images, omitting historical ones), describe-via-seer fallback, group routing
to a vision member, model-generated image output beyond Gemini clients and
provider-specific rewrites remain separate work.
