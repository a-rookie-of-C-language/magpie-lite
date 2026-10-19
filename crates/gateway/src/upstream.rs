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
    // DeepSeek's validator takes a tool pattern's null escape as `\u0000`,
    // not ECMAScript's `\0` (#370).
    body = crate::deepseek_schema::deepseek_tool_patterns(body, host.as_deref());
    // Moonshot refuses an untyped enum in a tool schema (#886), and since
    // kimi-k2.5 locks temperature and top_p.
    body = crate::kimi_schema::kimi_tool_enum_types(body, host.as_deref());
    body = crate::kimi_schema::kimi_locked_sampling(body, host.as_deref(), model);
    // OpenRouter's grammar compiler refuses a oneOf whose branches are not
    // provably disjoint; anyOf holds the same branches.
    body = crate::oneof_schema::one_of_as_any_of(body, host.as_deref());
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

/// The most of an upstream error body worth reading. A vendor that answers
/// an error with a whole page (or a reply) must not hold the client while
/// it is read; past this the status alone is the message.
const MAX_ERROR_BODY: usize = 1 << 20;

/// Pull a readable message out of an upstream error body. Takes the
/// response: reading its body consumes it, and callers only want the
/// status after this. At most `MAX_ERROR_BODY` bytes are read; a longer
/// body is dropped, as its head is not worth holding a client for.
pub async fn error_detail(mut resp: reqwest::Response) -> UpstreamError {
    let status = resp.status();
    let mut bytes = Vec::new();
    if resp
        .content_length()
        .is_some_and(|length| length > MAX_ERROR_BODY as u64)
    {
        return UpstreamError {
            message: format!("upstream {status}"),
            body: Value::Null,
        };
    }
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => {
                return UpstreamError {
                    message: format!("upstream {status}"),
                    body: Value::Null,
                };
            }
        };
        if bytes.len() + chunk.len() > MAX_ERROR_BODY {
            return UpstreamError {
                message: format!("upstream {status}"),
                body: Value::Null,
            };
        }
        bytes.extend_from_slice(&chunk);
    }
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
    chat_error_kind(status, message, kind)
}

/// A Chat-shaped error with the type the client sees, not the status's
/// default. The reference's `writeErrorSaid` names the type for each
/// status (a 413 is `request_too_large`, an overflow a 400
/// `context_length_exceeded`) and says it in every client shape.
pub fn chat_error_kind(status: reqwest::StatusCode, message: &str, kind: &str) -> Value {
    let code = if magpie_core::stream_error::overflow_words(message) {
        json!("context_length_exceeded")
    } else {
        json!(status.as_u16())
    };
    json!({"error": {"message": message, "type": kind, "code": code}})
}

/// An error body in the Gemini generateContent shape.
pub fn gemini_error(status: reqwest::StatusCode, message: &str) -> Value {
    gemini_error_reason(status, message, gemini_reason(status.as_u16()))
}

pub fn gemini_reason(status: u16) -> &'static str {
    match status {
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        408 => "DEADLINE_EXCEEDED",
        413 => "INVALID_ARGUMENT",
        429 => "RESOURCE_EXHAUSTED",
        500 => "INTERNAL",
        502 | 503 | 529 => "UNAVAILABLE",
        _ => "UNKNOWN",
    }
}

/// A Gemini-shaped error with the reason the client sees.
pub fn gemini_error_reason(
    status: reqwest::StatusCode,
    message: &str,
    reason: &'static str,
) -> Value {
    json!({"error": {"code": status.as_u16(), "message": message, "status": reason}})
}

/// An error body in the Anthropic Messages shape.
pub fn anthropic_error(status: reqwest::StatusCode, message: &str) -> Value {
    let kind = match status.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    };
    anthropic_error_kind(message, kind)
}

/// An Anthropic-shaped error with the type the client sees.
pub fn anthropic_error_kind(message: &str, kind: &str) -> Value {
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
    responses_error_kind(status, message, kind)
}

/// A Responses-shaped error with the type the client sees.
pub fn responses_error_kind(status: reqwest::StatusCode, message: &str, kind: &str) -> Value {
    chat_error_kind(status, message, kind)
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

    /// Serve one error body on a loopback port, and hand back the
    /// response as the gateway would read it.
    async fn error_response(body: Vec<u8>, length: Option<usize>) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let length = match length {
                Some(length) => format!("Content-Length: {length}\r\n"),
                None => String::new(),
            };
            let head = format!("HTTP/1.1 400 Bad Request\r\n{length}Connection: close\r\n\r\n");
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        reqwest::Client::new()
            .post(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_error_body_past_the_limit_is_reported_by_status_alone() {
        // A Content-Length that already says it is too long is not read.
        let resp = error_response(vec![b'x'; 8], Some(MAX_ERROR_BODY + 1)).await;
        let error = error_detail(resp).await;
        assert_eq!(error.message, "upstream 400 Bad Request");
        assert_eq!(error.body, Value::Null);
        // A body that only outgrows the limit while it is read is dropped
        // too, and the stream ends without a hang.
        let resp = error_response(vec![b'x'; MAX_ERROR_BODY + 1], None).await;
        let error = error_detail(resp).await;
        assert_eq!(error.message, "upstream 400 Bad Request");
        assert_eq!(error.body, Value::Null);
    }

    #[tokio::test]
    async fn a_normal_error_body_keeps_its_readable_message() {
        let body = br#"{"error":{"message":"model is gone"}}"#.to_vec();
        let resp = error_response(body.clone(), Some(body.len())).await;
        let error = error_detail(resp).await;
        assert_eq!(error.message, "upstream 400 Bad Request: model is gone");
        assert_eq!(error.body["error"]["message"], "model is gone");
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

    #[test]
    fn a_deepseek_tool_pattern_gets_its_null_escape_respelled() {
        let request = chat::decode_request(&json!({"model":"client",
            "messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{"name":"Artifact","parameters":{
                "type":"object","properties":{
                    "content":{"type":"string","pattern":"^[^\\0]*$"}}}}}]}))
        .unwrap();
        let out = build_request(
            &provider(Protocol::Chat, "https://api.deepseek.com/v1"),
            &request,
            "up",
        )
        .unwrap();
        assert_eq!(
            out["tools"][0]["function"]["parameters"]["properties"]["content"]["pattern"],
            r"^[^\u0000]*$"
        );
        // Every other host keeps ECMAScript's own spelling.
        for url in [
            "https://api.openai.com/v1",
            "https://api.deepseek.com.evil.invalid/v1",
        ] {
            let other = build_request(&provider(Protocol::Chat, url), &request, "up").unwrap();
            assert_eq!(
                other["tools"][0]["function"]["parameters"]["properties"]["content"]["pattern"],
                r"^[^\0]*$"
            );
        }
    }

    #[test]
    fn a_kimi_tool_enum_gets_its_type_and_a_locked_model_loses_its_sampling() {
        let request = chat::decode_request(&json!({"model":"client",
            "messages":[{"role":"user","content":"hi"}], "temperature":0.1, "top_p":1,
            "tools":[{"type":"function","function":{"name":"t","parameters":{
                "type":"object","properties":{"kind":{"enum":["text","icon"]}}}}}]}))
        .unwrap();
        let out = build_request(
            &provider(Protocol::Chat, "https://api.moonshot.ai/v1"),
            &request,
            "kimi-k2.5",
        )
        .unwrap();
        assert_eq!(
            out["tools"][0]["function"]["parameters"]["properties"]["kind"]["type"],
            "string"
        );
        assert!(out.get("temperature").is_none());
        assert!(out.get("top_p").is_none());
        // A range model keeps the sampling it was given.
        let ranged = build_request(
            &provider(Protocol::Chat, "https://api.moonshot.ai/v1"),
            &request,
            "kimi-k2-0905-preview",
        )
        .unwrap();
        assert_eq!(ranged["temperature"], 0.1);
        assert_eq!(ranged["top_p"], 1.0);
        // Another host keeps both the untyped enum and the sampling.
        let other = build_request(
            &provider(Protocol::Chat, "https://api.openai.com/v1"),
            &request,
            "kimi-k2.5",
        )
        .unwrap();
        assert!(
            other["tools"][0]["function"]["parameters"]["properties"]["kind"]
                .get("type")
                .is_none()
        );
        assert_eq!(other["temperature"], 0.1);
    }

    #[test]
    fn an_openrouter_tool_one_of_reaches_it_as_any_of() {
        let request = chat::decode_request(&json!({"model":"client",
            "messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{"name":"t","parameters":{
                "type":"object","properties":{"x":{"oneOf":[
                    {"type":"string"},{"type":"integer"}]}}}}}]}))
        .unwrap();
        let out = build_request(
            &provider(Protocol::Chat, "https://openrouter.ai/api/v1"),
            &request,
            "up",
        )
        .unwrap();
        let x = &out["tools"][0]["function"]["parameters"]["properties"]["x"];
        assert!(x.get("oneOf").is_none());
        assert_eq!(x["anyOf"], json!([{"type":"string"},{"type":"integer"}]));
        // Every other host keeps oneOf.
        let other = build_request(
            &provider(Protocol::Chat, "https://api.openai.com/v1"),
            &request,
            "up",
        )
        .unwrap();
        assert_eq!(
            other["tools"][0]["function"]["parameters"]["properties"]["x"]["oneOf"],
            json!([{"type":"string"},{"type":"integer"}])
        );
    }
}
