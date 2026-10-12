//! Structured status hints for errors carried inside an HTTP-200 SSE response.
//! Do not classify free-form messages: invalid requests/refusals must not become
//! quota failures merely because their prose contains a word like "limit".
use reqwest::StatusCode;
use serde_json::Value;

pub(crate) fn error_status(body: &Value) -> StatusCode {
    let error = body
        .pointer("/response/error")
        .or_else(|| body.get("error"))
        .unwrap_or(body);
    for field in ["status_code", "statusCode", "status", "code", "type"] {
        let Some(value) = error.get(field) else {
            continue;
        };
        if let Some(number) = value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
            && (400..=599).contains(&number)
        {
            return StatusCode::from_u16(number as u16).expect("HTTP error status");
        }
        let Some(code) = value.as_str() else { continue };
        let status = match code.to_ascii_lowercase().as_str() {
            "rate_limit_error" | "rate_limit_exceeded" | "resource_exhausted" => 429,
            "insufficient_quota" | "billing_error" => 402,
            "authentication_error" | "invalid_api_key" | "unauthenticated" => 401,
            "permission_error" | "permission_denied" | "access_denied" => 403,
            "not_found_error" | "not_found" | "model_not_found" => 404,
            "invalid_request_error"
            | "invalid_request"
            | "invalid_argument"
            | "invalid_prompt"
            | "content_policy_violation"
            | "content_filter"
            | "refusal" => 400,
            "context_length_exceeded" | "request_too_large" => 413,
            "overloaded_error" => 529,
            "api_error" | "server_error" | "internal" => 500,
            "unavailable" => 503,
            "timeout_error" | "deadline_exceeded" => 504,
            _ => continue,
        };
        return StatusCode::from_u16(status).expect("mapped HTTP error status");
    }
    StatusCode::BAD_GATEWAY
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn structured_codes_preserve_retry_and_refusal_distinctions() {
        for (error, status) in [
            (json!({"code": 429}), 429),
            (json!({"code": "429"}), 429),
            (json!({"type": "rate_limit_error"}), 429),
            (json!({"code": "rate_limit_exceeded"}), 429),
            (json!({"status": "RESOURCE_EXHAUSTED"}), 429),
            (json!({"code": "context_length_exceeded"}), 413),
            (
                json!({"type": "invalid_request_error", "message":"limit rate quota"}),
                400,
            ),
            (json!({"statusCode": 529}), 529),
            (json!({"type": "authentication_error"}), 401),
            (json!({"status": "UNAVAILABLE"}), 503),
            (json!({"code": 200, "type": "permission_error"}), 403),
            (json!({"code": 70000}), 502),
            (json!({"message": "rate limited"}), 502),
        ] {
            for body in [
                error.clone(),
                json!({"error": error}),
                json!({"type":"response.failed", "response":{"error":error}}),
            ] {
                assert_eq!(error_status(&body).as_u16(), status, "{body}");
            }
        }
    }
}
