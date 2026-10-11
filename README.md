# magpie-lite

One local gateway for every coding agent — a Rust rewrite of [magpie](https://github.com/yetone/magpie)'s core: any agent, any provider, one place.

```
Claude Code ─┐                                  ┌─ OpenAI-compatible (DeepSeek, Kimi, GLM, OpenRouter, Ollama, ...)
Codex ───────┼─▶ magpie-lite gateway ─▶ IR ────▶├─ Anthropic Messages
             ─┘   Chat + Anthropic in,          └─ (Gemini: planned)
                  translated per provider
```

## What works

- **Gateway** at `127.0.0.1:3426` speaking both **OpenAI Chat Completions** (`/v1/chat/completions`) and **Anthropic Messages** (`/v1/messages`)
- **Cross-protocol translation** through a neutral IR: requests, replies, and **SSE streams** — text, tool calls, reasoning deltas, usage
- **Providers** from one TOML file: any OpenAI-compatible upstream, plus Anthropic
- **Model routing**: `deepseek/deepseek-chat` by provider id, bare names by provider model lists, default provider fallback
- **Agent wiring**: `magpie-lite claude <model>` (settings.json env block), `magpie-lite codex <model>` (config.toml provider table, `wire_api = "chat"`)
- **Conformance tests** for every translation direction, and end-to-end tests over real HTTP with a mock upstream

## Quick start

```sh
cargo build --release

# add a provider
magpie-lite provider add deepseek \
  --base-url https://api.deepseek.com/v1 \
  --api-key sk-... \
  --models deepseek-chat deepseek-reasoner --default

# run the gateway
magpie-lite serve

# point agents at it
magpie-lite claude deepseek/deepseek-chat
magpie-lite codex deepseek/deepseek-chat
```

Restart the agent after wiring it. Unwire by restoring its own provider (or use your agent switcher).

## Layout

| Crate | What it is |
|---|---|
| `crates/core` | The IR and the wire codecs (chat, anthropic), streaming included |
| `crates/gateway` | The axum server, model routing, upstream clients, SSE translation |
| `crates/cli` | The `magpie-lite` binary and per-agent config adapters |

## Design

Every request is decoded into [`ir::CompletionRequest`](crates/core/src/ir.rs), every reply is built from `CompletionResponse`, and every stream passes through `StreamEvent`. A protocol plugs in as one decoder plus one encoder — never N-squared pairwise translators. This is magpie's design, kept.

Tests are the contract: `crates/core/tests/translation.rs` checks real wire shapes in every direction, and `crates/gateway/tests/e2e.rs` drives the whole server against a mock upstream, streaming included.

## Not yet (the roadmap)

- OpenAI **Responses** API (Codex's native dialect; today Codex is wired with `wire_api = "chat"`)
- **Gemini** generateContent
- Routing groups with failover between providers/keys
- More agents (Gemini CLI, OpenCode, ...)
- A TUI/Web UI

## Safety

Agent adapters only rewrite the fields that name the gateway, and tests run against explicit paths — never your real home. `MAGPIE_LITE_HOME` redirects agent config lookups when you want it.

## License

MIT, like magpie.
