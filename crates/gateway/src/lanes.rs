//! Per-key concurrency slots and a rolling requests-per-minute window.
//!
//! Ports the reference gateway's `concurrency.go` (`lanes`) and `rpm.go`
//! (`rpms`). A provider's `max_concurrency` is how many requests may be out
//! at the vendor at once on each of its keys; one more waits in a queue, in
//! the order it came, and is sent when one out is done. A `max_rpm` caps the
//! requests a key sends in any rolling 60 seconds; one more waits until the
//! oldest request of the minute is a minute old. Waiting is not failing: a
//! queued request is never passed to a fallback for want of a slot, and the
//! key it waits for doesn't rest.
//!
//! Both are keyed by the candidate's `who` — the key, else the provider —
//! as the reference's `candidate.who()` does. The whole registry lives in
//! `AppState`, so a `LaneGuard`/`RpmTicket` dropped on cancellation or
//! shutdown gives its place back through an `Arc` that outlives the request.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::time::timeout;

/// The longest a request waits for room in the minute when the provider's
/// `queue_wait` isn't shorter (the reference's `rpmLongest`).
const RPM_LONGEST: Duration = Duration::from_secs(120);

/// One more than the window, so a request counted when it starts to go is
/// still counted a beat after the vendor sees it arrive: the first on a
/// connection arrives later than the next ones by its dial and TLS
/// handshake (the reference's `rpmSlack`, ce1ecaa4).
const RPM_SLACK: Duration = Duration::from_secs(1);

/// The rolling window `max_rpm` counts over (the reference's `rpmWindow`).
const RPM_WINDOW: Duration = Duration::from_secs(60);

/// One provider's per-key concurrency limit registry. Cloning shares the
/// same underlying map, so a `LaneGuard` can give its slot back after the
/// caller's request future is dropped.
#[derive(Clone, Default)]
pub struct Lanes {
    inner: Arc<Mutex<HashMap<String, Lane>>>,
}

struct Lane {
    limit: u32,
    busy: usize,
    queue: VecDeque<oneshot::Sender<()>>,
}

/// A held slot. Dropping it releases the slot and lets the next waiter go,
/// so a cancelled or finished request never holds the key's slot.
pub struct LaneGuard {
    lanes: Arc<Mutex<HashMap<String, Lane>>>,
    who: String,
}

impl Drop for LaneGuard {
    fn drop(&mut self) {
        let mut map = self.lanes.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(lane) = map.get_mut(&self.who) {
            lane.busy = lane.busy.saturating_sub(1);
            grant(lane);
            if lane.busy == 0 && lane.queue.is_empty() {
                map.remove(&self.who);
            }
        }
    }
}

/// Grant queued waiters while there is room.
fn grant(lane: &mut Lane) {
    while !lane.queue.is_empty() && (lane.limit == 0 || lane.busy < lane.limit as usize) {
        let Some(tx) = lane.queue.pop_front() else {
            break;
        };
        if tx.send(()).is_ok() {
            lane.busy += 1;
        }
        // A send that failed means the waiter is gone; its place is given on.
    }
}

/// Why a request was turned away from a key's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneError {
    /// The queue was at its bound (`queue_limit`).
    Full,
    /// It waited as long as `queue_wait` allows.
    Waited,
}

impl fmt::Display for LaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaneError::Full => f.write_str("queue full"),
            LaneError::Waited => f.write_str("waited too long"),
        }
    }
}

impl std::error::Error for LaneError {}

impl Lanes {
    /// Wait for one of `who`'s `limit` slots, in turn. `queue` bounds how
    /// many may wait (0: no bound); `wait` bounds how long one waits (0: as
    /// long as it takes). A `limit` of 0 takes no slot.
    ///
    /// The returned `LaneGuard` gives the slot back when dropped, so a
    /// request cancelled mid-queue or mid-send never leaks a slot.
    pub async fn take(
        &self,
        who: &str,
        limit: u32,
        queue: u32,
        wait: Duration,
    ) -> Result<LaneGuard, LaneError> {
        if limit == 0 {
            return Ok(LaneGuard {
                lanes: self.inner.clone(),
                who: who.to_owned(),
            });
        }
        let rx = {
            let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let lane = map.entry(who.to_owned()).or_insert_with(|| Lane {
                limit,
                busy: 0,
                queue: VecDeque::new(),
            });
            lane.limit = limit;
            grant(lane);
            if lane.busy < limit as usize && lane.queue.is_empty() {
                lane.busy += 1;
                return Ok(LaneGuard {
                    lanes: self.inner.clone(),
                    who: who.to_owned(),
                });
            }
            if queue > 0 && lane.queue.len() >= queue as usize {
                return Err(LaneError::Full);
            }
            let (tx, rx) = oneshot::channel();
            lane.queue.push_back(tx);
            rx
        };
        let sent = if wait > Duration::ZERO {
            match timeout(wait, rx).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(LaneError::Waited),
                Err(_) => Err(LaneError::Waited),
            }
        } else {
            match rx.await {
                Ok(()) => Ok(()),
                Err(_) => Err(LaneError::Waited),
            }
        };
        match sent {
            Ok(()) => Ok(LaneGuard {
                lanes: self.inner.clone(),
                who: who.to_owned(),
            }),
            Err(e) => {
                self.remove_waiter(who);
                Err(e)
            }
        }
    }

    /// Whether a request for `who` would be sent at once: no limit, or a
    /// slot free and nobody waiting for it.
    pub fn free(&self, who: &str, limit: u32) -> bool {
        if limit == 0 {
            return true;
        }
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(lane) = map.get(who) else {
            return true;
        };
        lane.busy < limit as usize && lane.queue.is_empty()
    }

    /// A snapshot of each key's lane, for the console: (who, busy, waiting,
    /// limit).
    pub fn snapshot(&self) -> Vec<(String, usize, usize, u32)> {
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.iter()
            .map(|(who, lane)| (who.clone(), lane.busy, lane.queue.len(), lane.limit))
            .collect()
    }

    /// Drop the first still-waiting request of `who`, if any. Called when a
    /// queued waiter's receiver is gone (dropped future, or its timeout
    /// fired): the slot it may already have been granted is given on.
    fn remove_waiter(&self, who: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(lane) = map.get_mut(who) {
            // Find the first waiter whose sender can still fire (the one
            // that timed out has a dead receiver). If none, a grant already
            // went through: the busy count is given back and the next
            // waiter let in.
            let mut refunded = false;
            for _ in 0..lane.queue.len() {
                let tx = lane.queue.pop_front().unwrap();
                if tx.is_closed() {
                    refunded = true;
                    break;
                }
                lane.queue.push_back(tx);
            }
            if refunded {
                lane.busy = lane.busy.saturating_sub(1);
            } else if lane.busy > 0 {
                lane.busy -= 1;
            }
            grant(lane);
            if lane.busy == 0 && lane.queue.is_empty() {
                map.remove(who);
            }
        }
    }
}

/// A rolling requests-per-minute window per key. `wait` parks a request
/// until the minute has room and counts it; `give_back` returns a reserved
/// time not used (its agent gone), and `free`/`used` report the window.
#[derive(Clone, Default)]
pub struct Rpms {
    inner: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl Rpms {
    /// The time `who`'s next request may go: now when the minute has room,
    /// else a minute (plus the dial slack) after the limit-th from the end.
    fn reserve(&self, who: &str, limit: u32, longest: Duration) -> Result<Instant, RpmError> {
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let times = map.entry(who.to_owned()).or_default();
        times.retain(|t| now.duration_since(*t) < RPM_WINDOW + RPM_SLACK);
        let mut at = now;
        if times.len() >= limit as usize {
            let idx = times.len() - limit as usize;
            at = times[idx] + RPM_WINDOW + RPM_SLACK;
        }
        let delay = at.saturating_duration_since(now);
        if longest > Duration::ZERO && delay > longest {
            return Err(RpmError {
                limit,
                after: delay,
                wait: longest,
            });
        }
        times.push(at);
        Ok(at)
    }

    /// Wait for room in `who`'s minute and count the request in it. Returns
    /// how long it waited, or an `RpmError` when the wait would be longer
    /// than `longest`. A `limit` of 0 counts nothing.
    ///
    /// Cancellation is safe: the caller holds the reserved `Instant` in an
    /// `RpmTicket`; dropping it calls `give_back` and the place is returned.
    pub async fn wait(
        &self,
        who: &str,
        limit: u32,
        longest: Duration,
    ) -> Result<Duration, RpmError> {
        if limit == 0 {
            return Ok(Duration::ZERO);
        }
        let at = self.reserve(who, limit, longest)?;
        let delay = at.saturating_duration_since(Instant::now());
        if delay.is_zero() {
            return Ok(Duration::ZERO);
        }
        tokio::time::sleep(delay).await;
        Ok(delay)
    }

    /// Take back a reserved time not used (its agent gone while it waited).
    pub fn give_back(&self, who: &str, at: Instant) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(times) = map.get_mut(who) {
            if let Some(pos) = times.iter().rposition(|t| *t == at) {
                times.remove(pos);
            }
            if times.is_empty() {
                map.remove(who);
            }
        }
    }

    /// How many of `who`'s requests are counted in its minute now, for the
    /// console: (who, used), oldest lanes of idle keys dropped.
    pub fn snapshot(&self) -> Vec<(String, usize)> {
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, times| {
            times.retain(|t| now.duration_since(*t) < RPM_WINDOW + RPM_SLACK);
            !times.is_empty()
        });
        let mut out = map
            .iter()
            .map(|(who, times)| (who.clone(), times.len()))
            .collect::<Vec<_>>();
        out.sort();
        out
    }

    /// Whether a request for `who` would go at once: no limit, or room in
    /// its minute.
    pub fn free(&self, who: &str, limit: u32) -> bool {
        if limit == 0 {
            return true;
        }
        let now = Instant::now();
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.get(who)
            .map(|times| {
                times
                    .iter()
                    .filter(|t| now.duration_since(**t) < RPM_WINDOW + RPM_SLACK)
                    .count()
                    < limit as usize
            })
            .unwrap_or(true)
    }

    /// How many of `who`'s requests are counted in its minute now.
    #[cfg(test)]
    pub fn used(&self, who: &str) -> usize {
        let now = Instant::now();
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.get(who)
            .map(|times| {
                times
                    .iter()
                    .filter(|t| now.duration_since(**t) < RPM_WINDOW + RPM_SLACK)
                    .count()
            })
            .unwrap_or(0)
    }
}

/// A request turned away for a wait longer than it may keep.
#[derive(Debug)]
pub struct RpmError {
    /// The per-minute limit it hit.
    pub limit: u32,
    /// How long until the minute has room.
    pub after: Duration,
    /// The wait bound that was exceeded.
    pub wait: Duration,
}

impl fmt::Display for RpmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} requests a minute is its limit, and the next could go in {:.0}s, longer than a request waits ({:.0}s)",
            self.limit,
            self.after.as_secs_f64().ceil(),
            self.wait.as_secs_f64()
        )
    }
}

impl std::error::Error for RpmError {}

impl RpmError {
    /// The Retry-After it carries, in whole seconds, at least 1.
    pub fn retry_after(&self) -> u64 {
        self.after.as_secs_f64().ceil().max(1.0) as u64
    }
}

/// Whether a request would go at once has room: a slot free in its lane
/// and room in its minute.
pub fn ready(lanes: &Lanes, rpms: &Rpms, who: &str, lane_limit: u32, rpm_limit: u32) -> bool {
    lanes.free(who, lane_limit) && rpms.free(who, rpm_limit)
}

/// The longest a request may wait for room in the minute when the
/// provider's `queue_wait` isn't shorter.
pub fn rpm_longest(provider: &crate::config::ProviderConfig) -> Duration {
    let wait = provider.queue_wait();
    if wait > Duration::ZERO && wait < RPM_LONGEST {
        wait
    } else {
        RPM_LONGEST
    }
}
