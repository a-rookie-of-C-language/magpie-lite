//! Downstream-only SSE liveness. Comments never count as upstream progress.

use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

pub(crate) const COMMENT: &[u8] = b": keepalive\n";
const ACTIVE_GAP: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
struct Activity {
    heard: Instant,
    allowed: bool,
}

/// Transport activity is shared with the response owner, not a spawned task.
/// Disabled requests incur no channel allocation or per-chunk notification.
#[derive(Clone, Default)]
pub(crate) struct UpstreamActivity(Option<watch::Sender<Activity>>);

impl UpstreamActivity {
    pub(crate) fn attempt(&self) {
        self.headers(true);
    }

    pub(crate) fn headers(&self, success: bool) {
        if let Some(sender) = &self.0 {
            sender.send_replace(Activity {
                heard: Instant::now(),
                // A known HTTP error body is not a quiet successful stream.
                allowed: success,
            });
        }
    }

    pub(crate) fn chunk(&self, bytes: &[u8]) {
        if !bytes.is_empty()
            && let Some(sender) = &self.0
        {
            sender.send_modify(|activity| activity.heard = Instant::now());
        }
    }
}

pub(crate) struct Keepalive {
    // Retain a sender so wait() cannot spin on a closed notification channel.
    _activity: UpstreamActivity,
    receiver: watch::Receiver<Activity>,
    interval: Duration,
    max_silence: Duration,
}

impl Keepalive {
    pub(crate) fn new(interval_ms: u64, max_silence_ms: u64) -> (UpstreamActivity, Option<Self>) {
        if interval_ms == 0 || max_silence_ms == 0 {
            return (UpstreamActivity::default(), None);
        }
        let (sender, receiver) = watch::channel(Activity {
            heard: Instant::now(),
            allowed: true,
        });
        let activity = UpstreamActivity(Some(sender));
        let keepalive = Self {
            _activity: activity.clone(),
            receiver,
            interval: Duration::from_millis(interval_ms),
            max_silence: Duration::from_millis(max_silence_ms),
        };
        (activity, Some(keepalive))
    }

    fn due(&self, activity: Activity, wrote: Instant, passing: bool) -> Option<Instant> {
        if !activity.allowed || activity.heard.elapsed() >= self.max_silence {
            return None;
        }
        let gap = if passing && activity.heard > wrote {
            self.interval.min(ACTIVE_GAP)
        } else {
            self.interval
        };
        // Use checked addition for arbitrary deserialized u64 settings.
        let due = wrote.checked_add(gap)?;
        if due.saturating_duration_since(activity.heard) >= self.max_silence {
            return None;
        }
        Some(due)
    }

    /// Wait without consuming or restarting the upstream future. Activity can
    /// resume comments after the silence cap, but comments themselves cannot.
    pub(crate) async fn wait(&mut self, wrote: Instant, passing: bool) {
        loop {
            let activity = *self.receiver.borrow_and_update();
            let due = self.due(activity, wrote, passing);
            tokio::select! {
                biased;
                _ = self.receiver.changed() => {},
                _ = async {
                    match due {
                        Some(due) => tokio::time::sleep_until(due).await,
                        None => std::future::pending().await,
                    }
                } => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn quiet_comments_stop_at_cap_and_resume_only_after_upstream_activity() {
        let (activity, keepalive) = Keepalive::new(15_000, 45_000);
        let mut keepalive = keepalive.unwrap();
        let start = Instant::now();
        keepalive.wait(start, false).await;
        assert_eq!(start.elapsed(), Duration::from_secs(15));
        keepalive.wait(Instant::now(), false).await;
        assert_eq!(start.elapsed(), Duration::from_secs(30));
        assert!(
            tokio::time::timeout(
                Duration::from_secs(60),
                keepalive.wait(Instant::now(), false)
            )
            .await
            .is_err()
        );
        activity.chunk(b": upstream comment\n");
        keepalive.wait(Instant::now(), true).await;
        // The last downstream write is newer than this upstream activity.
        assert_eq!(start.elapsed(), Duration::from_secs(105));
    }

    #[tokio::test(start_paused = true)]
    async fn ignored_transport_uses_fast_gap_only_after_reply_is_passing() {
        let (activity, keepalive) = Keepalive::new(15_000, 300_000);
        let mut keepalive = keepalive.unwrap();
        let wrote = Instant::now();
        tokio::time::sleep(Duration::from_millis(100)).await;
        activity.chunk(b": ignored\n");
        keepalive.wait(wrote, false).await;
        assert_eq!(wrote.elapsed(), Duration::from_secs(15));
        let wrote = Instant::now();
        tokio::time::sleep(Duration::from_millis(100)).await;
        activity.chunk(b"partial frame");
        keepalive.wait(wrote, true).await;
        assert_eq!(wrote.elapsed(), Duration::from_secs(1));
        // Empty transport reads do not renew the silence budget.
        tokio::time::sleep(Duration::from_secs(300)).await;
        activity.chunk(b"");
        assert!(
            keepalive
                .due(*keepalive.receiver.borrow(), Instant::now(), true)
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn known_http_error_suppresses_comments_until_next_attempt() {
        let (activity, keepalive) = Keepalive::new(10, 50);
        let mut keepalive = keepalive.unwrap();
        activity.headers(false);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                keepalive.wait(Instant::now(), false)
            )
            .await
            .is_err()
        );
        activity.attempt();
        let start = Instant::now();
        keepalive.wait(start, false).await;
        assert_eq!(start.elapsed(), Duration::from_millis(10));
        assert!(Keepalive::new(0, 10).1.is_none());
        assert!(Keepalive::new(10, 0).1.is_none());
        let (_, huge) = Keepalive::new(u64::MAX, u64::MAX);
        let huge = huge.unwrap();
        assert!(
            huge.due(*huge.receiver.borrow(), Instant::now(), false)
                .is_none()
        );
    }
}
