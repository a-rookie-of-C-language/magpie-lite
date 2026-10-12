//! Bounded, read-only operations on saved or unsaved provider drafts.
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use magpie_core::ir::{CompletionRequest, Message, Part, Role};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{Protocol, ProviderConfig};
use crate::providers::ProviderEdit;
use crate::server::AppState;
use crate::upstream;

const TIME_LIMIT: Duration = Duration::from_secs(30);
const BYTE_LIMIT: usize = 1024 * 1024;
const PAGE_LIMIT: usize = 20;
const MODEL_LIMIT: usize = 5000;
const KEY_LIMIT: usize = 8;
type ToolError = (StatusCode, String);
type ToolResult<T> = Result<T, ToolError>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolInput {
    provider: ProviderEdit,
    model: Option<String>,
}

fn bad(message: &str) -> ToolError {
    (StatusCode::BAD_REQUEST, message.to_owned())
}
fn failed(message: &str) -> ToolError {
    (StatusCode::BAD_GATEWAY, message.to_owned())
}
fn network(error: reqwest::Error) -> ToolError {
    if error.is_timeout() {
        (
            StatusCode::GATEWAY_TIMEOUT,
            "provider request timed out".to_owned(),
        )
    } else {
        failed("could not reach provider; check the base URL and TLS configuration")
    }
}
fn finish(result: ToolResult<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err((status, message)) => {
            (status, Json(upstream::chat_error(status, &message))).into_response()
        }
    }
}

fn input(state: &AppState, bytes: &[u8]) -> ToolResult<(ProviderConfig, Option<String>)> {
    let input: ToolInput = serde_json::from_slice(bytes)
        .map_err(|_| bad("expected provider draft JSON with optional test model"))?;
    let provider = input.provider.materialize(&state.config.current());
    provider
        .validate()
        .map_err(|error| bad(&error.to_string()))?;
    Ok((provider, input.model))
}

fn client() -> ToolResult<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| failed("could not create provider HTTP client"))
}

async fn bounded<T>(future: impl std::future::Future<Output = ToolResult<T>>) -> ToolResult<T> {
    tokio::time::timeout(TIME_LIMIT, future)
        .await
        .unwrap_or_else(|_| {
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                "provider operation exceeded 30 seconds".to_owned(),
            ))
        })
}

/// Read in chunks; a dishonest or absent Content-Length cannot bypass the limit.
async fn json_body(mut response: reqwest::Response, remaining: &mut usize) -> ToolResult<Value> {
    if response
        .content_length()
        .is_some_and(|length| length > *remaining as u64)
    {
        return Err(failed(
            "provider response exceeded the 1 MiB operation limit",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if chunk.len() > *remaining {
            return Err(failed(
                "provider response exceeded the 1 MiB operation limit",
            ));
        }
        *remaining -= chunk.len();
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| failed("provider returned invalid JSON"))
}

fn status_error(status: StatusCode) -> ToolError {
    // Never relay an upstream body, Location, URL or reqwest error containing credentials.
    failed(&format!("provider returned HTTP {}", status.as_u16()))
}
fn retry(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        401 | 402 | 403 | 429 | 500 | 502 | 503 | 504
    )
}

pub async fn models(State(state): State<Arc<AppState>>, bytes: Bytes) -> Response {
    let (provider, _) = match input(&state, &bytes) {
        Ok(input) => input,
        Err(error) => return finish(Err(error)),
    };
    finish(
        bounded(discover(&provider))
            .await
            .map(|models| json!({"models":models})),
    )
}

async fn discover(provider: &ProviderConfig) -> ToolResult<Vec<String>> {
    let http = client()?;
    let mut remaining = BYTE_LIMIT;
    let mut last_error = failed("no provider credentials were attempted");
    for key in provider.api_key_pool().into_iter().take(KEY_LIMIT) {
        match catalog(&http, provider, &key, &mut remaining).await {
            Ok(models) => return Ok(models),
            Err((error, true)) => last_error = error,
            Err((error, false)) => return Err(error),
        }
    }
    Err(last_error)
}

// A failed credential discards its partial catalog before the next is attempted.
async fn catalog(
    http: &reqwest::Client,
    provider: &ProviderConfig,
    key: &str,
    remaining: &mut usize,
) -> Result<Vec<String>, (ToolError, bool)> {
    let base = provider.base_url.trim_end_matches('/');
    let url = match provider.protocol {
        Protocol::Anthropic => format!("{base}/v1/models"),
        _ => format!("{base}/models"),
    };
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let keys = provider.api_key_pool();
    for _ in 0..PAGE_LIMIT {
        let mut request = http.get(&url);
        if let Some(cursor) = &cursor {
            request = request.query(&[(
                if provider.protocol == Protocol::Gemini {
                    "pageToken"
                } else {
                    "after_id"
                },
                cursor,
            )]);
        }
        let response = upstream::authenticate(request, provider.protocol, key)
            .send()
            .await
            .map_err(|error| (network(error), false))?;
        let status = response.status();
        if !status.is_success() {
            return Err((status_error(status), retry(status)));
        }
        let value = json_body(response, remaining)
            .await
            .map_err(|error| (error, false))?;
        let field = if provider.protocol == Protocol::Gemini {
            "models"
        } else {
            "data"
        };
        let list = value
            .get(field)
            .and_then(Value::as_array)
            .ok_or_else(|| (failed("provider returned an invalid model catalog"), false))?;
        for model in list {
            let id = if provider.protocol == Protocol::Gemini {
                let methods = model
                    .get("supportedGenerationMethods")
                    .and_then(Value::as_array)
                    .ok_or_else(|| (failed("provider returned an invalid Gemini model"), false))?;
                if !methods.iter().any(|method| method == "generateContent") {
                    continue;
                }
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(|name| name.strip_prefix("models/"))
            } else {
                model.get("id").and_then(Value::as_str)
            }
            .ok_or_else(|| {
                (
                    failed("provider returned an invalid model identifier"),
                    false,
                )
            })?;
            if !valid_model(id) || keys.iter().any(|key| !key.is_empty() && id.contains(key)) {
                return Err((
                    failed("provider returned an invalid model identifier"),
                    false,
                ));
            }
            if seen.insert(id.to_owned()) {
                ids.push(id.to_owned());
            }
            if ids.len() > MODEL_LIMIT {
                return Err((failed("provider catalog exceeded 5000 models"), false));
            }
        }
        cursor = match provider.protocol {
            Protocol::Gemini => match value.get("nextPageToken") {
                None | Some(Value::Null) => None,
                Some(Value::String(token)) if token.is_empty() => None,
                Some(Value::String(token)) => Some(token.clone()),
                _ => {
                    return Err((
                        failed("provider returned an invalid pagination token"),
                        false,
                    ));
                }
            },
            Protocol::Anthropic => match value.get("has_more") {
                Some(Value::Bool(true)) => Some(
                    value
                        .get("last_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| (failed("provider omitted the next catalog cursor"), false))?
                        .to_owned(),
                ),
                None | Some(Value::Bool(false)) => None,
                _ => {
                    return Err((
                        failed("provider returned an invalid pagination flag"),
                        false,
                    ));
                }
            },
            _ => None,
        };
        if let Some(next) = &cursor {
            if !cursors.insert(next.clone()) {
                return Err((failed("provider repeated a catalog cursor"), false));
            }
        } else {
            return Ok(ids);
        }
    }
    Err((failed("provider catalog exceeded 20 pages"), false))
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 1024
        && !model.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub async fn test(State(state): State<Arc<AppState>>, bytes: Bytes) -> Response {
    let (provider, model) = match input(&state, &bytes) {
        Ok(input) => input,
        Err(error) => return finish(Err(error)),
    };
    let model = model
        .or_else(|| provider.models.first().cloned())
        .unwrap_or_default();
    if !valid_model(&model) {
        return finish(Err(bad(
            "choose a non-empty test model without whitespace (maximum 1024 bytes)",
        )));
    }
    // The selected model is user input, but do not echo a accidentally pasted credential.
    if provider
        .api_key_pool()
        .iter()
        .any(|key| !key.is_empty() && model.contains(key))
    {
        return finish(Err(bad("test model cannot contain provider credentials")));
    }
    let started = Instant::now();
    finish(bounded(probe(&provider, &model)).await.map(|()| {
        json!({
            "ok":true, "protocol":provider.protocol, "model":model,
            "elapsed_ms":started.elapsed().as_millis()
        })
    }))
}

async fn probe(provider: &ProviderConfig, model: &str) -> ToolResult<()> {
    let http = client()?;
    let request = CompletionRequest {
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            parts: vec![Part::text("Reply with OK.")],
        }],
        max_tokens: Some(64),
        ..Default::default()
    };
    let body = upstream::build_request(provider, &request, model)
        .map_err(|_| failed("request cannot be encoded for this provider"))?;
    let mut remaining = BYTE_LIMIT;
    let mut last_error = failed("no provider credentials were attempted");
    for key in provider.api_key_pool().into_iter().take(KEY_LIMIT) {
        let response = upstream::send(&http, provider, &key, &body, model, false)
            .await
            .map_err(|error| match error.downcast::<reqwest::Error>() {
                Ok(error) => network(error),
                Err(_) => failed("could not reach provider"),
            })?;
        let status = response.status();
        if !status.is_success() {
            last_error = status_error(status);
            if retry(status) {
                continue;
            }
            return Err(last_error);
        }
        let value = json_body(response, &mut remaining).await?;
        if valid_reply(provider.protocol, &value) {
            return Ok(());
        }
        return Err(failed(
            "provider returned an invalid generation response for the selected protocol",
        ));
    }
    Err(last_error)
}

fn valid_reply(protocol: Protocol, value: &Value) -> bool {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return false;
    }
    match protocol {
        Protocol::Chat => value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice.pointer("/message/role") == Some(&json!("assistant"))
                        && (choice
                            .pointer("/message/content")
                            .is_some_and(|v| v.is_string() || v.is_null())
                            || choice
                                .pointer("/message/tool_calls")
                                .is_some_and(Value::is_array))
                })
            }),
        Protocol::Anthropic => {
            value.get("type") == Some(&json!("message"))
                && value.get("role") == Some(&json!("assistant"))
                && value.get("content").is_some_and(Value::is_array)
        }
        Protocol::Responses => {
            value.get("object") == Some(&json!("response"))
                && matches!(
                    value.get("status").and_then(Value::as_str),
                    Some("completed" | "incomplete")
                )
                && value.get("output").is_some_and(Value::is_array)
        }
        Protocol::Gemini => value
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate
                        .pointer("/content/parts")
                        .is_some_and(Value::is_array)
                        || candidate.get("finishReason").is_some_and(Value::is_string)
                })
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn overall_deadline_stops_a_multi_request_operation() {
        let result = bounded(async {
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_secs(9)).await;
            }
            Ok(())
        })
        .await;
        assert_eq!(result.unwrap_err().0, StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn probes_reject_other_protocols_and_error_envelopes() {
        let replies = [
            (
                Protocol::Chat,
                json!({"choices":[{"message":{"role":"assistant","content":"OK"}}]}),
            ),
            (
                Protocol::Responses,
                json!({"object":"response","status":"completed","output":[]}),
            ),
            (
                Protocol::Anthropic,
                json!({"type":"message","role":"assistant","content":[]}),
            ),
            (
                Protocol::Gemini,
                json!({"candidates":[{"finishReason":"STOP"}]}),
            ),
        ];
        for (expected, reply) in &replies {
            for protocol in [
                Protocol::Chat,
                Protocol::Responses,
                Protocol::Anthropic,
                Protocol::Gemini,
            ] {
                assert_eq!(valid_reply(protocol, reply), protocol == *expected);
                let mut error = reply.clone();
                error["error"] = json!({"message":"SECRET"});
                assert!(!valid_reply(protocol, &error));
            }
        }
        assert!(!valid_reply(
            Protocol::Responses,
            &json!({"object":"response","status":"failed","output":[]})
        ));
    }
}
