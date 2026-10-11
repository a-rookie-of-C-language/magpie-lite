//! The upstream HTTP client: one place that knows each protocol's URL
//! shape and auth headers.

use magpie_core::ir::CompletionRequest;
use magpie_core::{anthropic, chat, responses};
use serde_json::{Value, json};

use crate::config::{Protocol, ProviderConfig};

/// A configured HTTP client (connection pooling, no total timeout: streams
/// may run for a long while).
pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()?)
}

/// Build the upstream request body for a provider, translating from the IR
/// and renaming the model to the upstream's spelling.
pub fn build_request(p: &ProviderConfig, req: &CompletionRequest, model: &str) -> Value {
    match p.protocol {
        Protocol::Chat => chat::encode_request(req, model),
        Protocol::Anthropic => anthropic::encode_request(req, model),
        Protocol::Responses => responses::encode_request(req, model),
    }
}

/// POST the body upstream; the URL shape and auth follow the protocol.
pub async fn send(
    http: &reqwest::Client,
    p: &ProviderConfig,
    body: &Value,
) -> anyhow::Result<reqwest::Response> {
    let base = p.base_url.trim_end_matches('/');
    let mut req = match p.protocol {
        Protocol::Chat => http.post(format!("{base}/chat/completions")),
        Protocol::Anthropic => http.post(format!("{base}/v1/messages")),
        Protocol::Responses => http.post(format!("{base}/responses")),
    };
    match p.protocol {
        Protocol::Chat => {
            req = req.bearer_auth(&p.api_key);
        }
        Protocol::Anthropic => {
            req = req
                .bearer_auth(&p.api_key)
                .header("anthropic-version", "2023-06-01");
        }
        Protocol::Responses => {
            req = req.bearer_auth(&p.api_key);
        }
    }
    let resp = req.json(body).send().await?;
    Ok(resp)
}

/// Pull a readable message out of an upstream error body. Takes the
/// response: reading its body consumes it, and callers only want the
/// status after this.
pub async fn error_detail(resp: reqwest::Response) -> String {
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    if bytes.is_empty() {
        return format!("upstream {status}");
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => {
            let inner = v
                .pointer("/error/message")
                .or_else(|| v.pointer("/message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or(text);
            format!("upstream {status}: {inner}")
        }
        Err(_) => format!("upstream {status}: {text}"),
    }
}

/// An error body in the Chat Completions shape.
pub fn chat_error(status: reqwest::StatusCode, message: &str) -> Value {
    json!({"error": {"message": message, "type": "upstream_error", "code": status.as_u16()}})
}

/// An error body in the Anthropic Messages shape.
pub fn anthropic_error(status: reqwest::StatusCode, message: &str) -> Value {
    let kind = match status.as_u16() {
        401 | 403 => "authentication_error",
        429 => "rate_limit_error",
        404 => "not_found_error",
        _ => "api_error",
    };
    json!({"type": "error", "error": {"type": kind, "message": message}})
}
/// An error body in the OpenAI Responses shape.
pub fn responses_error(status: reqwest::StatusCode, message: &str) -> Value {
    json!({"error": {"message": message, "type": "upstream_error", "code": status.as_u16()}})
}
