# Core parity tracker

Updated: 2026-10-12. Reference checkout: `D:\code\magpie`; implementation: `D:\code\magpie-lite`.

Target: multiple coding agents sharing one Rust backend, provider adapters for different services, and an interface managing providers, models, routing and usage. The reference rust-core-refactor-plan.md, rust-core-contracts.md and rust-core-admin-api.md are proposals with broader acceptance gates. This tracker does not claim G1–G16 completion or full Magpie product parity.

| Area | Current evidence | Remaining work |
|---|---|---|
| Shared Rust gateway | core wire codecs and gateway HTTP/SSE tests in crates/core/tests and crates/gateway/tests/e2e.rs | Recorded real-agent fixtures, broader multimodal/tool/stream conformance, cancellation and disconnect cases |
| Agent adapters | Shared magpie-agents adapters for Claude Code, Codex, Crush, Gemini CLI, Goose, OpenCode and Zed; private staged preview, revision-checked apply, OS lock, schema-2 journaled selective field restore preserving unrelated edits, disconnect, managed-field drift and explicit recovery. Agent unit, HTTP, sandboxed child CLI and real-gateway console browser tests; Windows ACL/junction checks; recursive duplicate-key JSON refusal and seven-Agent selective restore through HTTP/CLI/browser | JSON/JSONC lossless editing, richer detection/project overrides and real-version compatibility evidence |
| Provider metadata and keys | Protected provider CRUD and console; providers.rs covers saved/live edits. provider_tools.rs covers four-protocol catalogs/probes, pagination, saved/draft/no-auth credentials and bounded read-only operations | Capability metadata, provider presets, specialized/stream-only probes and paid/live vendor evidence |
| Hot edits and key rotation | ConfigStore saves before publishing; requests own snapshots. Versioned key schedules ignore stale requests. providers.rs checks an old in-flight request after a smaller pool is saved; config.rs checks stale scheduler calls | Cross-process locking, client revision conflicts and idempotency; encrypted secret storage with recovery |
| Routing and catalog | Explicit provider/model, listed bare model, default provider, ordered fallback groups; groups.rs and e2e.rs | Rotation/load/usage policies, conversation affinity, richer model metadata and dynamic discovery |
| Usage | Sanitized JSONL history, recent stats, key slots/cooldowns and token totals; usage.rs and e2e.rs | Date-range queries, cost/pricing/quota, partial/unknown usage and persisted aggregates |
| Interface | Dependency-free protected console for Agent lifecycle, providers, routing groups, catalog and stats; browser exercised CRUD, draft/saved discovery and probes, errors, key/list clearing, busy controls, delete protection, Escape/focus and 390px layout; tests/browser/agents.cjs exercises all seven adapters, restoration/unrelated-edit preservation/drift/duplicate-JSON refusal/preview/recovery/session/busy flows | History filtering; replacement interface clients |
| Management security and persistence | Loopback server, bearer/header auth, atomic TOML save, secret-free metadata responses, external-edit conflict detection | Separate agent/admin scopes, versioned API, provider/group revision conflicts, encrypted secret store, schema migration and broader durable operations (agent operations now have revision/preview and journaled recovery) |

Provider workflows were compared with upstream internal/gui/providers.go and docs/subsystems/providers-accounts.md: ordinary API providers support lifecycle editing, protocol choice, model selection and connection checks. Lite now covers lifecycle editing, manual/discovered model lists and selected-protocol generation checks on saved or unsaved drafts. The new provider-tools suite uses local HTTP fixtures and checks time/byte/model/page/key bounds, no redirects, sanitized errors, unchanged config/statistics and untouched live rotation/cooldowns. Browser automation exercised both saved-key reuse and unsaved drafts; this is not evidence of paid live vendor compatibility.

Console lifecycle evidence is reproducible with tests/browser/agents.cjs using a
real child gateway, temporary Agent home and no inference traffic. The browser
suite passed on Windows/Edge and automatically checked 390px overflow/focus and
control sizes. Recovery is a synthetic journal fixture; some error/busy/session
responses are injected. Captured screenshots are not evidence of manual visual
inspection or real Agent-version compatibility.

Next implementation sequence:

1. Continue agent lifecycle parity: JSON/JSONC format-preserving editing and richer detection/project semantics. Selective field restoration and shared backend/CLI/console preview, journal, apply and recovery are implemented; see docs/agent-lifecycle.md for exact limits.
2. Routing policies and protocol fixture coverage across the existing four adapters.
3. Usage/history management and stronger management API concurrency/authentication contracts.

Subscriptions, plugin hosting, cloud sync and upstream platform integrations are outside this initial core scope. A green workspace suite supports only the behaviors it exercises; it does not prove those wider proposal gates.
