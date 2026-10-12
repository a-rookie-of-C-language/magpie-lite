//! Protected agent lifecycle operations. Files and journals are accessed only in blocking workers.
use crate::server::AppState;
use crate::upstream;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use fs2::FileExt;
use magpie_agents::lifecycle::{Action, AgentId, Error, ErrorKind, Gateway, Manager};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

pub struct AgentService {
    pub manager: Arc<Manager>,
    pub port: u16,
    pub state_dir: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    action: Action,
    model: Option<String>,
    revision: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AgentIdempotencyEntry {
    scope: String,
    key: String,
    digest: String,
    result: Option<Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AgentIdempotencyLedger {
    entries: Vec<AgentIdempotencyEntry>,
}

const AGENT_IDEMPOTENCY_RETENTION: usize = 256;

struct AgentApplyInput<'a> {
    id: AgentId,
    action: Action,
    model: Option<&'a str>,
    revision: &'a str,
    gateway: Gateway<'a>,
    request: crate::config_store::IdempotencyRequest,
}
fn agent_error(kind: ErrorKind, message: &'static str) -> Error {
    Error { kind, message }
}

fn agent_idempotency_path(state_dir: &FsPath) -> PathBuf {
    state_dir.join("agent-idempotency.json")
}

fn agent_idempotency_lock_path(state_dir: &FsPath) -> PathBuf {
    state_dir.join("agent-idempotency.lock")
}

fn acquire_agent_idempotency_lock(state_dir: &FsPath) -> Result<File, Error> {
    let path = agent_idempotency_lock_path(state_dir);
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|_| agent_error(ErrorKind::Unavailable, "cannot open agent idempotency lock"))?;
    lock.try_lock_exclusive().map_err(|_| {
        agent_error(
            ErrorKind::Conflict,
            "another agent idempotency operation is in progress; retry shortly",
        )
    })?;
    Ok(lock)
}

fn load_agent_idempotency(state_dir: &FsPath) -> Result<AgentIdempotencyLedger, Error> {
    let path = agent_idempotency_path(state_dir);
    if !path.exists() {
        return Ok(AgentIdempotencyLedger::default());
    }
    let bytes = std::fs::read(path).map_err(|_| {
        agent_error(
            ErrorKind::Unavailable,
            "cannot read agent idempotency ledger",
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        agent_error(
            ErrorKind::Unavailable,
            "cannot parse agent idempotency ledger",
        )
    })
}

fn save_agent_idempotency(
    state_dir: &FsPath,
    ledger: &AgentIdempotencyLedger,
) -> Result<(), Error> {
    let path = agent_idempotency_path(state_dir);
    let mut file = tempfile::NamedTempFile::new_in(state_dir).map_err(|_| {
        agent_error(
            ErrorKind::Unavailable,
            "cannot create agent idempotency ledger",
        )
    })?;
    serde_json::to_writer_pretty(file.as_file_mut(), ledger).map_err(|_| {
        agent_error(
            ErrorKind::Unavailable,
            "cannot serialize agent idempotency ledger",
        )
    })?;
    file.as_file().sync_all().map_err(|_| {
        agent_error(
            ErrorKind::Unavailable,
            "cannot flush agent idempotency ledger",
        )
    })?;
    file.persist(path).map_err(|_| {
        agent_error(
            ErrorKind::Unavailable,
            "cannot save agent idempotency ledger",
        )
    })?;
    Ok(())
}

fn apply_with_idempotency(
    manager: &Manager,
    state_dir: &FsPath,
    input: AgentApplyInput<'_>,
) -> Result<Value, Error> {
    let AgentApplyInput {
        id,
        action,
        model,
        revision,
        gateway,
        request,
    } = input;
    let _lock = acquire_agent_idempotency_lock(state_dir)?;
    let mut ledger = load_agent_idempotency(state_dir)?;
    if let Some(entry) = ledger
        .entries
        .iter()
        .find(|entry| entry.scope == request.scope && entry.key == request.key)
    {
        if entry.digest != request.digest {
            return Err(agent_error(
                ErrorKind::Conflict,
                "idempotency key was already used for a different request",
            ));
        }
        return entry.result.clone().ok_or_else(|| {
            agent_error(
                ErrorKind::Conflict,
                "idempotent agent operation is incomplete; inspect agent status before retrying",
            )
        });
    }

    let entry_index = ledger.entries.len();
    ledger.entries.push(AgentIdempotencyEntry {
        scope: request.scope,
        key: request.key,
        digest: request.digest,
        result: None,
    });
    save_agent_idempotency(state_dir, &ledger)?;

    let result = encode(manager.apply(id, action, model, revision, gateway)?);
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            ledger.entries.pop();
            if save_agent_idempotency(state_dir, &ledger).is_err() {
                return Err(agent_error(
                    ErrorKind::Unavailable,
                    "agent operation failed and idempotency recovery is required",
                ));
            }
            return Err(error);
        }
    };

    ledger.entries[entry_index].result = Some(value.clone());
    if ledger.entries.len() > AGENT_IDEMPOTENCY_RETENTION {
        let remove = ledger.entries.len() - AGENT_IDEMPOTENCY_RETENTION;
        ledger.entries.drain(..remove);
    }
    save_agent_idempotency(state_dir, &ledger).map_err(|_| {
        agent_error(
            ErrorKind::Unavailable,
            "agent operation applied but idempotency recovery is required before retrying",
        )
    })?;
    Ok(value)
}
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(upstream::chat_error(status, message))).into_response()
}
async fn run(
    state: Arc<AppState>,
    work: impl FnOnce(
        &Manager,
        &crate::config::Config,
        u16,
    ) -> Result<Value, magpie_agents::lifecycle::Error>
    + Send
    + 'static,
) -> Response {
    let Some(service) = &state.agents else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent management is not configured",
        );
    };
    let manager = service.manager.clone();
    let port = service.port;
    let config = state.config.current();
    match tokio::task::spawn_blocking(move || work(&manager, &config, port)).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => error(
            match e.kind {
                ErrorKind::Invalid => StatusCode::BAD_REQUEST,
                ErrorKind::Conflict => StatusCode::CONFLICT,
                ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            },
            e.message,
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "agent worker failed; check status before retrying",
        ),
    }
}
async fn run_apply(
    state: Arc<AppState>,
    request: crate::config_store::IdempotencyRequest,
    work: impl FnOnce(
        &Manager,
        &crate::config::Config,
        u16,
        &FsPath,
        crate::config_store::IdempotencyRequest,
    ) -> Result<Value, magpie_agents::lifecycle::Error>
    + Send
    + 'static,
) -> Response {
    let Some(service) = &state.agents else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent management is not configured",
        );
    };
    let manager = service.manager.clone();
    let port = service.port;
    let state_dir = service.state_dir.clone();
    let config = state.config.current();
    match tokio::task::spawn_blocking(move || work(&manager, &config, port, &state_dir, request))
        .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => error(
            match e.kind {
                ErrorKind::Invalid => StatusCode::BAD_REQUEST,
                ErrorKind::Conflict => StatusCode::CONFLICT,
                ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            },
            e.message,
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "agent worker failed; check status before retrying",
        ),
    }
}
fn encode(value: impl serde::Serialize) -> Result<Value, magpie_agents::lifecycle::Error> {
    serde_json::to_value(value).map_err(|_| magpie_agents::lifecycle::Error {
        kind: ErrorKind::Unavailable,
        message: "cannot encode agent status",
    })
}
fn validate_model(
    edit: &Edit,
    config: &crate::config::Config,
) -> Result<(), magpie_agents::lifecycle::Error> {
    if edit.action == Action::Connect
        && !edit
            .model
            .as_deref()
            .is_some_and(|model| config.resolve(model).is_some())
    {
        return Err(magpie_agents::lifecycle::Error {
            kind: ErrorKind::Invalid,
            message: "model does not resolve to a configured provider or routing group",
        });
    }
    Ok(())
}
pub async fn list(State(state): State<Arc<AppState>>) -> Response {
    run(state, |manager, _, _| {
        Ok(json!({"agents": manager.statuses()?}))
    })
    .await
}
pub async fn preview(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Response {
    let Ok(id) = serde_json::from_value::<AgentId>(json!(id)) else {
        return error(StatusCode::NOT_FOUND, "unsupported agent");
    };
    let Ok(edit) = serde_json::from_slice::<Edit>(&bytes) else {
        return error(
            StatusCode::BAD_REQUEST,
            "expected agent action and optional model JSON",
        );
    };
    run(state, move |manager, config, port| {
        validate_model(&edit, config)?;
        encode(manager.preview(
            id,
            edit.action,
            edit.model.as_deref(),
            Gateway {
                port,
                token: &config.token,
            },
        )?)
    })
    .await
}
pub async fn apply(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let scope = format!("agent.apply:{id}");
    let request = match crate::server::idempotency_request(&state.config, &headers, &scope, &bytes)
    {
        Ok(value) => value,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let Ok(id) = serde_json::from_value::<AgentId>(json!(id)) else {
        return error(StatusCode::NOT_FOUND, "unsupported agent");
    };
    let Ok(edit) = serde_json::from_slice::<Edit>(&bytes) else {
        return error(
            StatusCode::BAD_REQUEST,
            "expected agent action, model and preview revision JSON",
        );
    };
    let Some(revision) = edit
        .revision
        .filter(|r| r.len() == 64 && r.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "apply requires a valid preview revision",
        );
    };
    let edit = Edit {
        revision: None,
        ..edit
    };
    if let Some(request) = request {
        run_apply(
            state,
            request,
            move |manager, config, port, state_dir, request| {
                validate_model(&edit, config)?;
                apply_with_idempotency(
                    manager,
                    state_dir,
                    AgentApplyInput {
                        id,
                        action: edit.action,
                        model: edit.model.as_deref(),
                        revision: &revision,
                        gateway: Gateway {
                            port,
                            token: &config.token,
                        },
                        request,
                    },
                )
            },
        )
        .await
    } else {
        run(state, move |manager, config, port| {
            validate_model(&edit, config)?;
            encode(manager.apply(
                id,
                edit.action,
                edit.model.as_deref(),
                &revision,
                Gateway {
                    port,
                    token: &config.token,
                },
            )?)
        })
        .await
    }
}
pub async fn recover(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Ok(id) = serde_json::from_value::<AgentId>(json!(id)) else {
        return error(StatusCode::NOT_FOUND, "unsupported agent");
    };
    run(state, move |manager, _, _| encode(manager.recover(id)?)).await
}
