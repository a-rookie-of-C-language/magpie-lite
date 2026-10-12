# magpie-lite

One local gateway for every coding agent — a Rust rewrite of [magpie](https://github.com/yetone/magpie)'s core: any agent, any provider, one place.

```
Claude Code ─┐                                  ┌─ OpenAI-compatible (DeepSeek, Kimi, GLM, OpenRouter, Ollama, ...)
Codex ───────┤                                  ├─ Anthropic Messages
Gemini CLI ─┤─▶ magpie-lite gateway ─▶ IR ────▶└─ Gemini generateContent
OpenCode ────┤
Zed ─────────┘   Chat + Responses +
                 Anthropic + Gemini
                 translated per provider
```

## What works

- **Gateway** at `127.0.0.1:3426` speaking **OpenAI Chat Completions** (`/v1/chat/completions`), **OpenAI Responses** (`/v1/responses`), **Anthropic Messages** (`/v1/messages`), and **Gemini generateContent** (`/v1beta/models/...`)
- **Cross-protocol translation** through a neutral IR: requests, replies, and **SSE streams** — text, tool calls, reasoning deltas, usage
- **Providers** from one TOML file: Chat-compatible, Responses-compatible, Anthropic, or Gemini upstreams; console CRUD, bounded model discovery and connection checks, credential rotation and cooldown editing with immediate runtime updates
- **Model routing**: `deepseek/deepseek-chat` by provider id, `group/daily-coding` route chains with process-local `order`, `rotate`, or `usage` scheduling, bare names by provider model lists, default provider fallback
- **Multi-key pools**: repeat `--api-key`; requests rotate across keys, rejected/rate-limited keys are retried before route fallback, and `402`/`429` starts a cooldown from `Retry-After`, common duration/reset headers, absolute reset timestamps, or JSON error-body retry delays (60s fallback, per-provider `key_cooldown_ms` / `--key-cooldown-ms` override; `0` disables it)
- **Failover**: ordered provider fallbacks and routing-group routes, tried before any reply bytes are streamed when the active provider is rate limited, out of quota, overloaded, or unreachable
- **Local authentication**: every non-health route requires the configured token as Bearer, Anthropic `x-api-key`, or Gemini `x-goog-api-key`; `/health` stays public
- **Web console**: open `http://127.0.0.1:3426/`, enter the local token, and manage Agent connections, providers and routing groups, discover upstream models, test draft connections, or inspect provider totals, routing providers and groups, key-slot totals and active cooldowns, available models, recent requests, and recent usage; prompts, replies, and saved keys are never returned to the page or persisted in request history
- **Request statistics**: token-protected `/stats`, `/config`, and `magpie-lite stats` show provider and per-key totals, token usage, statuses, latency, active secret-free key cooldowns, and recent request metadata (including which key slot served it, never the key); `recent_usage` adds provider/model aggregates over the recent window; `/stats?from=<unix-ms>&to=<unix-ms>` can filter that recent portion (`from` inclusive, `to` exclusive), and the protected web console exposes the same local-time range controls with validation, clear, and auto-refresh preservation; optional sanitized JSONL history keeps the window across restarts while process totals stay process-local
- **Agent wiring**: `magpie-lite claude <model>` (settings.json env block), `magpie-lite codex <model>` (config.toml provider table, native `wire_api = "responses"`), `magpie-lite crush <model>` (custom OpenAI-compatible provider and large-model pick), `magpie-lite gemini <model>` (`~/.gemini/settings.json` plus `.env`), `magpie-lite goose <model>` (custom OpenAI-compatible provider plus the active model), `magpie-lite opencode <model>` (`~/.config/opencode/opencode.json` provider block), `magpie-lite zed <model>` (OpenAI-compatible provider in Zed settings; the token goes in Zed provider settings or `MAGPIE_LITE_API_KEY`, not `settings.json`)
- **Conformance tests** for every translation direction, and end-to-end tests over real HTTP with mock Chat and Gemini upstreams

## Quick start

```sh
cargo build --release

# add a fallback provider before referencing it
magpie-lite provider add kimi \
  --base-url https://api.moonshot.cn/v1 \
  --api-key sk-kimi... --models moonshot-v1-128k

# add a provider
magpie-lite provider add deepseek \
  --base-url https://api.deepseek.com/v1 \
  --api-key sk-one... --api-key sk-two... \
  --models deepseek-chat deepseek-reasoner --default \
  --fallback kimi/moonshot-v1-128k

# add a named route chain agents pick as one model
magpie-lite group add daily-coding \
  --routing rotate \
  --route deepseek/deepseek-chat \
  --route kimi/moonshot-v1-128k

# run the gateway
magpie-lite serve
# open the console (models and statistics still require the local token)
# http://127.0.0.1:3426/

# agents use the configured token (default: magpie-lite) as their local API key

# point agents at it
magpie-lite claude group/daily-coding
magpie-lite codex group/daily-coding
magpie-lite crush group/daily-coding
magpie-lite gemini group/daily-coding
magpie-lite goose group/daily-coding
magpie-lite opencode group/daily-coding
magpie-lite zed group/daily-coding

# see provider/request/token totals
magpie-lite stats

# optionally keep sanitized recent history across restarts
magpie-lite serve --history-file 'D:\magpie\history.jsonl'
# or set history_path in config.toml
```

Restart the agent after wiring it. For Zed, also set MAGPIE_LITE_API_KEY to the gateway token in Zed's environment, or enter that token in Zed's AI provider settings. Disconnect through the console or `magpie-lite agent disconnect <id>` to restore the saved original configuration, provided no conflicting edits were made.

## Streaming timeouts

The root of `config.toml` accepts two gateway-wide streaming safety limits:

```toml
stream_first_content_timeout_ms = 600000 # per attempt, includes response headers
stream_idle_timeout_ms = 300000          # waiting for the next upstream body chunk
```

These are the defaults for old files too. Set either value to `0` to disable it.
Nonempty text/reasoning or a tool event satisfies the first-content limit; model,
role, usage and comment heartbeats do not extend it. The idle limit resets on
upstream transport bytes, not downstream backpressure. Before committing a
stream, timeouts can try a configured fallback/group route (not another key on
the same endpoint); after commitment they end with one protocol error and keep
observed partial usage. Header/error-body timeouts return HTTP 504 when no route
remains and no keepalive has sent HTTP headers; otherwise they become a stream
error. Each attempt gets a fresh budget. Nonstreaming requests are unchanged.

Both settings appear in protected GET `/config`; edit the TOML and restart to
change them (no timeout-edit API yet). They are not a total-duration limit.
See `docs/stream-lifecycle.md` for exact policies and remaining reference gaps.

## Streaming keepalive

Streaming Chat, Anthropic and Responses clients receive SSE comments while
waiting for upstream headers, opening metadata, or a quiet mid-answer stream:

```toml
stream_keepalive_interval_ms = 15000     # normal downstream comment interval
stream_keepalive_max_silence_ms = 300000 # stop after this much upstream silence
```

These defaults apply to old configuration files. Either value set to `0` disables
keepalive. Both appear in protected GET `/config`; edit the TOML and restart to
change them. Requests pin all streaming settings when they begin.

Only `: keepalive\n` is sent (no extra blank line), not opening model/usage from
an attempt that may still fail over. Key, provider and group fallback remain
available before actual content, Responses opening release, or buffering safety
bounds commit an attempt. Fast HTTP failures retain their status; once a comment has sent
HTTP 200/SSE headers, a later HTTP failure becomes one protocol stream error,
never a JSON body or a success marker. Known non-2xx body reads do not send
comments. Gemini and all nonstreaming requests never get these comments.

Transport bytes, including ignored comments and partial frames, renew the
silence window. After a stream is handed off, upstream activity with no client
output can trigger a comment after a one-second gap (or the configured interval,
whichever is shorter). Reaching the silence cap stops comments, not the request;
new upstream activity resumes them. A fresh fallback attempt starts a fresh
window. Downstream comments never count as upstream progress or reset the
first-content/read-idle deadlines, and they cannot satisfy clients that require
actual protocol events. No background worker is spawned: closing the client
connection drops pending work and preserves one final accounting record.

## Responses opening-event hold budget

```toml
stream_responses_hold_timeout_ms = 240000 # request-wide budget; 0 disables
```

For streaming Responses clients only, this releases real buffered upstream
opening metadata when the request-wide budget expires, even if answer content
has not arrived. The default also applies to old configuration files and the
setting appears in protected GET `/config`. It works independently of SSE
comment keepalive, because some agents require actual protocol events.

The budget is shared across key, provider and group retries. Pending headers,
comments and usage-only input do not fabricate an opening event. Releasing an
opening commits that attempt: later failures become stream errors, not retries.
It does not reset first-content or upstream read-idle deadlines. This is not a
post-content progress policy or a claim of real-Agent compatibility.

## Editing providers

The protected web console can create, edit, and delete providers without restarting the gateway. Provider metadata and route lists are editable directly; API keys are never returned. When editing an existing provider, choose **Keep existing keys**, **Replace key pool**, or **No upstream authentication** explicitly. New requests use a saved provider immediately, while in-flight requests keep their original endpoint and credential snapshot.

The same API is available to other interfaces:

| Method | Endpoint | Body / response |
|---|---|---|
| POST | `/config/providers` | Provider JSON with id, protocol, base_url, optional capabilities, api_keys and models; 201 |
| PUT | `/config/providers/{id}` | Same body; omit `api_keys` to preserve saved credentials, use `[]` to clear them; omit `capabilities` to preserve metadata, use `{}` to clear it; 200 |
| DELETE | `/config/providers/{id}` | 200; deletion is rejected while a group or fallback still references the provider |

Optional fields include `name`, `models`, `fallback`, `key_cooldown_ms`, `make_default`, and `capabilities`. Capability metadata accepts `supported_protocols` (`chat`, `responses`, `anthropic`, `gemini`) plus nullable `streaming`, `tools`, and `vision` booleans. An empty protocol list means the configured protocol; a non-empty list must include it and contain no duplicates. `null` means unknown, not unsupported. This metadata is currently for management/UI and does not change request translation or prove a live vendor feature probe. PUT replaces metadata: omitted lists become empty and an omitted/null cooldown inherits the gateway default. Only `api_keys` and `capabilities` have preserve-on-omission semantics (null also preserves for keys; `{}` explicitly clears capabilities). `make_default: true` selects the provider; false keeps the current default. Creating the first provider selects it automatically. Deleting the default selects the first remaining provider, or clears the default when none remain.
IDs permit ASCII letters, digits, hyphens and underscores; `group` is reserved. Base URLs require HTTP/HTTPS without embedded credentials, query strings, fragments or whitespace. Fallbacks name existing `provider/model`, `group/id`, or listed bare model IDs. CLI writes use the same validation and reference checks; adding a local provider may omit `--api-key`. As with the existing add command, CLI updates replace the supplied provider including its key pool.

`/config` returns `credential_count` (zero for no upstream auth), effective `key_cooldown_ms`, and nullable `key_cooldown_override_ms`. Endpoint/protocol/key-pool changes start a new process-local rotation schedule; metadata changes keep active cooldowns. A delayed quota rejection from an old request cannot cool a replacement key. Credentials remain in the local TOML file; a separate secret store is still planned.

GET `/config` and successful provider/group edits return a numeric `revision` and a quoted `ETag` (for example, `"2"`). Send that ETag as `If-Match` on POST/PUT/DELETE to reject stale writes with 409 without changing memory or disk. Bare numeric revisions are also accepted; malformed values return 400. Omitting the header or using `*` keeps legacy unconditional updates. The console sends its latest loaded revision. Revisions are persisted in the atomically replaced TOML document under the reserved `__magpie_lite.revision` table, so a gateway restart cannot reset the concurrency token. Legacy files without metadata start at revision 1. Gateway writes also take a bounded lock at `<config-path>.lock`; a lock held by another gateway returns 409.

Provider and group create/update/delete also accept an optional `Idempotency-Key` header (up to 200 bytes). For the same scope, key, and token-keyed HMAC-SHA256 request digest, the gateway replays the original secret-free snapshot without incrementing the revision; reusing a key with a different request returns 409. Entries are stored in `<config-path>.idempotency.json`, retaining the most recent 256 committed operations. The ledger never stores the token, provider credentials, or raw request body. A pending entry left by a crash fails closed with 409 rather than risking a duplicate write. Agent `POST /agents/{id}/apply` accepts the same optional header and stores its secret-free status in `<agent-state-dir>/agent-idempotency.json`; a committed retry replays the status, a digest mismatch returns 409, and a crash left pending entry fails closed. Agent apply still requires a fresh preview revision, and the agent ledger never stores the gateway token, provider credentials, or raw request body. Successful edits return the secret-free `/config` snapshot. Invalid provider IDs, protocols, URLs, models, keys, and fallback routes return 400. Duplicate providers return 409; missing providers return 404. Changes are persisted through the same atomic config update used by routing groups.

## Agent connection, restore and recovery

The CLI and protected HTTP API now share seven adapters through the
`magpie-agents` crate. Use `agent preview <id> <model>`,
`agent connect <id> <model>`, `agent preview <id> --disconnect`,
`agent disconnect <id>` and `agent recover <id>`. `agent list` reports managed
connection, drift, pending recovery and passive detection details. `ls` includes
configuration markers, the discovered CLI path and whether it is on the launcher's
captured PATH (or unknown when the search is limited). Existing per-agent shorthand
commands also save a durable backup before writing.

The **Coding agents** panel shows detection, connection, selected model, drift
and recovery status for all seven agents. It also separates configuration markers
from CLI discovery and warns about launchers outside the gateway's PATH. Detection
never runs a launcher and is not a terminal-PATH, version or health check.
Click **Manage**, choose a configured
route (or type `provider/model` / `group/id`), and **Preview changes**. Review the
paths, operations and byte counts, then **Confirm connection**. To disconnect,
choose **Disconnect / restore backup**, preview and **Confirm restoration**.
Interrupted operations expose a separate, explicitly confirmed recovery action.
Model/action edits, changed state/routes and sign-out clear reviewed previews.
A status-read failure disables Agent actions without breaking the other panels.

HTTP management exposes `GET /agents` and
`POST /agents/{id}/preview|apply|recover`. Apply requires the reviewed preview
revision. Previews show paths, operations and byte counts, not secrets.

Unchanged managed files can be restored byte-for-byte; unrelated later edits are
preserved by selective field restoration. Managed-field conflicts and unsafe
configuration shapes are refused rather than overwritten. Multi-file operations have explicit journaled
recovery, not an atomic all-file transaction. Journals retain sensitive baseline
credentials and are **not encrypted**. See `docs/agent-lifecycle.md` for commands,
API bodies, port overrides, backup retention, platform permissions and precise limitations.

## Discovering models and testing providers

The provider editor offers **Discover models** and **Test connection** for both
saved providers and unsaved drafts. Discovery appends unique IDs to the draft
model list; it does not remove manually entered IDs. Save explicitly to publish
that list. Testing uses the chosen test model, or the first draft model, and sends
`Reply with OK.` as a non-streaming generation request with a 64-output-token
limit. It may incur provider charges. A successful test checks the selected
protocol's response envelope, not model quality or support for tools/images.

Both endpoints require the local gateway token:

| Method | Endpoint | Body / response |
|---|---|---|
| POST | `/config/provider-tools/models` | `{"provider": <provider-edit-json>}`; 200 with `{"models":["model-id"]}` |
| POST | `/config/provider-tools/test` | `{"provider": <provider-edit-json>,"model":"model-id"}`; 200 with `ok`, `protocol`, `model` and `elapsed_ms` |

The draft uses the provider-edit fields above. An omitted/null `api_keys` reuses
credentials of the saved provider with the same ID; explicit keys replace them
for this operation only, and `[]` sends no authentication. Unknown IDs with no
keys are unauthenticated. Checks validate the draft endpoint/keys/model without
requiring its unsaved fallback references to resolve. They work even when the
server was started without a writable configuration path.

Catalog endpoints are `/models` for Chat/Responses, `/v1/models` for Anthropic,
and `/models` for Gemini, relative to each protocol's documented base URL.
Anthropic uses `has_more` / `last_id` with `after_id` pagination; Gemini uses
`nextPageToken` / `pageToken`, strips `models/` from IDs and includes only models
advertising `generateContent`. Anthropic API keys use `x-api-key` plus
`anthropic-version` for both discovery and normal inference. Gemini uses
`x-goog-api-key`; Chat/Responses use Bearer authentication.

Each operation has a 30-second overall deadline, 5-second connection timeout,
10-second per-request timeout, 1 MiB cumulative success-body limit, at most
20 catalog pages per credential and 5000 unique models. At most the first eight
configured credentials are attempted, in saved order, retrying only HTTP
401/402/403/429/500/502/503/504. Redirects are not followed. Errors expose only
sanitized diagnostics and HTTP status codes, never upstream bodies or URLs.
Responses that echo provider credentials as model IDs are rejected.

Checks do not save configuration, alter live key rotation/cooldowns, log model
outputs or add gateway usage totals. Provider-side accounting is independent.
Catalogs unsupported by a compatible service can still be entered manually;
stream-only/specialized authentication services need a future probe adapter.
The current tests use isolated local HTTP fixtures, not paid live vendor keys.

## Editing routing groups

The web console can create, edit, and delete routing groups. Enter an ID such as
`daily-coding`, an optional display name, and one `provider/model` route per line
in fallback order. Agents select `group/daily-coding`. IDs allow ASCII letters,
digits, hyphens and underscores; routes require an existing provider and a nonempty
model. Models may be unlisted, for providers that discover them dynamically. Choose
`order` to always start at the first route, `rotate` to advance the starting route
for each request, or `usage` to prefer routes with fewer attempts in this gateway
process. All remaining routes are still tried as fallbacks before response bytes
are streamed.

The following endpoints require the same local token as inference routes:

| Method | Endpoint | Body / response |
|---|---|---|
| POST | `/config/groups` | `{"id":"daily","name":"Daily","routing":"rotate","routes":["deepseek/deepseek-chat"]}`; 201 on creation |
| PUT | `/config/groups/{id}` | Same body with matching ID; replaces name, routing, and routes; 200 |
| DELETE | `/config/groups/{id}` | No body; 200 |

Successful edits return the secret-free `/config` snapshot. Invalid input returns
400, missing groups 404, duplicate IDs or referenced group deletions 409. A provider
fallback that names a group must be changed before that group can be deleted.

Changes are written to the selected `--config` file (or the standard config path)
using a temporary file and replacement, then become visible to new requests and
model catalogs immediately. In-flight requests keep their original routing snapshot.
A failed save leaves the active configuration unchanged. Edits are serialized within
one gateway process. External file changes are detected before saving and return
409; restart the gateway to load CLI or manual config edits. Run one gateway process
per config file: the external-change check does not lock out simultaneous writes
from other processes. Comments and formatting are rewritten when saving TOML.

`serve --port` and `serve --history-file` remain runtime overrides and are not
persisted by provider or group edits. Embedded callers using `serve(config, port)` without a
file path receive 503 for editing; use `serve_with_config_path` to enable it.

## Layout

| Crate | What it is |
|---|---|
| `crates/core` | The IR and the wire codecs (chat, responses, anthropic, gemini), streaming included |
| `crates/gateway` | The axum server, model routing, upstream clients, SSE translation |
| `crates/agents` | Shared agent adapters, private preview, revision checks, journals and restoration/recovery |
| `crates/cli` | The `magpie-lite` binary using the same backend and agent lifecycle as HTTP management |

## Design

Every request is decoded into [`ir::CompletionRequest`](crates/core/src/ir.rs), every reply is built from `CompletionResponse`, and every stream passes through `StreamEvent`. A protocol plugs in as one decoder plus one encoder — never N-squared pairwise translators. This is magpie's design, kept.

Real loopback HTTP/1.1 tests verify that closing a client before response headers
releases pending upstream work, retains observed usage and persists exactly one
cancellation record, including after fallback. See [stream lifecycle](docs/stream-lifecycle.md)
for tested scope and remaining HTTP/2/keepalive limits.

Tests are the contract: `crates/core/tests/translation.rs` checks real wire shapes in every direction, and `crates/gateway/tests/e2e.rs` drives the whole server against mock Chat and Gemini upstreams, streaming included.

## Not yet (the roadmap)

- More agents (Cursor CLI, ...)
- A TUI
- Selective Agent restoration preserving later unrelated edits and richer detection/override semantics
- Broader provider capability metadata and stream-only/specialized connection probes
- More routing policies, versioned admin API and separate management authentication

See [core parity tracker](docs/core-parity.md) for implementation evidence and remaining reference gaps.

## Safety

Agent configuration operations use reviewed revisions and durable backups, and refuse later file drift. The test suites run against explicit temporary paths or child processes — never your real home. `MAGPIE_LITE_HOME` redirects agent config lookups in sandboxes and wrappers. Sensitive backups are access-restricted but not encrypted; see `docs/agent-lifecycle.md`.

## License

MIT, like magpie.

### Browser verification

After `cargo build -p magpie-lite`, run `node --test tests/browser/agents.cjs`
with Node 18+ and Playwright available. Set `PLAYWRIGHT_MODULE` to an absolute
Playwright module directory if it is not on Node's module path;
`PLAYWRIGHT_EXECUTABLE_PATH` optionally selects an installed Chromium browser.
The runner launches its own gateway on an ephemeral loopback port with
`MAGPIE_LITE_HOME` set only in the child. It uses a temporary home and configuration,
never the user's Agent files, and cleans up after stopping that child.
See `docs/agent-lifecycle.md` for coverage and fixture limitations.
