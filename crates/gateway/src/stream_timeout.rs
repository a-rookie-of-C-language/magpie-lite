//! Upstream deadlines and the request-wide Responses opening-event hold budget.

use std::future::Future;
use std::time::Duration;

use magpie_core::stream::StreamEvent;
use tokio::time::Instant;

/// A Responses agent may ignore SSE comments for its event-idle timeout. Once
/// this request-wide budget expires, release a real buffered opening event.
/// Unlike upstream deadlines this never fails or resets on retry, or fabricates
/// metadata when the upstream has not supplied an encodable Start event.
#[derive(Clone, Copy)]
pub(crate) struct ResponsesHold(Option<Instant>);

impl ResponsesHold {
    pub(crate) fn new(timeout_ms: u64) -> Self {
        Self(
            (timeout_ms > 0)
                .then(|| Instant::now().checked_add(Duration::from_millis(timeout_ms)))
                .flatten(),
        )
    }

    pub(crate) async fn wait(&self) {
        match self.0 {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    }
}

/// One upstream attempt's group budget. Use the same absolute deadline for
/// headers and opening prefetch, then discard it once the candidate commits.
/// Never apply it to the final available route or to an already-known HTTP error.
pub(crate) struct GroupFirstToken(Option<Instant>);

impl GroupFirstToken {
    pub(crate) fn new(timeout_ms: u64) -> Self {
        Self(
            (timeout_ms > 0)
                .then(|| Instant::now().checked_add(Duration::from_millis(timeout_ms)))
                .flatten(),
        )
    }

    pub(crate) async fn wait<T>(
        &self,
        future: impl Future<Output = T>,
    ) -> Result<T, StreamTimeout> {
        let Some(deadline) = self.0 else {
            return Ok(future.await);
        };
        tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| StreamTimeout::GroupFirstToken)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamTimeout {
    FirstContent,
    Idle,
    GroupFirstToken,
}

impl StreamTimeout {
    pub(crate) fn attempt_status(self) -> &'static str {
        match self {
            Self::GroupFirstToken => "group_first_token_timeout",
            _ => "504",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::FirstContent => "upstream first-content timeout",
            Self::Idle => "upstream stream idle timeout",
            Self::GroupFirstToken => "group first-token timeout",
        }
    }
}

pub(crate) struct StreamClock {
    started: Instant,
    first_content: Option<Duration>,
    idle: Option<Duration>,
    remaining_idle: Option<Duration>,
}

impl StreamClock {
    pub(crate) fn new(first_content_ms: u64, idle_ms: u64) -> Self {
        Self {
            started: Instant::now(),
            first_content: (first_content_ms > 0).then(|| Duration::from_millis(first_content_ms)),
            idle: (idle_ms > 0).then(|| Duration::from_millis(idle_ms)),
            remaining_idle: None,
        }
    }

    pub(crate) fn observe(&mut self, event: &StreamEvent) {
        let content = event.is_content();
        if content {
            self.first_content = None;
        }
    }

    pub(crate) async fn headers<T>(
        &self,
        future: impl Future<Output = T>,
    ) -> Result<T, StreamTimeout> {
        self.wait(future, None).await
    }

    pub(crate) async fn chunk<T>(
        &mut self,
        future: impl Future<Output = T>,
    ) -> Result<T, StreamTimeout> {
        // Start a fresh idle interval only when actually waiting for transport.
        // Time spent yielding to a backpressured downstream does not count.
        let idle = self.remaining_idle.take().or(self.idle);
        self.wait(future, idle).await
    }

    /// Interrupt one upstream read to release buffered opening metadata. Carry
    /// the unused idle budget across the yield, without counting downstream
    /// backpressure or granting a new full interval for this same read.
    pub(crate) async fn held_chunk<T>(
        &mut self,
        future: impl Future<Output = T>,
        hold: &ResponsesHold,
    ) -> Result<Option<T>, StreamTimeout> {
        let began = Instant::now();
        let idle = self.remaining_idle.take().or(self.idle);
        let result = self
            .wait(
                async {
                    tokio::select! {
                        biased;
                        chunk = future => Some(chunk),
                        _ = hold.wait() => None,
                    }
                },
                idle,
            )
            .await;
        if matches!(result, Ok(None)) {
            self.remaining_idle = idle.map(|limit| limit.saturating_sub(began.elapsed()));
        }
        result
    }

    async fn wait<T>(
        &self,
        future: impl Future<Output = T>,
        idle: Option<Duration>,
    ) -> Result<T, StreamTimeout> {
        let first = self
            .first_content
            .map(|limit| limit.saturating_sub(self.started.elapsed()));
        let deadline = match (first, idle) {
            (Some(first), Some(idle)) if idle < first => Some((idle, StreamTimeout::Idle)),
            (Some(first), _) => Some((first, StreamTimeout::FirstContent)),
            (None, Some(idle)) => Some((idle, StreamTimeout::Idle)),
            (None, None) => None,
        };
        let Some((remaining, reason)) = deadline else {
            return Ok(future.await);
        };
        // A permanently-ready drip of ignored chunks must not outrun timeout's
        // polling semantics and reset or starve the absolute first-content cap.
        if remaining.is_zero() {
            return Err(reason);
        }
        tokio::time::timeout(remaining, future)
            .await
            .map_err(|_| reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn group_budget_covers_headers_and_prefetch_without_rearming() {
        let group = GroupFirstToken::new(300);
        let began = Instant::now();
        assert_eq!(
            group
                .wait(async {
                    tokio::time::sleep(Duration::from_millis(180)).await;
                    7
                })
                .await,
            Ok(7)
        );
        assert_eq!(
            group.wait(std::future::pending::<()>()).await,
            Err(StreamTimeout::GroupFirstToken)
        );
        assert_eq!(began.elapsed(), Duration::from_millis(300));
        let next_attempt = GroupFirstToken::new(300);
        assert_eq!(
            next_attempt.wait(std::future::pending::<()>()).await,
            Err(StreamTimeout::GroupFirstToken)
        );
        assert_eq!(began.elapsed(), Duration::from_millis(600));
    }

    #[tokio::test(start_paused = true)]
    async fn group_budget_disabled_extreme_and_ready_result_semantics() {
        let disabled = GroupFirstToken::new(0);
        assert_eq!(
            disabled
                .wait(async {
                    tokio::time::sleep(Duration::from_secs(600)).await;
                    7
                })
                .await,
            Ok(7)
        );
        assert_eq!(
            GroupFirstToken::new(u64::MAX)
                .wait(std::future::ready(7))
                .await,
            Ok(7)
        );
        let expired = GroupFirstToken::new(100);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Like timeout_at, a ready result wins over a simultaneously-ready cap.
        assert_eq!(expired.wait(std::future::ready(7)).await, Ok(7));
        assert_eq!(
            expired.wait(std::future::pending::<()>()).await,
            Err(StreamTimeout::GroupFirstToken)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn responses_hold_defaults_to_four_minutes_and_is_not_rearmed_by_waiting() {
        let hold = ResponsesHold::new(240_000);
        let started = Instant::now();
        tokio::time::sleep(Duration::from_secs(180)).await;
        hold.wait().await;
        assert_eq!(started.elapsed(), Duration::from_secs(240));
        // The next attempt shares the same expired budget, not another 4 min.
        hold.wait().await;
        assert_eq!(started.elapsed(), Duration::from_secs(240));
    }

    #[tokio::test(start_paused = true)]
    async fn opening_release_preserves_remaining_idle_and_excludes_yield_backpressure() {
        let mut clock = StreamClock::new(0, 300);
        let hold = ResponsesHold::new(120);
        let began = Instant::now();
        assert_eq!(
            clock.held_chunk(std::future::pending::<()>(), &hold).await,
            Ok(None)
        );
        assert_eq!(began.elapsed(), Duration::from_millis(120));
        // The opening event is downstream-backpressured for longer than idle.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            clock.chunk(std::future::pending::<()>()).await,
            Err(StreamTimeout::Idle)
        );
        assert_eq!(began.elapsed(), Duration::from_millis(900));
    }

    #[tokio::test(start_paused = true)]
    async fn opening_release_does_not_rearm_the_absolute_first_content_deadline() {
        let mut clock = StreamClock::new(300, 0);
        let hold = ResponsesHold::new(120);
        let began = Instant::now();
        assert_eq!(
            clock.held_chunk(std::future::pending::<()>(), &hold).await,
            Ok(None)
        );
        assert_eq!(
            clock.chunk(std::future::pending::<()>()).await,
            Err(StreamTimeout::FirstContent)
        );
        assert_eq!(began.elapsed(), Duration::from_millis(300));
    }

    #[tokio::test(start_paused = true)]
    async fn responses_hold_zero_is_disabled_and_extreme_settings_are_safe() {
        for hold in [ResponsesHold::new(0), ResponsesHold::new(u64::MAX)] {
            assert!(
                tokio::time::timeout(Duration::from_secs(3600), hold.wait())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn first_content_includes_headers_and_cannot_be_extended_by_metadata() {
        let mut clock = StreamClock::new(100, 80);
        clock
            .headers(tokio::time::sleep(Duration::from_millis(40)))
            .await
            .unwrap();
        clock.observe(&StreamEvent::Start {
            model: "fixture".into(),
        });
        clock.observe(&StreamEvent::TextDelta {
            text: String::new(),
        });
        clock.observe(&StreamEvent::Usage(Default::default()));
        clock
            .chunk(tokio::time::sleep(Duration::from_millis(40)))
            .await
            .unwrap();
        assert_eq!(
            clock.chunk(std::future::pending::<()>()).await,
            Err(StreamTimeout::FirstContent)
        );
        assert_eq!(clock.started.elapsed(), Duration::from_millis(100));
        // Even a ready future cannot revive an already expired first deadline.
        assert_eq!(
            clock.chunk(std::future::ready(())).await,
            Err(StreamTimeout::FirstContent)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn content_disarms_first_deadline_but_idle_resets_only_for_each_read() {
        for event in [
            StreamEvent::TextDelta {
                text: "answer".into(),
            },
            StreamEvent::ThinkingDelta {
                text: "reason".into(),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call".into(),
                name: "tool".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args: "{}".into(),
            },
        ] {
            let mut clock = StreamClock::new(30, 100);
            clock.observe(&event);
            clock
                .chunk(tokio::time::sleep(Duration::from_millis(80)))
                .await
                .unwrap();
            clock
                .chunk(tokio::time::sleep(Duration::from_millis(80)))
                .await
                .unwrap();
            // Simulate downstream backpressure before requesting another chunk.
            tokio::time::sleep(Duration::from_millis(200)).await;
            clock
                .chunk(tokio::time::sleep(Duration::from_millis(80)))
                .await
                .unwrap();
            assert_eq!(
                clock.chunk(std::future::pending::<()>()).await,
                Err(StreamTimeout::Idle)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_can_expire_before_content_and_zero_disables_each_limit() {
        assert_eq!(
            StreamClock::new(100, 30)
                .chunk(std::future::pending::<()>())
                .await,
            Err(StreamTimeout::Idle)
        );
        let mut clock = StreamClock::new(0, 30);
        clock
            .headers(tokio::time::sleep(Duration::from_secs(1)))
            .await
            .unwrap();
        assert_eq!(
            clock.chunk(std::future::pending::<()>()).await,
            Err(StreamTimeout::Idle)
        );
        let mut clock = StreamClock::new(0, 0);
        clock
            .chunk(tokio::time::sleep(Duration::from_secs(3600)))
            .await
            .unwrap();
        StreamClock::new(u64::MAX, u64::MAX)
            .chunk(std::future::ready(()))
            .await
            .unwrap();
    }
}
