//! Cline API vendor headers.
//!
//! The Cline API answers its `cline-free/` models `403 "only available
//! via Cline product surfaces"` unless the request carries the desktop
//! app's headers, and its recommended-models feed keys its free list on
//! the desktop client type (the reference's `internal/provider/cline.go`,
//! `ClineVersion = "3.5.54"`). Lite carries no Cline subscription state,
//! feed fetching; this module is the request
//! headers, applied to Chat/Responses/Anthropic/Gemini sends aimed at a
//! Cline host so a user-configured Cline provider is answered like the
//! desktop app's own requests, and the opt-in DeepSeek upstream pin
//! (`cline_pin.go`): a pinned provider's DeepSeek-model Chat request
//! carries `providerOptions.gateway.only: ["deepseek"]` so Cline's AI
//! gateway serves it from DeepSeek's own API.

use regex::{Captures, Regex};
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::Duration;

/// The Cline desktop release lite says it is to the Cline API.
pub const CLINE_VERSION: &str = "3.5.54";

/// The host a base URL is served from, lowercased, without port or
/// trailing dot. Anything unparsable comes back as written, which
/// matches no Cline host.
fn host_of(base_url: &str) -> String {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once(']')
            .map(|(inside, _)| format!("[{inside}]"))
            .unwrap_or_else(|| authority.to_owned())
    } else {
        authority
            .split_once(':')
            .map(|(host, _)| host)
            .unwrap_or(authority)
            .to_owned()
    };
    host.strip_suffix('.').unwrap_or(&host).to_lowercase()
}

/// Whether a base URL is served from the Cline API: `cline.bot` itself
/// or any subdomain of it (the API lives at `api.cline.bot`).
pub fn is_cline_base(base_url: &str) -> bool {
    let host = host_of(base_url);
    host == "cline.bot" || host.ends_with(".cline.bot")
}

/// Carry the Cline desktop app's headers on a request aimed at a Cline
/// host; any other host's request goes through untouched.
pub fn apply_cline_headers(
    mut request: reqwest::RequestBuilder,
    base_url: &str,
) -> reqwest::RequestBuilder {
    if !is_cline_base(base_url) {
        return request;
    }
    request = request
        .header("HTTP-Referer", "https://cline.bot")
        .header("X-Title", "Cline")
        .header("X-IS-MULTIROOT", "false")
        .header("X-CLIENT-TYPE", "cline-desktop")
        .header("X-CLIENT-VERSION", CLINE_VERSION)
        .header("User-Agent", format!("Cline/{CLINE_VERSION}"));
    request
}

/// The start of a model name and its maker's own upstream on Cline's AI
/// gateway (Vercel's AI Gateway, whose provider slugs these are).
const MAKERS: [(&str, &str); 2] = [
    ("deepseek", "deepseek"),
    ("glm", "zai"), // sdytman on Discord: GLM only through Z.ai
];

/// Pin a DeepSeek or GLM model to its maker's own API on the Cline API:
/// a Chat request for one from a pinning provider carries
/// `providerOptions.gateway.only: ["deepseek"]` (GLM: `["zai"]`), which
/// Cline's AI gateway reads to serve the model from that host alone
/// (the reference's `clinePin`, as ClinePass + ProviderPin set it).
/// Anything else — another protocol, an unpinned provider, another
/// host, a free model (served where Cline puts it), another maker's
/// model, or a body already naming `providerOptions` — goes through
/// untouched.
pub fn pin_to_upstream(
    mut body: Value,
    base_url: &str,
    enabled: bool,
    chat: bool,
    model: &str,
) -> Value {
    if !chat || !enabled || !is_cline_base(base_url) {
        return body;
    }
    if model.starts_with("cline-free/") {
        return body;
    }
    let name = model.rsplit('/').next().unwrap_or(model).to_lowercase();
    let Some((_, upstream)) = MAKERS.iter().find(|(prefix, _)| name.starts_with(prefix)) else {
        return body;
    };
    let Some(object) = body.as_object_mut() else {
        return body;
    };
    if object.contains_key("providerOptions") {
        return body;
    }
    object.insert(
        "providerOptions".into(),
        json!({"gateway": {"only": [upstream]}}),
    );
    body
}

/// Take the `{success, data}` envelope off a Cline whole reply: Cline's
/// API answers a request that is not streamed with the completion inside
/// `{"success":true,"data":{chat.completion}}`, which every client of the
/// Chat API reads at the top (the reference's `clineUnwrapped`, 6094S on
/// Discord: Alma's tool-model calls failed with "Invalid JSON response").
/// `{"success":true,"data"}` goes on as the completion, so the codecs,
/// the usage sniffer and the agent see it; `{"success":false}` goes on
/// as `{"error":{"message":…}}`, which the whole-reply error branch
/// turns into the agent's API's refusal. A body without a boolean
/// `success`, a `true` one whose `data` is no object, and any other
/// provider's reply pass as they came. Streams never reach here: the
/// whole-reply branch alone calls this, and a translated request is
/// asked of Cline as a stream, which comes plain.
pub fn unwrap_envelope(base_url: &str, value: Value) -> Value {
    if !is_cline_base(base_url) {
        return value;
    }
    let Some(success) = value.get("success").and_then(Value::as_bool) else {
        return value;
    };
    if success {
        match value.get("data") {
            Some(data) if data.is_object() => data.clone(),
            _ => value,
        }
    } else {
        let message = value
            .get("error")
            .and_then(|e| {
                e.as_str()
                    .map(str::to_owned)
                    .or_else(|| e.get("message").and_then(Value::as_str).map(str::to_owned))
            })
            .or_else(|| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "Cline request failed".to_owned());
        json!({"error": {"message": message}})
    }
}

/// Cline's refusal of a free model whose day's allowance is used up, 429
/// "Daily free limit reached on model deepseek/deepseek-v4-flash. Try
/// again in 23h 59m", as Cline's own clients read it (the reference's
/// `clineFreeLimit`, #1398): the limit is that model's, and the model
/// alone rests.
fn free_limit_words() -> &'static Regex {
    static FREE_LIMIT: OnceLock<Regex> = OnceLock::new();
    FREE_LIMIT
        .get_or_init(|| Regex::new(r"(?i)free limit reached on model").expect("free-limit words"))
}

/// How long Cline's free-model refusal says until the model is back
/// ("Try again in 23h 59m"), in days, hours and minutes (the reference's
/// `tryAgainIn`).
fn try_again_in() -> &'static Regex {
    static TRY_AGAIN: OnceLock<Regex> = OnceLock::new();
    TRY_AGAIN.get_or_init(|| {
        Regex::new(
            r"(?i)\btry again in (?:(\d+)\s*d(?:ays?)?\b\s*)?(?:(\d+)\s*h(?:ours?|rs?)?\b\s*)?(?:(\d+)\s*m(?:in(?:utes?|s)?)?\b)?",
        )
        .expect("try-again words")
    })
}

/// The longest a refusal's named wait is believed: a week, the
/// reference's `longestWritten`.
const LONGEST_WRITTEN_SECS: u64 = 7 * 24 * 60 * 60;

/// The days, hours and minutes a `tryAgainIn` match names, held to a
/// week. Zero when it names none.
fn span_written(caps: Option<Captures<'_>>) -> Duration {
    let Some(caps) = caps else {
        return Duration::ZERO;
    };
    let part = |index: usize| {
        caps.get(index)
            .and_then(|found| found.as_str().parse::<u64>().ok())
            .unwrap_or(0)
    };
    let secs = part(1) * 24 * 60 * 60 + part(2) * 60 * 60 + part(3) * 60;
    Duration::from_secs(secs.min(LONGEST_WRITTEN_SECS))
}

/// How long the model named by Cline's free-model daily-limit refusal is
/// out: `None` for anything that is not one model's own named limit — a
/// non-429, another vendor's same words (only a Cline host's refusal
/// names its model's limit), a Cline 429 that doesn't name the model's
/// limit, or one that names no wait. A named wait past a week is
/// believed for a week.
pub fn free_limit_rest(base_url: &str, status: u16, body: &Value) -> Option<Duration> {
    if status != 429 || !is_cline_base(base_url) {
        return None;
    }
    let text = body.to_string();
    if !free_limit_words().is_match(&text) {
        return None;
    }
    let back_in = span_written(try_again_in().captures(&text));
    (!back_in.is_zero()).then_some(back_in)
}

/// The refusal's own words for the agent: the vendor's message when it
/// names one, else the body as it came.
pub fn refusal_message(body: &Value) -> String {
    body.pointer("/error/message")
        .or_else(|| body.pointer("/message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| body.to_string())
}

/// The model rest a quota refusal lays down: how long the model named by
/// Cline's free-model daily-limit refusal is out, with the refusal's own
/// words for the replay. `None` for anything that rests the key as before
/// — a non-429, another vendor's same words, a Cline 429 that doesn't name
/// the model's limit, or one that names no wait. Both quota branches call
/// this one hook so the host gate can't drift between the streaming and
/// whole-reply paths; a loopback fake can't live on a Cline host, so the
/// gate itself is pinned here by unit tests against the real host names.
pub fn model_rest_for_quota(
    base_url: &str,
    status: u16,
    body: &Value,
) -> Option<(Duration, String)> {
    free_limit_rest(base_url, status, body).map(|back_in| (back_in, refusal_message(body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLINE_BASE: &str = "https://api.cline.bot/api/v1";

    fn completion() -> Value {
        json!({"id":"gen_1","object":"chat.completion","model":"deepseek/deepseek-v4.1-flash",
            "choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}})
    }

    #[test]
    fn whole_replies_lose_the_success_envelope() {
        let out = unwrap_envelope(CLINE_BASE, json!({"success": true, "data": completion()}));
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "pong");
        assert!(out.get("success").is_none(), "{out}");
    }

    #[test]
    fn other_hosts_and_plain_bodies_pass_through() {
        let enveloped = json!({"success": true, "data": completion()});
        assert_eq!(
            unwrap_envelope("https://api.openai.com/v1", enveloped.clone()),
            enveloped
        );
        let plain = completion();
        assert_eq!(unwrap_envelope(CLINE_BASE, plain.clone()), plain);
        let data_less = json!({"success": true});
        assert_eq!(unwrap_envelope(CLINE_BASE, data_less.clone()), data_less);
    }

    #[test]
    fn refusals_become_agent_errors() {
        let out = unwrap_envelope(
            CLINE_BASE,
            json!({"success": false, "error": "Insufficient balance"}),
        );
        assert_eq!(out, json!({"error": {"message": "Insufficient balance"}}));
        let nested = unwrap_envelope(
            CLINE_BASE,
            json!({"success": false, "error": {"message": "quota spent"}}),
        );
        assert_eq!(nested, json!({"error": {"message": "quota spent"}}));
        let top = unwrap_envelope(CLINE_BASE, json!({"success": false, "message": "nope"}));
        assert_eq!(top, json!({"error": {"message": "nope"}}));
        let bare = unwrap_envelope(CLINE_BASE, json!({"success": false}));
        assert_eq!(bare, json!({"error": {"message": "Cline request failed"}}));
    }

    #[test]
    fn cline_hosts_match_through_case_port_path_and_subdomains() {
        for url in [
            "https://api.cline.bot/api/v1",
            "HTTPS://API.CLINE.BOT:443/api/v1",
            "https://api.cline.bot./api/v1",
            "http://api.cline.bot/api/v1/chat/completions?x=1",
            "https://cline.bot/api/v1",
            "https://shop.example.cline.bot/api/v1",
        ] {
            assert!(is_cline_base(url), "{url}");
        }
    }

    #[test]
    fn lookalike_suffixes_and_other_hosts_do_not_match() {
        for url in [
            "https://relay.example.com/v1",
            "http://localhost:11434/v1",
            "https://evillcline.bot/",
            "https://cline.bot.evil.example/",
            "https://api.cline.bot.evil.example/api/v1",
            "https://api.openai.com/v1",
            "not a url at all",
            "",
        ] {
            assert!(!is_cline_base(url), "{url}");
        }
    }

    #[test]
    fn cline_headers_carry_the_desktop_version() {
        let request = apply_cline_headers(
            reqwest::Client::new().post("https://api.cline.bot/api/v1/chat/completions"),
            "https://api.cline.bot/api/v1",
        )
        .build()
        .expect("request builds");
        let headers = request.headers();
        assert_eq!(
            headers.get("HTTP-Referer").expect("referer"),
            "https://cline.bot"
        );
        assert_eq!(headers.get("X-Title").expect("title"), "Cline");
        assert_eq!(headers.get("X-IS-MULTIROOT").expect("multiroot"), "false");
        assert_eq!(
            headers.get("X-CLIENT-TYPE").expect("client type"),
            "cline-desktop"
        );
        assert_eq!(
            headers.get("X-CLIENT-VERSION").expect("client version"),
            CLINE_VERSION
        );
        assert_eq!(
            headers.get("User-Agent").expect("user agent"),
            &format!("Cline/{CLINE_VERSION}")
        );
    }

    #[test]
    fn other_hosts_keep_their_headers_untouched() {
        let request = apply_cline_headers(
            reqwest::Client::new()
                .post("https://api.openai.com/v1/chat/completions")
                .header("User-Agent", "kept"),
            "https://api.openai.com/v1",
        )
        .build()
        .expect("request builds");
        let headers = request.headers();
        assert!(headers.get("X-CLIENT-TYPE").is_none());
        assert!(headers.get("X-CLIENT-VERSION").is_none());
        assert_eq!(headers.get("User-Agent").expect("user agent"), "kept");
    }

    fn chat_body(model: &str) -> Value {
        json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
    }

    fn pinned_to(body: &Value, upstream: &str) -> bool {
        body.get("providerOptions") == Some(&json!({"gateway": {"only": [upstream]}}))
    }

    fn pinned(body: &Value) -> bool {
        pinned_to(body, "deepseek")
    }

    #[test]
    fn deepseek_models_pin_to_deepseek_on_a_pinning_cline_provider() {
        for model in [
            "deepseek-v4-pro",
            "cline-pass/deepseek-v4.1-flash",
            "DEEPSEEK-V4-PRO",
        ] {
            let out = pin_to_upstream(
                chat_body(model),
                "https://api.cline.bot/api/v1",
                true,
                true,
                model,
            );
            assert!(pinned(&out), "{model}");
        }
    }

    #[test]
    fn glm_models_pin_to_zai_on_a_pinning_cline_provider() {
        for model in ["glm-5.3", "cline-pass/glm-5.3", "GLM-5.3"] {
            let out = pin_to_upstream(
                chat_body(model),
                "https://api.cline.bot/api/v1",
                true,
                true,
                model,
            );
            assert!(pinned_to(&out, "zai"), "{model}");
        }
    }

    #[test]
    fn pinning_leaves_everything_else_untouched() {
        // Free models are served where Cline puts them; other makers,
        // hosts, protocols and unpinned providers never pin.
        let cases = [
            (
                "cline-free/deepseek-v4.1-flash",
                "https://api.cline.bot/api/v1",
                true,
                true,
            ),
            (
                "cline-free/glm-5.3",
                "https://api.cline.bot/api/v1",
                true,
                true,
            ),
            (
                "cline-pass/qwen-max",
                "https://api.cline.bot/api/v1",
                true,
                true,
            ),
            ("deepseek-v4-pro", "https://api.openai.com/v1", true, true),
            (
                "deepseek-v4-pro",
                "https://api.cline.bot/api/v1",
                false,
                true,
            ),
            (
                "deepseek-v4-pro",
                "https://api.cline.bot/api/v1",
                true,
                false,
            ),
        ];
        for (model, base, enabled, chat) in cases {
            let out = pin_to_upstream(chat_body(model), base, enabled, chat, model);
            assert!(
                !pinned(&out) && !pinned_to(&out, "zai"),
                "{model} {base} {enabled} {chat}"
            );
        }
    }

    #[test]
    fn a_client_named_provider_options_wins_over_the_pin() {
        let mut body = chat_body("deepseek-v4-pro");
        body.as_object_mut().expect("object").insert(
            "providerOptions".into(),
            json!({"gateway": {"only": ["other"]}}),
        );
        let out = pin_to_upstream(
            body,
            "https://api.cline.bot/api/v1",
            true,
            true,
            "deepseek-v4-pro",
        );
        assert_eq!(
            out["providerOptions"],
            json!({"gateway": {"only": ["other"]}}),
            "client's own options stand"
        );
    }

    const FREE_LIMIT: &str =
        "Daily free limit reached on model deepseek/deepseek-v4-flash. Try again in 23h 59m";

    fn free_body(message: &str) -> Value {
        json!({"error": {"message": message}})
    }

    #[test]
    fn free_limit_rest_names_its_wait_on_a_cline_host() {
        let back_in = free_limit_rest(CLINE_BASE, 429, &free_body(FREE_LIMIT))
            .expect("one model's own limit");
        assert_eq!(back_in, Duration::from_secs(23 * 60 * 60 + 59 * 60));
    }

    #[test]
    fn free_limit_rest_reads_days_hours_and_minutes() {
        for (message, secs) in [
            (
                "free limit reached on model m. Try again in 1d",
                24 * 60 * 60,
            ),
            (
                "free limit reached on model m. Try again in 2h 5m",
                2 * 60 * 60 + 5 * 60,
            ),
            (
                "free limit reached on model m. Try again in 1d 2h 3m",
                24 * 60 * 60 + 2 * 60 * 60 + 3 * 60,
            ),
            // A named wait past a week is believed for a week.
            (
                "free limit reached on model m. Try again in 30d",
                7 * 24 * 60 * 60,
            ),
        ] {
            assert_eq!(
                free_limit_rest(CLINE_BASE, 429, &free_body(message)),
                Some(Duration::from_secs(secs)),
                "{message}"
            );
        }
    }

    #[test]
    fn free_limit_rest_needs_the_marker_and_a_wait() {
        // A Cline 429 that doesn't name the model's limit rests the key.
        assert_eq!(
            free_limit_rest(CLINE_BASE, 429, &free_body("Rate limit exceeded")),
            None
        );
        // The marker without a named wait names nothing to believe.
        assert_eq!(
            free_limit_rest(CLINE_BASE, 429, &free_body("free limit reached on model m")),
            None
        );
        // The model's limit on another status is not its daily cap.
        assert_eq!(
            free_limit_rest(CLINE_BASE, 500, &free_body(FREE_LIMIT)),
            None
        );
    }

    #[test]
    fn free_limit_rest_ignores_other_vendors() {
        // Another vendor's same words rest its key as before.
        assert_eq!(
            free_limit_rest("https://api.openai.com/v1", 429, &free_body(FREE_LIMIT)),
            None
        );
    }

    #[test]
    fn refusal_message_prefers_the_vendors_words() {
        assert_eq!(refusal_message(&free_body(FREE_LIMIT)), FREE_LIMIT);
        assert_eq!(
            refusal_message(&json!({"message": "plain refusal"})),
            "plain refusal"
        );
        let body = json!({"success": false});
        assert_eq!(refusal_message(&body), body.to_string());
    }

    #[test]
    fn quota_hook_rests_the_named_model_with_its_own_words() {
        let (back_in, message) = model_rest_for_quota(CLINE_BASE, 429, &free_body(FREE_LIMIT))
            .expect("one hook for both quota branches");
        assert_eq!(back_in, Duration::from_secs(23 * 60 * 60 + 59 * 60));
        assert_eq!(message, FREE_LIMIT);
    }

    #[test]
    fn quota_hook_leaves_the_key_path_what_it_does_not_name() {
        // A Cline 429 that names no wait lays down no rest.
        assert_eq!(
            model_rest_for_quota(CLINE_BASE, 429, &free_body("free limit reached on model m")),
            None
        );
        // The model's limit on another status is not its daily cap.
        assert_eq!(
            model_rest_for_quota(CLINE_BASE, 500, &free_body(FREE_LIMIT)),
            None
        );
        // Another vendor's same words rest its key as before.
        assert_eq!(
            model_rest_for_quota("https://api.openai.com/v1", 429, &free_body(FREE_LIMIT)),
            None
        );
        // A plain Cline rate limit names no model.
        assert_eq!(
            model_rest_for_quota(CLINE_BASE, 429, &free_body("Rate limit exceeded")),
            None
        );
    }
}
