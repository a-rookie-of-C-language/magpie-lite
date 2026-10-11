//! Quota/rate-limit cooldown hints from upstream responses.

use std::time::{Duration, SystemTime};

use httpdate::HttpDate;
use reqwest::header::HeaderMap;
use serde_json::Value;

/// Common headers that name how long until a quota or rate limit resets.
const RESET_HEADERS: [&str; 4] = [
    "x-ratelimit-reset-requests",
    "x-ratelimit-reset-tokens",
    "x-ratelimit-reset",
    "x-quota-reset",
];

/// Common headers that name the absolute time at which a limit resets.
const RESET_TIMESTAMP_HEADERS: [&str; 4] = [
    "x-ratelimit-reset-time",
    "x-ratelimit-reset-timestamp",
    "x-quota-reset-time",
    "x-quota-reset-timestamp",
];

/// Resolve the cooldown for a quota-limited key.
///
/// `Retry-After` is authoritative. Otherwise the longest usable reset hint
/// is used, including duration headers, absolute reset timestamps, and
/// retry-delay fields in an upstream error body. A configured cooldown of
/// zero disables hint-aware cooldown for that provider; otherwise it is the
/// fallback when the upstream provides no usable hint.
pub fn quota_cooldown(headers: &HeaderMap, body: Option<&Value>, fallback: Duration) -> Duration {
    if fallback.is_zero() {
        return Duration::ZERO;
    }
    if let Some(cooldown) = headers
        .get("retry-after")
        .and_then(header_value)
        .and_then(retry_after)
    {
        return cooldown;
    }

    let reset_hint = reset_header_cooldown(headers).max(body_retry_cooldown(body));
    reset_hint.unwrap_or(fallback)
}

fn header_value(value: &reqwest::header::HeaderValue) -> Option<&str> {
    value.to_str().ok().map(str::trim)
}

fn reset_header_cooldown(headers: &HeaderMap) -> Option<Duration> {
    RESET_HEADERS
        .into_iter()
        .filter_map(|name| headers.get(name))
        .filter_map(header_value)
        .filter_map(common_reset_hint)
        .chain(
            RESET_TIMESTAMP_HEADERS
                .into_iter()
                .filter_map(|name| headers.get(name))
                .filter_map(header_value)
                .filter_map(timestamp_duration),
        )
        .max()
}

/// Parse an HTTP `Retry-After` value: delay-seconds or an HTTP-date.
fn retry_after(value: &str) -> Option<Duration> {
    if let Some(delay) = reset_duration(value) {
        return Some(delay);
    }

    let target = SystemTime::from(value.parse::<HttpDate>().ok()?);
    target.duration_since(SystemTime::now()).ok()
}

/// A reset header may contain a duration or an absolute timestamp.
fn common_reset_hint(value: &str) -> Option<Duration> {
    if timestamp_shaped(value) {
        timestamp_duration(value)
    } else {
        reset_duration(value)
    }
}

/// Parse vendor reset values such as `120`, `1s`, `6m0s`, and `250ms`.
///
/// A number without a unit means seconds. Invalid or negative values are
/// ignored so a provider cannot accidentally disable its own cooldown.
fn reset_duration(value: &str) -> Option<Duration> {
    let text = value.trim();
    if text.is_empty() || text.starts_with('-') {
        return None;
    }

    let bytes = text.as_bytes();
    let mut cursor = 0;
    let mut millis = 0_u128;

    while cursor < bytes.len() {
        let whole_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == whole_start {
            return None;
        }
        let whole_end = cursor;
        let whole = text[whole_start..whole_end].parse::<u128>().ok()?;

        let mut fraction = 0_u128;
        let mut denominator = 1_u128;
        if cursor < bytes.len() && bytes[cursor] == b'.' {
            cursor += 1;
            let fraction_start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            if cursor == fraction_start {
                return None;
            }
            let digits = &text[fraction_start..cursor];
            let precision = digits.len().min(9);
            fraction = digits[..precision].parse::<u128>().ok()?;
            denominator = 10_u128.pow(precision as u32);
        }

        let rest = &text[cursor..];
        let (unit_millis, unit_len) = if rest.is_empty() {
            (1_000, 0)
        } else if rest.starts_with("ms") {
            (1, "ms".len())
        } else if rest.starts_with('s') {
            (1_000, 1)
        } else if rest.starts_with('m') {
            (60_000, 1)
        } else if rest.starts_with('h') {
            (3_600_000, 1)
        } else if rest.starts_with('d') {
            (86_400_000, 1)
        } else {
            return None;
        };
        cursor += unit_len;

        let component = whole.saturating_mul(unit_millis).saturating_add(
            fraction
                .saturating_mul(unit_millis)
                .checked_div(denominator)
                .unwrap_or(0),
        );
        millis = millis.saturating_add(component);
    }

    Some(Duration::from_millis(
        u64::try_from(millis).unwrap_or(u64::MAX),
    ))
}

fn timestamp_shaped(value: &str) -> bool {
    let text = value.trim();
    if text.bytes().all(|byte| byte.is_ascii_digit()) && !text.is_empty() {
        return numeric_timestamp_duration(text).is_some();
    }
    value.parse::<HttpDate>().is_ok()
        || time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).is_ok()
}

fn timestamp_duration(value: &str) -> Option<Duration> {
    let text = value.trim();
    if let Some(duration) = numeric_timestamp_duration(text) {
        return Some(duration);
    }

    if let Ok(date) = value.parse::<HttpDate>() {
        return SystemTime::from(date)
            .duration_since(SystemTime::now())
            .ok();
    }

    let target = time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()?
        .to_utc();
    let seconds = target.unix_timestamp();
    if seconds < 0 {
        return None;
    }
    let offset = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds as u64))?
        .checked_add(Duration::from_nanos(u64::from(target.nanosecond())))?;
    offset.duration_since(SystemTime::now()).ok()
}

/// Parse Unix timestamps in seconds or milliseconds.
///
/// Ten years is the cutoff for treating a bare number as epoch seconds. Real
/// quota cooldowns are much shorter, while current epoch-second timestamps
/// are far larger.
fn numeric_timestamp_duration(text: &str) -> Option<Duration> {
    let digits = text.trim();
    if digits.len() < 10 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = digits.parse::<u128>().ok()?;
    let millis = if digits.len() >= 13 {
        number
    } else if number >= 100_000_000 {
        number.saturating_mul(1_000)
    } else {
        return None;
    };
    let target = SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(
        u64::try_from(millis).unwrap_or(u64::MAX),
    ))?;
    target.duration_since(SystemTime::now()).ok()
}

fn body_retry_cooldown(body: Option<&Value>) -> Option<Duration> {
    let mut best = None;
    visit_retry_values(body?, &mut best);
    best
}

fn visit_retry_values(value: &Value, best: &mut Option<Duration>) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                if is_retry_delay_key(key)
                    && let Some(duration) = retry_value(child)
                {
                    merge_max(best, duration);
                }
                visit_retry_values(child, best);
            }
        }
        Value::Array(items) => {
            for child in items {
                visit_retry_values(child, best);
            }
        }
        _ => {}
    }
}

fn is_retry_delay_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase);
    matches!(
        normalized.collect::<String>().as_str(),
        "retryafter" | "retrydelay" | "retryin"
    )
}

fn retry_value(value: &Value) -> Option<Duration> {
    match value {
        Value::String(value) => reset_duration(value),
        Value::Number(value) => reset_duration(&value.to_string()),
        _ => None,
    }
}

fn merge_max(best: &mut Option<Duration>, candidate: Duration) {
    if best.is_none_or(|current| candidate > current) {
        *best = Some(candidate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(values: &[(&str, String)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.insert(
                reqwest::header::HeaderName::try_from(*name).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    fn epoch_millis_from_now(offset: Duration) -> String {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .checked_add(offset)
            .unwrap()
            .as_millis()
            .to_string()
    }

    #[test]
    fn retry_after_seconds_replace_the_fallback() {
        let headers = headers(&[("retry-after", "2".to_owned())]);
        assert_eq!(
            quota_cooldown(&headers, None, Duration::from_secs(60)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn retry_after_is_authoritative_over_body_and_reset_headers() {
        let headers = headers(&[
            ("retry-after", "2".to_owned()),
            ("x-ratelimit-reset", "10s".to_owned()),
        ]);
        let body = serde_json::json!({"error": {"retryAfter": "30s"}});
        assert_eq!(
            quota_cooldown(&headers, Some(&body), Duration::from_secs(60)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn retry_after_http_dates_are_relative_to_now() {
        let date = HttpDate::from(SystemTime::now() + Duration::from_secs(30)).to_string();
        let headers = headers(&[("retry-after", date)]);
        assert!(quota_cooldown(&headers, None, Duration::from_secs(60)) > Duration::from_secs(25));
    }

    #[test]
    fn reset_headers_use_the_longest_common_hint() {
        let headers = headers(&[
            ("x-ratelimit-reset-requests", "6m0s".to_owned()),
            ("x-ratelimit-reset-tokens", "250ms".to_owned()),
        ]);
        assert_eq!(
            quota_cooldown(&headers, None, Duration::from_secs(60)),
            Duration::from_secs(360)
        );
    }

    #[test]
    fn absolute_reset_timestamp_headers_work() {
        let target = SystemTime::now() + Duration::from_secs(35);
        let epoch_headers = headers(&[(
            "x-ratelimit-reset-timestamp",
            epoch_millis_from_now(Duration::from_secs(35)),
        )]);
        let cooldown = quota_cooldown(&epoch_headers, None, Duration::from_secs(60));
        assert!(cooldown > Duration::from_secs(30));
        assert!(cooldown <= Duration::from_secs(35));

        let epoch_seconds = (SystemTime::now() + Duration::from_secs(35))
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let epoch_seconds_headers = headers(&[("x-ratelimit-reset", epoch_seconds)]);
        assert!(
            quota_cooldown(&epoch_seconds_headers, None, Duration::from_secs(60))
                > Duration::from_secs(30)
        );
        let date_headers =
            headers(&[("x-ratelimit-reset-time", HttpDate::from(target).to_string())]);
        assert!(
            quota_cooldown(&date_headers, None, Duration::from_secs(60)) > Duration::from_secs(30)
        );

        let rfc3339_headers =
            headers(&[("x-ratelimit-reset-time", "2100-01-01T00:00:00Z".to_owned())]);
        assert!(
            quota_cooldown(&rfc3339_headers, None, Duration::from_secs(60))
                > Duration::from_secs(30)
        );
    }

    #[test]
    fn body_retry_delays_cover_nested_provider_shapes() {
        let body = serde_json::json!({
            "error": {
                "message": "Resource exhausted",
                "details": [
                    {"@type": "google.rpc.RetryInfo", "retryDelay": "27ms"},
                    {"retry_after": 10}
                ]
            }
        });
        assert_eq!(
            quota_cooldown(&HeaderMap::new(), Some(&body), Duration::from_secs(60)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn invalid_hints_fall_back_to_the_configured_cooldown() {
        let headers = headers(&[("x-ratelimit-reset", "tomorrow".to_owned())]);
        let body = serde_json::json!({"error": {"retryAfter": "later"}});
        assert_eq!(
            quota_cooldown(&headers, Some(&body), Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn zero_configuration_disables_header_hints() {
        let headers = headers(&[("retry-after", "120".to_owned())]);
        assert_eq!(
            quota_cooldown(&headers, None, Duration::ZERO),
            Duration::ZERO
        );
    }
}
