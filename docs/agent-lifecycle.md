# Agent lifecycle

The shared `magpie-agents` crate owns all seven adapters. Both CLI and authenticated
HTTP management run its preview, revision check, journal, apply and recovery flow;
there is no separate CLI implementation. The protected console uses the same HTTP
operations. None of these operations run agent binaries.

## CLI

```powershell
magpie-lite agent list
magpie-lite agent preview claude deepseek/deepseek-chat
magpie-lite agent connect claude deepseek/deepseek-chat --revision <preview-revision>
magpie-lite agent preview claude --disconnect
magpie-lite agent disconnect claude --revision <preview-revision>
magpie-lite agent recover claude
```

IDs: `claude`, `codex`, `crush`, `gemini`, `goose`, `opencode`, `zed`.
Connect accepts the same provider/model, bare-model and group/<id> routes as the
gateway. A route must resolve in the current configuration; it need not be in a
provider's manually listed catalog. List and preview output JSON metadata, never
configuration contents or credentials.

Without `--revision`, connect/disconnect immediately preview and apply under a
fresh revision check. The existing `claude <model>`, `codex <model>` and other
agent shorthand commands use this same journaled connect flow. `ls` includes
managed connection, drift and recovery state. Disconnect and recover do not
require a readable gateway TOML file or a running server.

The CLI uses the configured port. If the running server uses `serve --port` to
override it, use HTTP management on that server to connect: HTTP operations use
the actual listening port (including an OS-assigned port requested with `0`).

## HTTP

All endpoints require the gateway token, exactly like `/config` and inference.
No endpoint accepts arbitrary configuration paths or reads backup contents back
to the caller.

| Method | Endpoint | Body / response |
|---|---|---|
| GET | `/agents` | `{"agents":[...]}`: detection, connection/model, drift, pending recovery, managed paths and restart notice |
| POST | `/agents/{id}/preview` | `{"action":"connect","model":"provider/model"}` or `{"action":"disconnect"}`; returns revision and file create/update/delete/unchanged plus before/after byte counts |
| POST | `/agents/{id}/apply` | Same action/model plus `"revision":"<64-hex-preview-revision>"`; returns resulting status |
| POST | `/agents/{id}/recover` | No body required; rolls back a pending connect or finishes a pending disconnect |

Preview runs the actual adapter against private staged copies. It does **not**
change the agent's real files, but creates/uses local state, locking and temporary
staging files. Apply recalculates the preview while holding the shared operation
lock. A changed file, selected path, model, gateway endpoint/token or journal
invalidates its revision. Inputs return sanitized 400 errors, unknown IDs 404,
stale revisions/drift/lock contention 409, and inaccessible state 503.

## Console workflow

Open the local gateway console and enter its token. **Coding agents** displays
all seven supported adapters, including Agents not detected locally. Detection
is a local config/PATH hint, not proof that an Agent is running or reachable.

1. Click **Manage**. The editor lists managed paths and Agent-specific restart/
   override instructions. Zed also shows its manual credential requirement.
2. Choose **Connect / change model** and use the model-route suggestions, or
   manually enter `provider/model` or `group/id`. Unknown providers/groups are
   rejected; a configured provider may accept an unlisted model.
3. Click **Preview changes**, review create/update/delete/unchanged operations
   and byte counts, and click **Confirm connection**. Preview contains no file
   contents. There is no automatic apply or retry.
4. Choose **Disconnect / restore backup**, then preview and confirm restoration.
   A drift warning does not blindly disable this action: an OpenCode overriding
   path can coexist with safe restoration of the originally managed file.
5. An interrupted operation disables connect/disconnect and shows **Recover
   interrupted operation**. Its confirmation explains rollback versus finishing
   restoration; the backend checks the journal and current files before writing.

Form edits, changed Agent status/routing metadata, route mutations, closing the
editor and sign-out invalidate previews. Backend revision checks are still the
final authority for changes invisible to the UI (credentials, files or port).
Provider/group/Agent operations share one UI busy gate; late responses from an
old refresh/session cannot republish data. Escape closes an idle editor and
returns focus to its Agent button. Failed operations retain the route input and
require a new preview; failed status reads disable Agent actions while leaving
provider/statistics panels usable. Requests have a 30-second browser deadline;
if a write times out, refresh status rather than assuming it was rolled back.

## Backup and recovery

Each configuration directory contains an `agent-state` directory beside the
TOML file. CLI and gateway processes use the same OS operation lock and journals
there. A journal preserves the bytes before the **first** connect; rewiring does
not replace that original baseline. A disconnect restores those bytes exactly
or removes a file that did not originally exist, provided no later drift exists.
Codex comments, Gemini dotenv comments/newlines and foreign provider files are
therefore restored, not reconstructed from parsed fields.

Gemini, Goose and split-layout Crush may change two files; Windows Crush can use
one shared file. A durable pending journal is saved before publishing any of
them. Per-file replacement is atomic; the **multi-file operation is not**.
An interruption leaves `recovery_needed=true`:

- Pending first connect: restore the pre-connect files.
- Pending rewire: restore the previous managed connection/model, retaining the
  original pre-connection baseline for a subsequent disconnect.
- Pending disconnect: finish restoring the baseline.

Recovery checks all files first and rechecks each immediately before writing.
If a file matches neither the recorded operation nor its recovery image, recovery
refuses to overwrite it. Preserve the journal and resolve that edit before retrying;
do not delete backups to dismiss an error.

## Safety and limitations

- Journals and staged copies contain **plaintext credentials** (base64 is not
  encryption), including original credentials. Disconnected journals retain
  their original baseline. Treat this directory as sensitive backup material;
  no encrypted secret store or automatic retention/purge policy exists yet.
- Unix state directories use mode 700; journal and connected config files use
  600. Disconnect restores the recorded original file mode. Unix-specific tests
  are present but are not evidence of execution on this Windows development host.
- Windows state/new secret files get a protected current-user-only DACL. Existing
  agent file ACLs are preserved on replacement/restoration; pre-existing broad
  ACLs are not automatically narrowed. Windows ACL tests verify both behaviors.
- Symlinks and Windows reparse points in managed paths are refused. Each agent
  file is limited to 4 MiB, journals to 64 MiB. Adapter and HTTP errors never echo
  file contents, upstream keys or the gateway token.
- Whole-file changes after connect are reported as drift and block reconnect or
  disconnect. **Field-level restoration that preserves subsequent unrelated
  user edits is still pending**, unlike upstream Magpie. External editors do not
  participate in our lock; immediate checks are not a filesystem-wide CAS.
- OpenCode's JSONC/JSON/legacy precedence is checked live. A new overriding file
  marks an existing connection drifted; disconnect/recovery can still restore
  the originally managed path, including after restart. JSONC wiring is refused
  rather than dropping comments or writing an ignored JSON file beside it.
- Detection checks configuration files/folders and binary names on this process's
  PATH. It does not inspect login-shell/off-PATH installations, running processes,
  project overrides or reachability. Restart the agent after changing settings.
- Zed still needs its token in its environment/provider settings; no credential is
  written to its JSON. Real-agent-version compatibility is not yet verified.
- Agent-specific model catalogs, selective restore, broader protocol fixtures
  and real-agent execution evidence remain on the parity tracker. Console route
  suggestions come from gateway providers/groups, not a running Agent.

## Verification

```powershell
cargo test -p magpie-agents
cargo test -p magpie-gateway --test agents
cargo test -p magpie-lite --test agent_lifecycle
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Tests use `Manager::at_home` with explicit temporary directories, or child CLI
processes with `MAGPIE_LITE_HOME` pointing to a temporary home. They never mutate
process-wide home variables or target the user's actual agent configuration.

### Reproducible browser checks

```powershell
cargo build -p magpie-lite
# If Playwright is not resolvable through Node's module path:
$env:PLAYWRIGHT_MODULE = '<absolute Playwright module directory>'
# Optional: use an already installed Chromium browser.
$env:PLAYWRIGHT_EXECUTABLE_PATH = '<absolute Chromium/Edge executable>'
node --test tests/browser/agents.cjs
```

`MAGPIE_LITE_BIN` optionally overrides the debug gateway binary;
`MAGPIE_LITE_SCREENSHOTS` optionally saves desktop and 390px editor screenshots
outside the disposable sandbox. No frontend runtime dependency is added to the
Rust distribution; Playwright is only needed to run this browser suite.

The suite exercises actual HTTP preview/apply endpoints for every adapter:
original-byte restoration, missing-file creation/removal, rewiring via a group,
stale revisions, refused whole-file drift and OpenCode overriding paths. It also
checks model suggestions, form/preview invalidation, provider/group CRUD and
delete-reference protection, busy coordination in both directions, errors,
session expiry, late old-session responses, Escape/focus,
secret-free rendered text/forms, 44px controls and 390px page overflow.

Recovery uses a **synthetic pending-disconnect journal** in the temporary state
directory, not a real process crash. Agent-status failure, authentication expiry
and provider-tool busy coordination use explicit browser response fixtures;
the timeout check accelerates one browser timer. These are UI coordination
checks, not live vendor or real Agent execution evidence. Screenshots were
captured on Windows/Edge; layout/focus were measured automatically. Manual visual
inspection was unavailable on this development host, and is not claimed.
