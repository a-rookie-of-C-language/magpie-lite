//! Capability presets for well-known API vendors.
//!
//! A provider added with a known vendor's base URL gets that vendor's
//! capability metadata prefilled, so the console, CLI and `/config`
//! show what the vendor takes without the user spelling it out. An
//! explicitly supplied capabilities object always wins, an existing
//! provider's customized metadata is never overwritten, and a preset
//! never fails validation: when the preset's protocol list does not
//! hold the configured protocol, the list stays empty (only the
//! configured protocol is known) while the vendor's streaming, tools
//! and vision answers still apply. Capability metadata is management/UI
//! information and proves no live vendor probe, except an explicit
//! `vision: false`: a text-only member is never sent image bytes (a
//! current user image is refused, older and tool-returned images go on
//! as an omission note).

use crate::config::{Protocol, ProviderCapabilities};

/// A known vendor: the host its base URL is served from, the wire
/// protocols it takes, and whether it streams, takes tools and takes
/// image input. `None` means unknown, not unsupported.
struct Preset {
    host: &'static str,
    protocols: &'static [Protocol],
    streaming: Option<bool>,
    tools: Option<bool>,
    vision: Option<bool>,
}

// Only vendors whose answers are certain belong here. A relay or a
// niche vendor keeps unknown metadata until the user or a probe says
// otherwise.
const PRESETS: &[Preset] = &[
    Preset {
        host: "api.openai.com",
        protocols: &[Protocol::Chat, Protocol::Responses],
        streaming: Some(true),
        tools: Some(true),
        vision: Some(true),
    },
    Preset {
        host: "api.anthropic.com",
        protocols: &[Protocol::Anthropic],
        streaming: Some(true),
        tools: Some(true),
        vision: Some(true),
    },
    Preset {
        host: "api.deepseek.com",
        protocols: &[Protocol::Chat],
        streaming: Some(true),
        tools: Some(true),
        vision: Some(false),
    },
    Preset {
        host: "api.moonshot.ai",
        protocols: &[Protocol::Chat],
        streaming: Some(true),
        tools: Some(true),
        vision: None,
    },
    Preset {
        host: "api.moonshot.cn",
        protocols: &[Protocol::Chat],
        streaming: Some(true),
        tools: Some(true),
        vision: None,
    },
    Preset {
        host: "generativelanguage.googleapis.com",
        protocols: &[Protocol::Gemini],
        streaming: Some(true),
        tools: Some(true),
        vision: Some(true),
    },
    Preset {
        host: "api.x.ai",
        protocols: &[Protocol::Chat],
        streaming: Some(true),
        tools: Some(true),
        vision: None,
    },
    Preset {
        host: "openrouter.ai",
        protocols: &[Protocol::Chat],
        streaming: Some(true),
        tools: Some(true),
        vision: None,
    },
    Preset {
        host: "api.groq.com",
        protocols: &[Protocol::Chat],
        streaming: Some(true),
        tools: Some(true),
        vision: None,
    },
    Preset {
        host: "api.siliconflow.cn",
        protocols: &[Protocol::Chat],
        streaming: Some(true),
        tools: Some(true),
        vision: None,
    },
    // The Cline API speaks OpenAI Chat Completions (the reference's
    // `clinepass` preset, `Chat: "https://api.cline.bot/api/v1"`) and
    // answers streamed asks (its probe asks streamed). Tools and image
    // input are unconfirmed, so they stay unknown rather than guessed.
    Preset {
        host: "api.cline.bot",
        protocols: &[Protocol::Chat],
        streaming: Some(true),
        tools: None,
        vision: None,
    },
];

/// The host a base URL is served from, lowercased, without port or
/// trailing dot: `HTTPS://API.OPENAI.COM:443/v1` becomes
/// `api.openai.com`. Anything unparsable comes back as written, which
/// matches no preset.
fn host_of(base_url: &str) -> String {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // An IPv6 literal, `[::1]` or `[::1]:8080`: never a preset.
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

fn preset_for_host(host: &str) -> Option<&'static Preset> {
    if let Some(found) = PRESETS.iter().find(|preset| preset.host == host) {
        return Some(found);
    }
    // Azure OpenAI serves every customer from its own subdomain.
    const AZURE: Preset = Preset {
        host: ".openai.azure.com",
        protocols: &[Protocol::Chat, Protocol::Responses],
        streaming: Some(true),
        tools: Some(true),
        vision: Some(true),
    };
    if host.ends_with(".openai.azure.com") {
        return Some(&AZURE);
    }
    None
}

/// The capability metadata a known vendor's base URL implies for a
/// provider speaking `protocol`, or `None` when the host is unknown.
/// The protocol list holds the vendor's list only when it holds the
/// configured protocol; otherwise it stays empty, which validation
/// accepts.
pub fn preset_capabilities(base_url: &str, protocol: Protocol) -> Option<ProviderCapabilities> {
    let preset = preset_for_host(&host_of(base_url))?;
    let supported_protocols = if preset.protocols.contains(&protocol) {
        preset.protocols.to_vec()
    } else {
        Vec::new()
    };
    Some(ProviderCapabilities {
        supported_protocols,
        streaming: preset.streaming,
        tools: preset.tools,
        vision: preset.vision,
    })
}

/// The capabilities a provider add should store: an existing
/// provider's customized metadata stays untouched, a fresh or
/// never-customized provider gets its vendor's preset when known,
/// otherwise the all-unknown default.
pub fn capabilities_for_add(
    existing: Option<&ProviderCapabilities>,
    base_url: &str,
    protocol: Protocol,
) -> ProviderCapabilities {
    if let Some(known) = existing.filter(|c| **c != ProviderCapabilities::default()) {
        return (*known).clone();
    }
    preset_capabilities(base_url, protocol).unwrap_or_default()
}

/// One-line management display for stored capability metadata, or `None`
/// when nothing is known beyond the configured protocol.
pub fn describe_capabilities(
    capabilities: &ProviderCapabilities,
    current: Protocol,
) -> Option<String> {
    if *capabilities == ProviderCapabilities::default() {
        return None;
    }
    fn said(value: Option<bool>) -> &'static str {
        match value {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        }
    }
    let protocols = if capabilities.supported_protocols.is_empty() {
        current.as_str().to_owned()
    } else {
        capabilities
            .supported_protocols
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join("+")
    };
    Some(format!(
        "capabilities {protocols}; stream {}; tools {}; vision {}",
        said(capabilities.streaming),
        said(capabilities.tools),
        said(capabilities.vision),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hosts_prefill_their_protocols_and_answers() {
        let openai =
            preset_capabilities("https://api.openai.com/v1", Protocol::Chat).expect("preset");
        assert_eq!(
            openai.supported_protocols,
            vec![Protocol::Chat, Protocol::Responses]
        );
        assert_eq!(openai.streaming, Some(true));
        assert_eq!(openai.tools, Some(true));
        assert_eq!(openai.vision, Some(true));
        let anthropic =
            preset_capabilities("https://api.anthropic.com", Protocol::Anthropic).expect("preset");
        assert_eq!(anthropic.supported_protocols, vec![Protocol::Anthropic]);
        let deepseek =
            preset_capabilities("https://api.deepseek.com/v1", Protocol::Chat).expect("preset");
        assert_eq!(deepseek.vision, Some(false));
    }

    #[test]
    fn cline_prefills_chat_and_streaming_only() {
        // The Cline API speaks Chat and answers streamed asks; tools and
        // image input are unconfirmed, so they stay unknown.
        let cline =
            preset_capabilities("https://api.cline.bot/api/v1", Protocol::Chat).expect("preset");
        assert_eq!(cline.supported_protocols, vec![Protocol::Chat]);
        assert_eq!(cline.streaming, Some(true));
        assert_eq!(cline.tools, None);
        assert_eq!(cline.vision, None);
    }

    #[test]
    fn hosts_match_through_scheme_case_port_path_and_subdomains() {
        for url in [
            "HTTPS://API.OPENAI.COM:443/v1",
            "https://api.openai.com./v1",
            "http://api.openai.com/v1/chat/completions?x=1",
            "https://api.openai.com",
        ] {
            let preset = preset_capabilities(url, Protocol::Chat).expect("preset");
            assert_eq!(preset.streaming, Some(true), "{url}");
        }
        let azure = preset_capabilities("https://my-shop.openai.azure.com/", Protocol::Chat)
            .expect("preset");
        assert_eq!(
            azure.supported_protocols,
            vec![Protocol::Chat, Protocol::Responses]
        );
    }

    #[test]
    fn unknown_hosts_and_lookalike_suffixes_have_no_preset() {
        for url in [
            "https://relay.example.com/v1",
            "http://localhost:11434/v1",
            "https://evilopenai.azure.com/",
            "https://openai.azure.com.evil.example/",
            "not a url at all",
            "",
        ] {
            assert_eq!(preset_capabilities(url, Protocol::Chat), None, "{url}");
        }
    }

    #[test]
    fn a_protocol_the_vendor_list_misses_keeps_the_answers_but_no_list() {
        let preset =
            preset_capabilities("https://api.anthropic.com", Protocol::Chat).expect("preset");
        assert!(preset.supported_protocols.is_empty());
        assert_eq!(preset.streaming, Some(true));
    }

    #[test]
    fn existing_customized_metadata_is_never_overwritten() {
        let stored = ProviderCapabilities {
            supported_protocols: vec![Protocol::Chat],
            streaming: Some(false),
            tools: None,
            vision: None,
        };
        assert_eq!(
            capabilities_for_add(Some(&stored), "https://api.openai.com/v1", Protocol::Chat),
            stored
        );
        assert_eq!(
            capabilities_for_add(None, "https://relay.example.com/v1", Protocol::Chat),
            ProviderCapabilities::default()
        );
        let fresh = capabilities_for_add(None, "https://api.deepseek.com/v1", Protocol::Chat);
        assert_eq!(fresh.tools, Some(true));
    }
}
