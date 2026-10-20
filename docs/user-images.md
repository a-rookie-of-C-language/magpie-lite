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
Agent compatibility evidence. Describe-via-seer fallback,
model-generated image output beyond Gemini clients and
provider-specific rewrites remain separate work.

## Text-only members

Reference: `internal/gateway/image_input.go` (`textOnlyBody`,
`hasImage`) and `image_input_test.go` (known text-only rejection, mixed
group routing, historical/tool omission). A provider that declares
`capabilities.vision: false` is never sent image bytes. The check runs on
the shared IR per attempt, not on wire JSON: a user image in the request's
last message is refused with `400 model "<provider>/<model>" does not
support image input` before any upstream call; older direct images become
the text `[Image omitted: this model accepts text only.]`, and
tool-returned images are dropped with the same note appended to the result
text. A group skips a text-only member for an image-carrying request and
serves it from the first member that sees, through the existing
local-conversion fallback; a direct text-only route answers 400. Unknown
(`None`) or `true` vision leaves the request untouched. Lite tracks vision
per provider, where the reference tracks it per model; presets prefill
`false` for known text-only vendors (e.g. DeepSeek).

### Text-only evidence and limits

`crates/gateway/src/text_only.rs` has 4 unit regressions (current-turn
detection, tool-result exclusion, omission shape, imageless pass-through).
`crates/gateway/tests/text_only_images.rs` sends **20 real loopback HTTP
requests**: a blind (`vision: false`) and a seeing provider behind one
`mixed` group, four client protocols each. Current-turn user images are
refused with 400 on the direct blind route with neither upstream touched;
older-turn and tool-returned images reach the blind upstream as omission
notes with no image bytes; grouped image requests skip the blind member
and reach the seeing upstream intact over JSON and SSE.

These are synthetic loopback fixtures. Per-model vision catalogs, paid
live-vendor evidence, describe-via-seer fallback and subscription-account
affinity remain outside this contract.

## Unportable references

Reference: `internal/gateway/image_input.go`
(`hasUnportableCurrentImage`) and `gateway.go` (`translate`): a Responses
`input_image` with a `file_id` but no `image_url` is answered 400 on a
cross-protocol route, since only its own Responses API can open the file.
Lite refuses the same block at decode with `input_image without image_url
is not supported; send an image_url`, on every route: lite has no file
registry, so a native Responses route cannot carry it either (the reference
passes the `file_id` through natively). Nothing is silently dropped.
Tool-returned `file_id` images keep the existing lenient behavior — dropped
with the text kept (`unsupported_file_ids_empty_images...` in
`crates/core/tests/tool_images.rs` pins it) — matching the reference,
which only rejects current-turn user images.

### Unportable evidence and limits

`crates/core/tests/responses.rs` has a decode regression (refusal message,
portable URL/data-URL images still decode).
`crates/gateway/tests/unportable_images.rs` sends **2 real loopback HTTP
requests**: a Responses client `file_id` image to a Chat upstream and to a
native Responses upstream, both answered 400 with neither upstream
touched.
