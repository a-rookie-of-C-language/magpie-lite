//! Vendor-retired models, remembered and kept out of the catalog.
//!
//! Ported from the reference gateway's `provider.Retire` (retired.go).
//! A model its vendor answered with a retirement (410 "ModelDeprecated"
//! and shapes like it) leaves `/v1/models` and the bare-name catalogs
//! for that provider only, until the vendor answers a request for it
//! again or `RETIRED_FOR` has passed. A request that names it as
//! `provider/model` is still sent: the vendor's answer is what the agent
//! is told, and a model back in service is seen at once.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// How long a model retired so stays out of the lists: the vendor's list
/// is what brings it back, and the vendor's list still has it, so a
/// refresh can't be the end of it. A week later it is listed again, and
/// the next request it gets refused so takes it out again.
pub(crate) const RETIRED_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Default, Serialize, Deserialize)]
struct RetiredFile {
    /// `provider/model` → when the vendor said it is retired (unix ms).
    #[serde(flatten)]
    at: BTreeMap<String, u64>,
}

struct RetiredState {
    path: PathBuf,
    file: RetiredFile,
}

fn state() -> &'static Mutex<Option<RetiredState>> {
    static RETIRED: OnceLock<Mutex<Option<RetiredState>>> = OnceLock::new();
    RETIRED.get_or_init(|| Mutex::new(None))
}

fn unix_ms(now: SystemTime) -> u64 {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load(path: &PathBuf) -> RetiredFile {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => RetiredFile::default(),
    }
}

fn save(state: &mut RetiredState, now: SystemTime) {
    // Expired entries leave on the next write; the list is self-cleaning.
    let now_ms = unix_ms(now);
    let cutoff = now_ms.saturating_sub(RETIRED_FOR.as_millis() as u64);
    state.file.at.retain(|_, at| *at > cutoff);
    if let Ok(body) = serde_json::to_vec_pretty(&state.file) {
        if let Some(parent) = state.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = state.path.with_extension("json.tmp");
        if std::fs::write(&tmp, &body).is_ok() {
            let _ = std::fs::rename(&tmp, &state.path);
        }
    }
}

fn enabled() -> bool {
    state().lock().unwrap_or_else(|p| p.into_inner()).is_some()
}

fn with_state<T>(now: SystemTime, f: impl FnOnce(&mut RetiredState) -> T) -> Option<T> {
    let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
    let state = guard.as_mut()?;
    // A running process owns its in-memory view; `now` is the caller's
    // clock and stays with it.
    let _ = now;
    Some(f(state))
}

/// Point the store at `path`, reading what is saved there. The gateway
/// process calls this once at startup; tests call it per case.
pub fn init(path: PathBuf) {
    let file = load(&path);
    *state().lock().unwrap_or_else(|p| p.into_inner()) = Some(RetiredState { path, file });
}

/// Note that provider pid's vendor said model is retired: it leaves the
/// catalog for that provider until it answers again or the week passes.
pub(crate) fn retire(provider_id: &str, model: &str, now: SystemTime) {
    if !enabled() {
        return;
    }
    let key = format!("{provider_id}/{model}");
    with_state(now, |state| {
        state.file.at.insert(key, unix_ms(now));
        save(state, now);
    });
}

/// Take back a retirement when it has one: the vendor answered a request.
pub(crate) fn unretire(provider_id: &str, model: &str, now: SystemTime) {
    if !enabled() {
        return;
    }
    let key = format!("{provider_id}/{model}");
    with_state(now, |state| {
        if state.file.at.remove(&key).is_some() {
            save(state, now);
        }
    });
}

/// Says whether provider pid's vendor said model is retired, within
/// `RETIRED_FOR`.
pub fn is_retired(provider_id: &str, model: &str, now: SystemTime) -> bool {
    if !enabled() {
        return false;
    }
    let key = format!("{provider_id}/{model}");
    with_state(now, |state| {
        state
            .file
            .at
            .get(&key)
            .is_some_and(|at| unix_ms(now).saturating_sub(*at) < RETIRED_FOR.as_millis() as u64)
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store is process-global; tests that re-point it must not run
    /// concurrently.
    static STORE_LOCK: Mutex<()> = Mutex::new(());

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "magpie-lite-retired-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("retired.json")
    }

    fn clean(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

    #[test]
    fn retire_unretire_round_trip_and_expiry() {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = temp_file("round-trip");
        clean(&path);
        // A sibling test that panicked under the lock would leave this
        // one pointing at that failed test's file; a fresh empty init
        // proves the store re-points cleanly.
        init(path.clone());
        assert!(!is_retired("zen", "exo-free", SystemTime::UNIX_EPOCH));
        let now = SystemTime::now();
        assert!(!is_retired("zen", "exo-free", now));
        retire("zen", "exo-free", now);
        assert!(is_retired("zen", "exo-free", now));
        assert!(!is_retired("zen", "other", now));
        assert!(!is_retired("other", "exo-free", now));
        // The file is written and read back by another init.
        init(path.clone());
        assert!(is_retired("zen", "exo-free", now));
        unretire("zen", "exo-free", now);
        assert!(!is_retired("zen", "exo-free", now));
        clean(&path);
    }

    #[test]
    fn expiry_releases_the_model_after_a_week() {
        let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = temp_file("expiry");
        clean(&path);
        init(path.clone());
        let now = SystemTime::now();
        retire("zen", "exo-free", now);
        let later = now + RETIRED_FOR;
        assert!(!is_retired("zen", "exo-free", later));
        clean(&path);
    }
}
