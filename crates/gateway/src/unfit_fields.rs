//! In-memory memory of optional request fields a provider's model has
//! refused on a protocol. The reference gateway's `unfit` map: once a
//! vendor named a field it doesn't take and a retry without it worked,
//! later requests to the same provider, model and protocol are built
//! without it. Process-local by design; the reference's is too.

use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Default)]
struct UnfitFields(HashSet<String>);

fn set() -> &'static Mutex<UnfitFields> {
    static SET: std::sync::OnceLock<Mutex<UnfitFields>> = std::sync::OnceLock::new();
    SET.get_or_init(|| Mutex::new(UnfitFields::default()))
}

fn key(provider_id: &str, model: &str, protocol: &str, field: &str) -> String {
    format!("{provider_id}\u{0}{model}\u{0}{protocol}\u{0}{field}")
}

/// Remember that provider pid's model turned `field` away on `protocol`.
pub(crate) fn refuse(provider_id: &str, model: &str, protocol: &str, field: &str) {
    set()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .0
        .insert(key(provider_id, model, protocol, field));
}

/// Says whether provider pid's model has turned `field` away on
/// `protocol` before.
pub(crate) fn unfit(provider_id: &str, model: &str, protocol: &str, field: &str) -> bool {
    set()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .0
        .contains(&key(provider_id, model, protocol, field))
}
