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

use serde_json::{Value, json};

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

/// Pin a DeepSeek model to DeepSeek's own API on the Cline API: a Chat
/// request for one from a pinning provider carries
/// `providerOptions.gateway.only: ["deepseek"]`, which Cline's AI
/// gateway reads to serve the model from that host alone (the
/// reference's `clinePin`, as ClinePass + ProviderPin set it). Anything
/// else — another protocol, an unpinned provider, another host, a
/// free model (served where Cline puts it), a non-DeepSeek model, or a
/// body already naming `providerOptions` — goes through untouched.
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
    let name = model.rsplit('/').next().unwrap_or(model);
    if !name.to_lowercase().starts_with("deepseek") {
        return body;
    }
    let Some(object) = body.as_object_mut() else {
        return body;
    };
    if object.contains_key("providerOptions") {
        return body;
    }
    object.insert(
        "providerOptions".into(),
        json!({"gateway": {"only": ["deepseek"]}}),
    );
    body
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn pinned(body: &Value) -> bool {
        body.get("providerOptions") == Some(&json!({"gateway": {"only": ["deepseek"]}}))
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
    fn pinning_leaves_everything_else_untouched() {
        // Free models are served where Cline puts them; other models,
        // hosts, protocols and unpinned providers never pin.
        let cases = [
            (
                "cline-free/deepseek-v4.1-flash",
                "https://api.cline.bot/api/v1",
                true,
                true,
            ),
            (
                "cline-pass/glm-5.3",
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
            assert!(!pinned(&out), "{model} {base} {enabled} {chat}");
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
}
