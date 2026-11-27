use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::time::Instant;

use crate::api::EngineState;

const MAX_REST: Duration = Duration::from_secs(4 * 60 * 60);

pub struct Limits {
    base_rest: Duration,
    consecutive_blocks: u32,
    rest: Option<(Instant, DateTime<Utc>)>,
}

pub enum Admit {
    Send,
    Resting(DateTime<Utc>),
}

impl Limits {
    pub fn new(base_rest: Duration) -> Self {
        Self {
            base_rest,
            consecutive_blocks: 0,
            rest: None,
        }
    }

    pub fn admit(&mut self, now: Instant, _wall: DateTime<Utc>) -> Admit {
        match self.rest {
            Some((deadline, ready_at)) if now < deadline => Admit::Resting(ready_at),
            _ => Admit::Send,
        }
    }

    pub fn record(
        &mut self,
        state: EngineState,
        now: Instant,
        wall: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        match state {
            EngineState::Blocked => {
                self.consecutive_blocks = self.consecutive_blocks.saturating_add(1);
                let multiplier = 2_u32.saturating_pow(self.consecutive_blocks - 1);
                let duration = self.base_rest.saturating_mul(multiplier).min(MAX_REST);
                let ready_at =
                    wall + TimeDelta::from_std(duration).expect("rest is at most 4 hours");
                self.rest = Some((now + duration, ready_at));
                Some(ready_at)
            }
            EngineState::Ok => {
                self.consecutive_blocks = 0;
                self.rest = None;
                None
            }
            EngineState::Empty | EngineState::ParseFailed | EngineState::Error => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{advance, pause};

    #[tokio::test]
    async fn consecutive_blocks_double_rest_up_to_four_hours() {
        pause();
        let mut limits = Limits::new(Duration::from_secs(3600));
        let mut wall = Utc::now();
        for hours in [1, 2, 4, 4] {
            let rest = Duration::from_secs(hours * 3600);
            let ready_at = wall + TimeDelta::from_std(rest).unwrap();
            assert_eq!(
                limits.record(EngineState::Blocked, Instant::now(), wall),
                Some(ready_at)
            );

            advance(rest - Duration::from_secs(1)).await;
            assert!(matches!(
                limits.admit(Instant::now(), ready_at - TimeDelta::seconds(1)),
                Admit::Resting(deadline) if deadline == ready_at
            ));
            advance(Duration::from_secs(1)).await;
            assert!(matches!(
                limits.admit(Instant::now(), ready_at),
                Admit::Send
            ));
            wall = ready_at;
        }
    }

    #[tokio::test]
    async fn ok_clears_rest_and_resets_the_block_count() {
        pause();
        let mut limits = Limits::new(Duration::from_secs(10));
        let wall = Utc::now();
        limits.record(EngineState::Blocked, Instant::now(), wall);
        advance(Duration::from_secs(10)).await;
        let wall = wall + TimeDelta::seconds(10);
        limits.record(EngineState::Blocked, Instant::now(), wall);

        assert_eq!(limits.record(EngineState::Ok, Instant::now(), wall), None);
        assert!(matches!(limits.admit(Instant::now(), wall), Admit::Send));
        assert_eq!(
            limits.record(EngineState::Blocked, Instant::now(), wall),
            Some(wall + TimeDelta::seconds(10))
        );
    }

    #[tokio::test]
    async fn empty_parse_failed_and_error_preserve_rest_and_the_block_count() {
        pause();
        for state in [
            EngineState::Empty,
            EngineState::ParseFailed,
            EngineState::Error,
        ] {
            let mut limits = Limits::new(Duration::from_secs(10));
            let wall = Utc::now();
            let first_deadline = wall + TimeDelta::seconds(10);
            limits.record(EngineState::Blocked, Instant::now(), wall);

            advance(Duration::from_secs(5)).await;
            let wall = wall + TimeDelta::seconds(5);
            assert_eq!(
                limits.record(state, Instant::now(), wall),
                None,
                "{state:?}"
            );
            assert!(
                matches!(
                    limits.admit(Instant::now(), wall),
                    Admit::Resting(deadline) if deadline == first_deadline
                ),
                "{state:?}"
            );

            advance(Duration::from_secs(5)).await;
            assert!(
                matches!(limits.admit(Instant::now(), first_deadline), Admit::Send),
                "{state:?}"
            );
            assert_eq!(
                limits.record(EngineState::Blocked, Instant::now(), first_deadline),
                Some(first_deadline + TimeDelta::seconds(20)),
                "{state:?}"
            );
        }
    }
}
