//! Model-retirement and model-refusal heuristics for cross-route fallback.
//!
//! Ported from the reference gateway's `modelRetired` / `modelRefused`.
//! Free-form prose is matched only for these two vendor shapes, and never
//! for quota or rate-limit classification, which stay status-code driven.
//! The error `body` should be the vendor JSON error object (or a wrapper
//! around it); matching uses the raw text of that JSON so that both the
//! `"type": "ModelDeprecated"` field and the human message are visible.
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

/// How a vendor says "this model no longer exists here".
fn retired_words() -> &'static Regex {
    static RETIRED: OnceLock<Regex> = OnceLock::new();
    RETIRED.get_or_init(|| {
        Regex::new(r#"(?i)"ModelDeprecated"|model_deprecated|model.{0,80}(has been|was|is now|is) (deprecated|retired|discontinued|decommissioned|sunset)"#)
            .expect("retired words regex")
    })
}

/// How a vendor says "this model is not one this key (or this API
/// dialect) serves" — the same model may still exist for another
/// provider, or another key on this one.
fn unserved_words() -> &'static Regex {
    static UNSERVED: OnceLock<Regex> = OnceLock::new();
    UNSERVED.get_or_init(|| {
        Regex::new(r#"(?i)model.{0,80}(not (supported|accessible|available|found|enabled|allowed)|unsupported|does ?n[o']t exist|unknown|invalid)|(no such|unknown|invalid|unsupported) model|model_not_found|模型.{0,12}(不存在|不支持|无权|未开通)"#)
            .expect("unserved words regex")
    })
}

/// OpenCode Zen's 401 for a model that left its free list: the key is
/// still good, but the model is gone. The vendor's full JSON error is
/// `{"type":"error","error":{"type":"ModelError","message":"Model
/// glm-5-free is not supported"}}`.
fn zen_model_gone() -> &'static Regex {
    static ZEN: OnceLock<Regex> = OnceLock::new();
    ZEN.get_or_init(|| {
        Regex::new(r#"(^|[\s:"\\])Model [^"\s\\]+ is not supported("|\s*$)"#)
            .expect("zen model gone regex")
    })
}

fn error_text(body: &Value) -> String {
    let error = body
        .get("error")
        .or_else(|| body.pointer("/response/error"))
        .unwrap_or(body);
    error.to_string()
}

/// The vendor says this model is retired there: another provider of the
/// same model may still serve it, so the next route is worth trying.
pub(crate) fn model_retired(status: u16, body: &Value) -> bool {
    match status {
        400 | 404 | 410 | 422 => {}
        401 => {
            return zen_model_gone().is_match(&body.to_string());
        }
        _ => return false,
    }
    let text = error_text(body);
    retired_words().is_match(&text) && !quota_words().is_match(&text)
}

/// The vendor turned this request away over its model alone: not one
/// this key's plan serves, or not one this endpoint serves. The next
/// route may still serve it.
pub(crate) fn model_refused(status: u16, body: &Value) -> bool {
    if model_retired(status, body) {
        return true;
    }
    if !matches!(status, 400 | 403 | 404 | 422) {
        return false;
    }
    let text = error_text(body);
    unserved_words().is_match(&text)
        && !quota_words().is_match(&text)
        && !refused_words().is_match(&text)
}

fn quota_words() -> &'static Regex {
    static QUOTA: OnceLock<Regex> = OnceLock::new();
    QUOTA.get_or_init(|| {
        Regex::new(r#"(?i)quota|insufficient|balance|credit|billing|out of budget|budget (exceeded|exhausted)|exceeded|rate.?limit|usage.?limit|limit.?reached|hit your .*limit|limit.{0,24}resets|too many requests|overloaded|余额|额度|欠费|限流|频率|套餐|用量|上限"#)
            .expect("quota words regex")
    })
}

fn refused_words() -> &'static Regex {
    static REFUSED: OnceLock<Regex> = OnceLock::new();
    REFUSED.get_or_init(|| {
        Regex::new(r#"(?i)unapproved channel|illegal api invocation"#).expect("refused words regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retired_status_codes_and_wording() {
        for status in [400, 404, 410, 422] {
            assert!(model_retired(
                status,
                &json!({"error": {"type": "ModelDeprecated", "message": "Model exo-free has been deprecated."}})
            ));
        }
        assert!(model_retired(
            410,
            &json!({"error": {"message": "model_deprecated"}})
        ));
        assert!(model_retired(
            400,
            &json!({"error": {"message": "The model gpt-x was retired on 2026-01-01."}})
        ));
        assert!(model_retired(
            422,
            &json!({"error": {"message": "this model is now discontinued"}})
        ));
        // 401 only retires through Zen's model-gone wording.
        assert!(!model_retired(
            401,
            &json!({"error": {"type": "ModelDeprecated", "message": "gone"}})
        ));
        assert!(model_retired(
            401,
            &json!({"error": {"type": "ModelError", "message": "Model glm-5-free is not supported"}})
        ));
        assert!(!model_retired(
            401,
            &json!({"error": {"message": "bad key"}})
        ));
        // Quota wording wins even on a retirement-shaped 410.
        assert!(!model_retired(
            410,
            &json!({"error": {"type": "ModelDeprecated", "message": "Model x has been deprecated, your quota is exceeded"}})
        ));
        assert!(!model_retired(
            410,
            &json!({"error": {"message": "unknown error"}})
        ));
        assert!(!model_retired(403, &json!({"error": {"message": "no"}})));
    }

    #[test]
    fn refused_covers_unserved_wording_without_quota() {
        for status in [400, 403, 404, 422] {
            assert!(model_refused(
                status,
                &json!({"error": {"message": "model deepseek-v4.1-flash is not available in the current token plan"}})
            ));
        }
        assert!(!model_refused(
            400,
            &json!({"error": {"message": "model x is not supported, your credit is exhausted"}})
        ));
        assert!(!model_refused(
            500,
            &json!({"error": {"message": "model not supported"}})
        ));
        // Retirement implies refused.
        assert!(model_refused(
            410,
            &json!({"error": {"message": "The model was retired."}})
        ));
    }

    #[test]
    fn wrapper_error_objects_are_matched() {
        let body = json!({
            "type": "response.failed",
            "response": {"error": {"message": "model not_found: no such model"}}
        });
        assert!(model_refused(404, &body));
    }
}
