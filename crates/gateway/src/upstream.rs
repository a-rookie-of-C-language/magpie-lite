//! The upstream HTTP client: one place that knows each protocol's URL
//! shape and auth headers.

use magpie_core::ir::CompletionRequest;
use magpie_core::{anthropic, chat, gemini, responses};
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
    let mut body = match p.protocol {
        Protocol::Chat => chat::encode_request(req, model),
        Protocol::Anthropic => anthropic::encode_request(req, model),
        Protocol::Responses => responses::encode_request(req, model),
        Protocol::Gemini => gemini::encode_request(req, model),
    };
    // Gemini names the model in the request URL, not in the JSON body.
    if p.protocol == Protocol::Gemini
        && let Some(object) = body.as_object_mut()
    {
        object.remove("model");
    }
    body
}

/// POST the body upstream; the URL shape and auth follow the protocol.
pub async fn send(
    http: &reqwest::Client,
    p: &ProviderConfig,
    body: &Value,
    model: &str,
    stream: bool,
) -> anyhow::Result<reqwest::Response> {
    let base = p.base_url.trim_end_matches('/');
    let mut req = match p.protocol {
        Protocol::Chat => http.post(format!("{base}/chat/completions")),
        Protocol::Anthropic => http.post(format!("{base}/v1/messages")),
        Protocol::Responses => http.post(format!("{base}/responses")),
        Protocol::Gemini => {
            let method = if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            };
            http.post(format!("{base}/models/{}:{method}", path_escape(model)))
        }
    };
    match p.protocol {
        Protocol::Chat | Protocol::Responses => {
            req = req.bearer_auth(&p.api_key);
        }
        Protocol::Anthropic => {
            req = req
                .bearer_auth(&p.api_key)
                .header("anthropic-version", "2023-06-01");
        }
        Protocol::Gemini => {
            req = req.header("x-goog-api-key", &p.api_key);
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

/// An error body in the Gemini generateContent shape.
pub fn gemini_error(status: reqwest::StatusCode, message: &str) -> Value {
    let reason = match status.as_u16() {
        400 => "INVALID_ARGUMENT",
        401 | 403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        500 | 502 | 503 | 504 => "UNAVAILABLE",
        _ => "UNKNOWN",
    };
    json!({"error": {"code": status.as_u16(), "message": message, "status": reason}})
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
/// Percent-encode a model for one path segment.
fn path_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// An error body in the OpenAI Responses shape.
pub fn responses_error(status: reqwest::StatusCode, message: &str) -> Value {
    json!({"error": {"message": message, "type": "upstream_error", "code": status.as_u16()}})
}
