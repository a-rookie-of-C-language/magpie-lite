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
- **Providers** from one TOML file: Chat-compatible, Responses-compatible, Anthropic, or Gemini upstreams
- **Model routing**: `deepseek/deepseek-chat` by provider id, `group/daily-coding` route chains, bare names by provider model lists, default provider fallback
- **Multi-key pools**: repeat `--api-key`; requests rotate across keys, rejected/rate-limited keys are retried before route fallback, and `402`/`429` starts a cooldown from `Retry-After`, common duration/reset headers, absolute reset timestamps, or JSON error-body retry delays (60s fallback, per-provider `key_cooldown_ms` / `--key-cooldown-ms` override; `0` disables it)
- **Failover**: ordered provider fallbacks and routing-group routes, tried before any reply bytes are streamed when the active provider is rate limited, out of quota, overloaded, or unreachable
- **Local authentication**: every non-health route requires the configured token as Bearer, Anthropic `x-api-key`, or Gemini `x-goog-api-key`; `/health` stays public
- **Web console**: open `http://127.0.0.1:3426/`, enter the local token, and manage routing groups or inspect provider totals, routing providers and groups, key-slot totals and active cooldowns, available models, recent requests, and recent usage; prompts, replies, and real keys never enter history or the page
- **Request statistics**: token-protected `/stats`, `/config`, and `magpie-lite stats` show provider and per-key totals, token usage, statuses, latency, active secret-free key cooldowns, and recent request metadata (including which key slot served it, never the key); `recent_usage` adds provider/model aggregates over the recent window, and optional sanitized JSONL history keeps that window across restarts while process totals stay process-local
- **Agent wiring**: `magpie-lite claude <model>` (settings.json env block), `magpie-lite codex <model>` (config.toml provider table, native `wire_api = "responses"`), `magpie-lite crush <model>` (custom OpenAI-compatible provider and large-model pick), `magpie-lite gemini <model>` (`~/.gemini/settings.json` plus `.env`), `magpie-lite goose <model>` (custom OpenAI-compatible provider plus the active model), `magpie-lite opencode <model>` (`~/.config/opencode/opencode.json` provider block), `magpie-lite zed <model>` (OpenAI-compatible provider in Zed settings; the token goes in Zed provider settings or `MAGPIE_LITE_API_KEY`, not `settings.json`)
- **Conformance tests** for every translation direction, and end-to-end tests over real HTTP with mock Chat and Gemini upstreams

## Quick start

```sh
cargo build --release

# add a provider
magpie-lite provider add deepseek \
  --base-url https://api.deepseek.com/v1 \
  --api-key sk-one... --api-key sk-two... \
  --models deepseek-chat deepseek-reasoner --default \
  --fallback kimi/moonshot-v1-128k

# add a named route chain agents pick as one model
magpie-lite group add daily-coding \
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

Restart the agent after wiring it. For Zed, also set MAGPIE_LITE_API_KEY to the gateway token in Zed's environment, or enter that token in Zed's AI provider settings. Unwire by restoring the agent's own provider (or use your agent switcher).

## Editing routing groups

The web console can create, edit, and delete routing groups. Enter an ID such as
`daily-coding`, an optional display name, and one `provider/model` route per line
in fallback order. Agents select `group/daily-coding`. IDs allow ASCII letters,
digits, hyphens and underscores; routes require an existing provider and a nonempty
model. Models may be unlisted, for providers that discover them dynamically.

The following endpoints require the same local token as inference routes:

| Method | Endpoint | Body / response |
|---|---|---|
| POST | `/config/groups` | `{"id":"daily","name":"Daily","routes":["deepseek/deepseek-chat"]}`; 201 on creation |
| PUT | `/config/groups/{id}` | Same body with matching ID; replaces name and routes; 200 |
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
persisted by group edits. Embedded callers using `serve(config, port)` without a
file path receive 503 for editing; use `serve_with_config_path` to enable it.

## Layout

| Crate | What it is |
|---|---|
| `crates/core` | The IR and the wire codecs (chat, responses, anthropic, gemini), streaming included |
| `crates/gateway` | The axum server, model routing, upstream clients, SSE translation |
| `crates/cli` | The `magpie-lite` binary and per-agent config adapters |

## Design

Every request is decoded into [`ir::CompletionRequest`](crates/core/src/ir.rs), every reply is built from `CompletionResponse`, and every stream passes through `StreamEvent`. A protocol plugs in as one decoder plus one encoder — never N-squared pairwise translators. This is magpie's design, kept.

Tests are the contract: `crates/core/tests/translation.rs` checks real wire shapes in every direction, and `crates/gateway/tests/e2e.rs` drives the whole server against mock Chat and Gemini upstreams, streaming included.

## Not yet (the roadmap)

- More agents (Cursor CLI, ...)
- Editing providers and key cooldowns from the console
- A TUI

## Safety

Agent adapters only rewrite the fields that name the gateway, and tests run against explicit paths — never your real home. `MAGPIE_LITE_HOME` redirects agent config lookups when you want it.

## License

MIT, like magpie.
