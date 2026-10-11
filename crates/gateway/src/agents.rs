//! Protected agent lifecycle operations. Files and journals are accessed only in blocking workers.
use crate::server::AppState;
use crate::upstream;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use magpie_agents::lifecycle::{Action, AgentId, ErrorKind, Gateway, Manager};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct AgentService {
    pub manager: Arc<Manager>,
    pub port: u16,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    action: Action,
    model: Option<String>,
    revision: Option<String>,
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
    bytes: Bytes,
) -> Response {
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
pub async fn recover(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Ok(id) = serde_json::from_value::<AgentId>(json!(id)) else {
        return error(StatusCode::NOT_FOUND, "unsupported agent");
    };
    run(state, move |manager, _, _| encode(manager.recover(id)?)).await
}
