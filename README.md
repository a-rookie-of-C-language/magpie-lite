# magpie-lite

One local gateway for every coding agent — a Rust rewrite of [magpie](https://github.com/yetone/magpie)'s core: any agent, any provider, one place.

```
Claude Code ─┐                                  ┌─ OpenAI-compatible (DeepSeek, Kimi, GLM, OpenRouter, Ollama, ...)
Codex ───────┤                                  ├─ Anthropic Messages
Gemini CLI ─┤─▶ magpie-lite gateway ─▶ IR ────▶└─ Gemini generateContent
OpenCode ────┘   Chat + Responses +
                 Anthropic + Gemini
                 translated per provider
```

## What works

- **Gateway** at `127.0.0.1:3426` speaking **OpenAI Chat Completions** (`/v1/chat/completions`), **OpenAI Responses** (`/v1/responses`), **Anthropic Messages** (`/v1/messages`), and **Gemini generateContent** (`/v1beta/models/...`)
- **Cross-protocol translation** through a neutral IR: requests, replies, and **SSE streams** — text, tool calls, reasoning deltas, usage
- **Providers** from one TOML file: Chat-compatible, Responses-compatible, Anthropic, or Gemini upstreams
- **Model routing**: `deepseek/deepseek-chat` by provider id, `group/daily-coding` route chains, bare names by provider model lists, default provider fallback
- **Multi-key pools**: repeat `--api-key`; requests rotate across keys, rejected/rate-limited keys are retried before route fallback, and `402`/`429` starts a fixed cooldown (60s default, per-provider `key_cooldown_ms` / `--key-cooldown-ms` override)
- **Failover**: ordered provider fallbacks and routing-group routes, tried before any reply bytes are streamed when the active provider is rate limited, out of quota, overloaded, or unreachable
- **Local authentication**: every non-health route requires the configured token as Bearer, Anthropic `x-api-key`, or Gemini `x-goog-api-key`; `/health` stays public
- **Request statistics**: token-protected `/stats` and `magpie-lite stats` show provider and per-key totals, token usage, statuses, latency, and recent request metadata (including which key slot served it, never the key) — in memory only, with no prompt/reply text or credentials
- **Agent wiring**: `magpie-lite claude <model>` (settings.json env block), `magpie-lite codex <model>` (config.toml provider table, native `wire_api = "responses"`), `magpie-lite gemini <model>` (`~/.gemini/settings.json` plus `.env`), `magpie-lite opencode <model>` (`~/.config/opencode/opencode.json` provider block)
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
# agents use the configured token (default: magpie-lite) as their local API key

# point agents at it
magpie-lite claude group/daily-coding
magpie-lite codex group/daily-coding
magpie-lite gemini group/daily-coding
magpie-lite opencode group/daily-coding

# see provider/request/token totals
magpie-lite stats
```

Restart the agent after wiring it. Unwire by restoring its own provider (or use your agent switcher).

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

- `Retry-After` and provider quota-header-aware cooldown durations (fixed cooldowns and per-provider overrides work)
- Persistent request history and richer usage views (in-memory `/stats` already works)
- More agents (Cursor CLI, Zed, Crush, Goose, ...)
- A TUI/Web UI

## Safety

Agent adapters only rewrite the fields that name the gateway, and tests run against explicit paths — never your real home. `MAGPIE_LITE_HOME` redirects agent config lookups when you want it.

## License

MIT, like magpie.
