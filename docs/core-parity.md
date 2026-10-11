# Core parity tracker

Updated: 2026-10-12. Reference checkout: `D:\code\magpie`; implementation: `D:\code\magpie-lite`.

Target: multiple coding agents sharing one Rust backend, provider adapters for different services, and an interface managing providers, models, routing and usage. The reference rust-core-refactor-plan.md, rust-core-contracts.md and rust-core-admin-api.md are proposals with broader acceptance gates. This tracker does not claim G1–G16 completion or full Magpie product parity.

| Area | Current evidence | Remaining work |
|---|---|---|
| Shared Rust gateway | core wire codecs and gateway HTTP/SSE tests in crates/core/tests and crates/gateway/tests/e2e.rs | Recorded real-agent fixtures, broader multimodal/tool/stream conformance, cancellation and disconnect cases |
| Agent adapters | CLI adapters for Claude Code, Codex, Crush, Gemini CLI, Goose, OpenCode and Zed; isolated config tests in crates/cli | Detection, preview, journaled backup/restore, disconnect, drift and recovery; real-version compatibility evidence |
| Provider metadata and keys | Protected provider CRUD API and console; crates/gateway/tests/providers.rs checks persisted/live updates, preserve/replace/clear credentials, default selection, dependency checks and concurrent edits | Model discovery, saved/unsaved connection tests, capability metadata and provider presets |
| Hot edits and key rotation | ConfigStore saves before publishing; requests own snapshots. Versioned key schedules ignore stale requests. providers.rs checks an old in-flight request after a smaller pool is saved; config.rs checks stale scheduler calls | Cross-process locking, client revision conflicts and idempotency; encrypted secret storage with recovery |
| Routing and catalog | Explicit provider/model, listed bare model, default provider, ordered fallback groups; groups.rs and e2e.rs | Rotation/load/usage policies, conversation affinity, richer model metadata and dynamic discovery |
| Usage | Sanitized JSONL history, recent stats, key slots/cooldowns and token totals; usage.rs and e2e.rs | Date-range queries, cost/pricing/quota, partial/unknown usage and persisted aggregates |
| Interface | Dependency-free protected console for providers, routing groups, catalog and stats; browser exercised CRUD, errors, key clearing, delete protection, Escape/focus and 390px layout | Agent management, model discovery/probe controls, history filtering; replacement interface clients |
| Management security and persistence | Loopback server, bearer/header auth, atomic TOML save, secret-free metadata responses, external-edit conflict detection | Separate agent/admin scopes, versioned API, revision/preview operations, secret store, schema migration, durable operations and recovery |

Provider workflows were compared with upstream internal/gui/providers.go and docs/subsystems/providers-accounts.md: ordinary API providers support lifecycle editing, protocol choice, model selection and connection checks. Lite now covers lifecycle editing and manual model lists; it does not yet cover live discovery or probes.

Next implementation sequence:

1. Provider model discovery and connection checks with bounded requests and no config mutation until Save.
2. Agent lifecycle through the shared backend: preview, backup, connect, restore/disconnect and drift reporting.
3. Routing policies and protocol fixture coverage across the existing four adapters.
4. Usage/history management and stronger management API concurrency/authentication contracts.

Subscriptions, plugin hosting, cloud sync and upstream platform integrations are outside this initial core scope. A green workspace suite supports only the behaviors it exercises; it does not prove those wider proposal gates.
