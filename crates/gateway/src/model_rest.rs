//! Per-model timed rests for limits that belong to one model.
//!
//! Cline answers a free model's daily cap with 429 "Daily free limit
//! reached on model X. Try again in 23h 59m" (the reference's
//! per-model rest, #1398): the limit is that model's, so the model
//! alone rests until the time the refusal names while its provider's
//! other models still answer. The whole key resting instead would
//! refuse every other free model on the account without asking it.
//! Anything that is not one model's own named limit rests the key as
//! before, through the key cooldowns.
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// A model out of its own limit: when it may be asked again, and the
/// refusal's own words, which answer its next ask without asking the
/// vendor again.
#[derive(Debug, Clone)]
pub struct ModelRest {
    pub until: Instant,
    pub message: String,
}

/// Rests by provider id and model.
static RESTS: LazyLock<Mutex<HashMap<(String, String), ModelRest>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Forget every rest (tests).
#[doc(hidden)]
pub fn reset() {
    RESTS.lock().unwrap().clear();
}

/// Rest `provider_id/model` for `duration`: it is not asked again before
/// then, and its next ask is answered with `message`. A later rest holds
/// the later of the two times, with its message. Zero rests nothing.
pub fn rest(provider_id: &str, model: &str, duration: Duration, message: impl Into<String>) {
    if duration.is_zero() {
        return;
    }
    let rest = ModelRest {
        until: Instant::now() + duration,
        message: message.into(),
    };
    let mut rests = RESTS.lock().unwrap();
    rests
        .entry((provider_id.to_owned(), model.to_owned()))
        .and_modify(|known| {
            if rest.until > known.until {
                *known = rest.clone();
            }
        })
        .or_insert(rest);
}

/// The rest on `provider_id/model`, if one is still held. Rests already
/// past leave on read.
pub fn resting(provider_id: &str, model: &str) -> Option<ModelRest> {
    let mut rests = RESTS.lock().unwrap();
    let rest = rests
        .get(&(provider_id.to_owned(), model.to_owned()))
        .filter(|rest| rest.until > Instant::now())
        .cloned()?;
    rests.retain(|_, rest| rest.until > Instant::now());
    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rested_model_is_held_and_released() {
        reset();
        assert!(resting("cline", "m").is_none());
        rest("cline", "m", Duration::from_secs(60), "out of its day");
        let held = resting("cline", "m").expect("resting");
        assert_eq!(held.message, "out of its day");
        assert!(resting("cline", "other").is_none());
        assert!(resting("other", "m").is_none());
        reset();
        assert!(resting("cline", "m").is_none());
    }

    #[test]
    fn a_later_rest_holds_longer_and_zero_rests_nothing() {
        reset();
        rest("cline", "m", Duration::from_secs(60), "first");
        let first = resting("cline", "m").expect("resting");
        rest("cline", "m", Duration::from_secs(3600), "later");
        let longer = resting("cline", "m").expect("resting");
        assert!(longer.until >= first.until);
        assert_eq!(longer.message, "later");
        rest("cline", "n", Duration::ZERO, "never");
        assert!(resting("cline", "n").is_none());
        reset();
    }
}
