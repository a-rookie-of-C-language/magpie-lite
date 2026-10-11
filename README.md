# magpie-lite

One local gateway for every coding agent — a Rust rewrite of [magpie](https://github.com/yetone/magpie)'s core: any agent, any provider, one place.

```
Claude Code ─┐                                  ┌─ OpenAI-compatible (DeepSeek, Kimi, GLM, OpenRouter, Ollama, ...)
Codex ───────┼─▶ magpie-lite gateway ─▶ IR ────▶├─ Anthropic Messages
Gemini CLI ─┘   Chat + Responses +                └─ Gemini generateContent
                Anthropic + Gemini
                translated per provider
```

## What works

- **Gateway** at `127.0.0.1:3426` speaking **OpenAI Chat Completions** (`/v1/chat/completions`), **OpenAI Responses** (`/v1/responses`), **Anthropic Messages** (`/v1/messages`), and **Gemini generateContent** (`/v1beta/models/...`)
- **Cross-protocol translation** through a neutral IR: requests, replies, and **SSE streams** — text, tool calls, reasoning deltas, usage
- **Providers** from one TOML file: Chat-compatible, Responses-compatible, Anthropic, or Gemini upstreams
- **Model routing**: `deepseek/deepseek-chat` by provider id, bare names by provider model lists, default provider fallback
- **Failover**: ordered `--fallback provider/model` routes, tried before any reply bytes are streamed when the active provider is rate limited, out of quota, overloaded, or unreachable
- **Local authentication**: every model route requires the configured token as Bearer, Anthropic `x-api-key`, or Gemini `x-goog-api-key`; `/health` stays public
- **Agent wiring**: `magpie-lite claude <model>` (settings.json env block), `magpie-lite codex <model>` (config.toml provider table, `wire_api = "chat"`)
- **Conformance tests** for every translation direction, and end-to-end tests over real HTTP with mock Chat and Gemini upstreams

## Quick start

```sh
cargo build --release

# add a provider
magpie-lite provider add deepseek \
  --base-url https://api.deepseek.com/v1 \
  --api-key sk-... \
  --models deepseek-chat deepseek-reasoner --default \
  --fallback kimi/moonshot-v1-128k

# run the gateway
magpie-lite serve# agents use the configured `token` (default: magpie-lite) as their local API key


# point agents at it
magpie-lite claude deepseek/deepseek-chat
magpie-lite codex deepseek/deepseek-chat
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

- Codex wiring with native `wire_api = "responses"` (the endpoint works; the adapter currently uses the widely compatible Chat dialect)
- Routing groups and multi-key provider pools (single-provider fallback already works)
- More agents (Gemini CLI, OpenCode, ...)
- A TUI/Web UI

## Safety

Agent adapters only rewrite the fields that name the gateway, and tests run against explicit paths — never your real home. `MAGPIE_LITE_HOME` redirects agent config lookups when you want it.

## License

MIT, like magpie.
