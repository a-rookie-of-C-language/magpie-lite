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
    api_key: &str,
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
    req = authenticate(req, p.protocol, api_key);

    let resp = req.json(body).send().await?;
    Ok(resp)
}

/// Shared authentication for inference and administrative provider checks.
pub(crate) fn authenticate(
    mut request: reqwest::RequestBuilder,
    protocol: Protocol,
    api_key: &str,
) -> reqwest::RequestBuilder {
    match protocol {
        Protocol::Chat | Protocol::Responses => {
            if !api_key.is_empty() {
                request = request.bearer_auth(api_key);
            }
        }
        Protocol::Anthropic => {
            request = request.header("anthropic-version", "2023-06-01");
            if !api_key.is_empty() {
                request = request.header("x-api-key", api_key);
            }
        }
        Protocol::Gemini => {
            if !api_key.is_empty() {
                request = request.header("x-goog-api-key", api_key);
            }
        }
    }
    request
}

/// An upstream error response after its body has been consumed.
#[derive(Debug)]
pub struct UpstreamError {
    /// A readable message for agents and logs.
    pub message: String,
    /// The parsed JSON body, when there was one. It is used only to extract
    /// cooldown metadata and is never persisted.
    pub body: Value,
}

/// Pull a readable message out of an upstream error body. Takes the
/// response: reading its body consumes it, and callers only want the
/// status after this.
pub async fn error_detail(resp: reqwest::Response) -> UpstreamError {
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    if bytes.is_empty() {
        return UpstreamError {
            message: format!("upstream {status}"),
            body: Value::Null,
        };
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = serde_json::from_str(&text).unwrap_or(Value::Null);
    let message = body
        .pointer("/error/message")
        .or_else(|| body.pointer("/message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(text);
    UpstreamError {
        message: format!("upstream {status}: {message}"),
        body,
    }
}

/// An error body in the Chat Completions shape.
pub fn chat_error(status: reqwest::StatusCode, message: &str) -> Value {
    let kind = if status == reqwest::StatusCode::BAD_REQUEST {
        "invalid_request_error"
    } else {
        "upstream_error"
    };
    json!({"error": {"message": message, "type": kind, "code": status.as_u16()}})
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
        400 => "invalid_request_error",
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
    let kind = if status == reqwest::StatusCode::BAD_REQUEST {
        "invalid_request_error"
    } else {
        "upstream_error"
    };
    json!({"error": {"message": message, "type": kind, "code": status.as_u16()}})
}
