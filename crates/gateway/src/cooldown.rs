//! Quota/rate-limit cooldown hints from upstream response headers.

use std::time::{Duration, SystemTime};

use httpdate::HttpDate;
use reqwest::header::HeaderMap;

/// Common headers that name how long until a quota or rate limit resets.
const RESET_HEADERS: [&str; 4] = [
    "x-ratelimit-reset-requests",
    "x-ratelimit-reset-tokens",
    "x-ratelimit-reset",
    "x-quota-reset",
];

/// Resolve the cooldown for a quota-limited key.
///
/// `Retry-After` is authoritative. Otherwise the longest common
/// rate-limit reset header is used. A configured cooldown of zero disables
/// hint-aware cooldown for that provider; otherwise it is the fallback when
/// the upstream provides no usable hint.
pub fn quota_cooldown(headers: &HeaderMap, fallback: Duration) -> Duration {
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

    RESET_HEADERS
        .into_iter()
        .filter_map(|name| headers.get(name))
        .filter_map(header_value)
        .filter_map(reset_duration)
        .max()
        .unwrap_or(fallback)
}

fn header_value(value: &reqwest::header::HeaderValue) -> Option<&str> {
    value.to_str().ok().map(str::trim)
}

/// Parse an HTTP `Retry-After` value: delay-seconds or an HTTP-date.
fn retry_after(value: &str) -> Option<Duration> {
    if let Some(delay) = reset_duration(value) {
        return Some(delay);
    }

    let target = SystemTime::from(value.parse::<HttpDate>().ok()?);
    target.duration_since(SystemTime::now()).ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.insert(
                reqwest::header::HeaderName::try_from(*name).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn retry_after_seconds_replace_the_fallback() {
        let headers = headers(&[("retry-after", "2")]);
        assert_eq!(
            quota_cooldown(&headers, Duration::from_secs(60)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn retry_after_http_dates_are_relative_to_now() {
        let date = HttpDate::from(SystemTime::now() + Duration::from_secs(30)).to_string();
        let headers = headers(&[("retry-after", &date)]);
        assert!(quota_cooldown(&headers, Duration::from_secs(60)) > Duration::from_secs(25));
    }

    #[test]
    fn reset_headers_use_the_longest_common_hint() {
        let headers = headers(&[
            ("x-ratelimit-reset-requests", "6m0s"),
            ("x-ratelimit-reset-tokens", "250ms"),
        ]);
        assert_eq!(
            quota_cooldown(&headers, Duration::from_secs(60)),
            Duration::from_secs(360)
        );
    }

    #[test]
    fn invalid_hints_fall_back_to_the_configured_cooldown() {
        let headers = headers(&[("x-ratelimit-reset", "tomorrow")]);
        assert_eq!(
            quota_cooldown(&headers, Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn zero_configuration_disables_header_hints() {
        let headers = headers(&[("retry-after", "120")]);
        assert_eq!(quota_cooldown(&headers, Duration::ZERO), Duration::ZERO);
    }
}
