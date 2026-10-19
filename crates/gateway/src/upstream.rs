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
pub fn build_request(
    p: &ProviderConfig,
    req: &CompletionRequest,
    model: &str,
) -> magpie_core::Result<Value> {
    let mut body = match p.protocol {
        Protocol::Chat => chat::encode_request(req, model),
        Protocol::Anthropic => anthropic::encode_request(req, model),
        Protocol::Responses => responses::encode_request(req, model),
        Protocol::Gemini => gemini::encode_request(req, model),
    }?;
    let host = reqwest::Url::parse(&p.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    if p.protocol == Protocol::Chat
        && host.as_deref() == Some("openrouter.ai")
        && let Some(options) = body.get("web_search_options")
    {
        if options.as_object().is_none_or(|m| !m.is_empty()) {
            return Err(magpie_core::CoreError::Invalid { field: "web_search",
                detail: "configured Chat search options cannot be translated to the OpenRouter web plugin".into() });
        }
        body.as_object_mut()
            .expect("request body object")
            .remove("web_search_options");
        body["plugins"] = json!([{"id":"web"}]);
    }
    // DeepSeek's chat API has no max_completion_tokens and ignores one,
    // so a client that asks OpenAI's way gets DeepSeek's 8K default. A
    // request that names max_tokens too goes as it is, since DeepSeek
    // reads that one. OpenAI's and Azure's APIs want the OpenAI field.
    if p.protocol == Protocol::Chat && host.as_deref() == Some("api.deepseek.com") {
        body = deepseek_reply_limit(body);
    }
    // xAI turns away a tool whose `pattern` Rust's regex syntax can't
    // parse, where ECMAScript reads the same pattern fine. Every protocol
    // carries tool schemas, so this runs wherever the tools are.
    body = crate::grok_schema::grok_tool_patterns(body, model, host.as_deref());
    if p.protocol == Protocol::Responses
        && host.as_deref() == Some("api.x.ai")
        && let Some(include) = body.get_mut("include").and_then(Value::as_array_mut)
    {
        include.retain(|v| v != "web_search_call.action.sources");
        if include.is_empty() {
            body.as_object_mut()
                .expect("request body object")
                .remove("include");
        }
    }
    // Gemini names the model in the request URL, not in the JSON body.
    if p.protocol == Protocol::Gemini
        && let Some(object) = body.as_object_mut()
    {
        object.remove("model");
    }
    Ok(body)
}

/// DeepSeek's chat API has no `max_completion_tokens` and ignores one, so
/// a client that asks OpenAI's way gets DeepSeek's 8K default. Rename it
/// in place; a request that names `max_tokens` too goes as it is, since
/// DeepSeek reads that one. OpenAI's and Azure's APIs want OpenAI's field,
/// so no other host is touched.
fn deepseek_reply_limit(mut body: Value) -> Value {
    if body.get("max_tokens").is_some() {
        return body;
    }
    let Some(max) = body.get("max_completion_tokens").cloned() else {
        return body;
    };
    if let Some(object) = body.as_object_mut() {
        object.remove("max_completion_tokens");
        object.insert("max_tokens".into(), max);
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

#[cfg(test)]
mod search_tests {
    use super::*;

    fn provider(protocol: Protocol, url: &str) -> ProviderConfig {
        serde_json::from_value(json!({"id":"fixture", "protocol":protocol, "base_url":url}))
            .unwrap()
    }
    fn search_request() -> CompletionRequest {
        responses::decode_request(
            &json!({"model":"client", "input":"hello", "tools":[{"type":"web_search"}]}),
        )
        .unwrap()
    }

    #[test]
    fn openrouter_uses_web_plugin_only_on_its_exact_host() {
        for url in [
            "https://openrouter.ai/api/v1",
            "https://OPENROUTER.AI:443/api/v1",
        ] {
            let out =
                build_request(&provider(Protocol::Chat, url), &search_request(), "up").unwrap();
            assert_eq!(out["plugins"], json!([{"id":"web"}]));
            assert!(out.get("web_search_options").is_none());
        }
        for url in [
            "https://openrouter.ai.evil.invalid/v1",
            "https://example.invalid/openrouter.ai/v1",
            "https://api.openai.com/v1",
        ] {
            let out =
                build_request(&provider(Protocol::Chat, url), &search_request(), "up").unwrap();
            assert!(out.get("plugins").is_none());
            assert_eq!(out["web_search_options"], json!({}));
        }
    }

    #[test]
    fn openrouter_does_not_silently_drop_native_chat_search_options() {
        let request = chat::decode_request(&json!({"model":"client", "messages":[],
            "web_search_options":{"search_context_size":"low"}}))
        .unwrap();
        assert!(
            build_request(
                &provider(Protocol::Chat, "https://openrouter.ai/api/v1"),
                &request,
                "up"
            )
            .is_err()
        );
        let out = build_request(
            &provider(Protocol::Chat, "https://api.openai.com/v1"),
            &request,
            "up",
        )
        .unwrap();
        assert_eq!(out["web_search_options"]["search_context_size"], "low");
    }

    #[test]
    fn xai_does_not_receive_openai_search_sources_include() {
        for (url, has_include) in [
            ("https://api.x.ai/v1", false),
            ("https://api.x.ai.evil.invalid/v1", true),
            ("https://api.openai.com/v1", true),
        ] {
            let out = build_request(&provider(Protocol::Responses, url), &search_request(), "up")
                .unwrap();
            assert_eq!(out["tools"][0]["type"], "web_search");
            assert_eq!(out.get("include").is_some(), has_include);
        }
    }

    #[test]
    fn deepseek_reads_openais_reply_limit_as_max_tokens() {
        // A body naming only OpenAI's field: DeepSeek would ignore it and
        // fall back to its 8K default.
        let renamed = deepseek_reply_limit(json!({"model":"m", "max_completion_tokens":700}));
        assert_eq!(renamed["max_tokens"], 700);
        assert!(renamed.get("max_completion_tokens").is_none());
        // A body that already names max_tokens keeps it, and drops nothing
        // else it was sent.
        let both = deepseek_reply_limit(json!({"model":"m", "max_tokens":900,
            "max_completion_tokens":700, "temperature":0.2}));
        assert_eq!(both["max_tokens"], 900);
        assert_eq!(both["max_completion_tokens"], 700);
        assert_eq!(both["temperature"], 0.2);
        // Nothing to rename comes back as it was.
        let plain = deepseek_reply_limit(json!({"model":"m", "messages":[]}));
        assert_eq!(plain, json!({"model":"m", "messages":[]}));
        // The rule is wired to DeepSeek's Chat API alone: OpenAI's, a
        // lookalike host and a non-Chat protocol keep OpenAI's field.
        let request = chat::decode_request(&json!({"model":"m",
            "messages":[{"role":"user","content":"hi"}], "max_tokens":700}))
        .unwrap();
        for host in [
            "https://api.openai.com/v1",
            "https://api.deepseek.com.evil.invalid/v1",
        ] {
            let out = build_request(&provider(Protocol::Chat, host), &request, "up").unwrap();
            assert_eq!(out["max_tokens"], 700);
        }
    }
    #[test]
    fn disabled_search_never_enables_openrouter_plugin_or_search_include() {
        let request = responses::decode_request(
            &json!({"model":"m", "tools":[{"type":"web_search"}], "tool_choice":"none"}),
        )
        .unwrap();
        for (protocol, url) in [
            (Protocol::Chat, "https://openrouter.ai/api/v1"),
            (Protocol::Responses, "https://api.openai.com/v1"),
        ] {
            let out = build_request(&provider(protocol, url), &request, "up").unwrap();
            for key in ["plugins", "tools", "web_search_options", "include"] {
                assert!(out.get(key).is_none());
            }
        }
    }

    #[test]
    fn a_grok_tool_pattern_is_respelled_before_it_goes_upstream() {
        let request = anthropic::decode_request(&json!({"model":"client", "max_tokens":16,
            "messages":[{"role":"user","content":"hi"}],
            "tools":[{"name":"t","input_schema":{"type":"object","properties":{
                "x":{"type":"string","pattern":"^[^\\s[\\]]+$"}}}}]}))
        .unwrap();
        // A relay that serves Grok under its own model name.
        let out = build_request(
            &provider(Protocol::Anthropic, "https://relay.example/v1"),
            &request,
            "grok-4.6",
        )
        .unwrap();
        assert_eq!(
            out["tools"][0]["input_schema"]["properties"]["x"]["pattern"],
            r"^[^\s\[\]]+$"
        );
        // xAI's own API counts by its host, whatever the model is called.
        let xai = build_request(
            &provider(Protocol::Anthropic, "https://api.x.ai/v1"),
            &request,
            "my-alias",
        )
        .unwrap();
        assert_eq!(
            xai["tools"][0]["input_schema"]["properties"]["x"]["pattern"],
            r"^[^\s\[\]]+$"
        );
        // Another model's tools go as they came, on a relay or on a
        // lookalike host.
        for (url, model) in [
            ("https://relay.example/v1", "claude-sonnet-5"),
            ("https://api.x.ai.evil.invalid/v1", "claude-sonnet-5"),
        ] {
            let other =
                build_request(&provider(Protocol::Anthropic, url), &request, model).unwrap();
            assert_eq!(
                other["tools"][0]["input_schema"]["properties"]["x"]["pattern"],
                r"^[^\s[\]]+$"
            );
        }
    }
}
