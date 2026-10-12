//! Per-attempt streaming deadlines, independent of protocol and credentials.

use std::future::Future;
use std::time::Duration;

use magpie_core::stream::StreamEvent;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamTimeout {
    FirstContent,
    Idle,
}

impl StreamTimeout {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::FirstContent => "upstream first-content timeout",
            Self::Idle => "upstream stream idle timeout",
        }
    }
}

pub(crate) struct StreamClock {
    started: Instant,
    first_content: Option<Duration>,
    idle: Option<Duration>,
}

impl StreamClock {
    pub(crate) fn new(first_content_ms: u64, idle_ms: u64) -> Self {
        Self {
            started: Instant::now(),
            first_content: (first_content_ms > 0).then(|| Duration::from_millis(first_content_ms)),
            idle: (idle_ms > 0).then(|| Duration::from_millis(idle_ms)),
        }
    }

    pub(crate) fn observe(&mut self, event: &StreamEvent) {
        let content = match event {
            StreamEvent::TextDelta { text } | StreamEvent::ThinkingDelta { text } => {
                !text.is_empty()
            }
            StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallEnd { .. } => true,
            StreamEvent::ToolCallDelta { args, .. } => !args.is_empty(),
            _ => false,
        };
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
        &self,
        future: impl Future<Output = T>,
    ) -> Result<T, StreamTimeout> {
        // Start a fresh idle interval only when actually waiting for transport.
        // Time spent yielding to a backpressured downstream does not count.
        self.wait(future, self.idle).await
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
        let clock = StreamClock::new(0, 30);
        clock
            .headers(tokio::time::sleep(Duration::from_secs(1)))
            .await
            .unwrap();
        assert_eq!(
            clock.chunk(std::future::pending::<()>()).await,
            Err(StreamTimeout::Idle)
        );
        let clock = StreamClock::new(0, 0);
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
