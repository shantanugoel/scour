use std::collections::VecDeque;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::time::Instant;

use crate::api::EngineState;

const MAX_REST: Duration = Duration::from_secs(4 * 60 * 60);
// A 30 s pace wait plus the 10 s engine timeout fits within QUERY_BUDGET.
const MAX_PACE_WAIT: Duration = Duration::from_secs(30);
const DAILY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy)]
pub struct LimitSettings {
    pub burst: u32,
    pub refill: Duration,
    pub daily: u32,
}

impl Default for LimitSettings {
    fn default() -> Self {
        Self {
            burst: 3,
            refill: Duration::from_secs(120),
            daily: 100,
        }
    }
}

pub struct Limits {
    base_rest: Duration,
    settings: LimitSettings,
    burst_credit: Duration,
    consecutive_blocks: u32,
    rest: Option<(Instant, DateTime<Utc>)>,
    last_state: Option<EngineState>,
    pace_cursor: Option<(Instant, DateTime<Utc>)>,
    daily: VecDeque<(Instant, DateTime<Utc>)>,
}

pub enum Admit {
    SendAt(Instant),
    Resting(DateTime<Utc>),
    Limited(DateTime<Utc>),
}

impl Limits {
    pub fn new(base_rest: Duration, settings: LimitSettings) -> Self {
        Self {
            base_rest,
            settings,
            burst_credit: settings.refill * (settings.burst - 1),
            consecutive_blocks: 0,
            rest: None,
            last_state: None,
            pace_cursor: None,
            daily: VecDeque::new(),
        }
    }

    pub fn admit(&mut self, now: Instant, wall: DateTime<Utc>) -> Admit {
        if let Some(ready_at) = self.active_rest(now) {
            self.last_state = Some(EngineState::Blocked);
            return Admit::Resting(ready_at);
        }
        self.daily.drain(..self.expired_count(now));
        if let Some(ready_at) = self.daily_deadline(now) {
            self.last_state = Some(EngineState::Limited);
            return Admit::Limited(ready_at);
        }
        let backlog = self.pace_backlog(now);
        if let Some(ready_at) = self.pace_deadline(backlog) {
            self.last_state = Some(EngineState::Limited);
            return Admit::Limited(ready_at);
        }

        // Represent the bucket as refill time owed. Burst credit lets requests consume it early.
        let next = backlog + self.settings.refill;
        self.pace_cursor = Some((now + next, wall + time_delta(next)));
        self.daily.push_back((now, wall));
        Admit::SendAt(now + backlog.saturating_sub(self.burst_credit))
    }

    pub fn confirm_send(
        &mut self,
        now: Instant,
        reservation: (Instant, DateTime<Utc>),
    ) -> Option<DateTime<Utc>> {
        let ready_at = self.active_rest(now)?;
        self.release(reservation);
        self.last_state = Some(EngineState::Blocked);
        Some(ready_at)
    }

    pub fn release(&mut self, reservation: (Instant, DateTime<Utc>)) {
        // A delayed task can wake after another request has already reserved a newer slot.
        if let Some(index) = self.daily.iter().rposition(|entry| *entry == reservation) {
            self.daily.remove(index);
        }
    }

    pub fn peek(&self, now: Instant) -> Option<DateTime<Utc>> {
        self.active_rest(now)
            .or_else(|| self.daily_deadline(now))
            .or_else(|| self.pace_deadline(self.pace_backlog(now)))
    }

    pub fn daily_count(&self, now: Instant) -> u32 {
        (self.daily.len() - self.expired_count(now)) as u32
    }

    pub fn last_state(&self) -> Option<EngineState> {
        self.last_state
    }

    fn active_rest(&self, now: Instant) -> Option<DateTime<Utc>> {
        self.rest
            .filter(|(deadline, _)| now < *deadline)
            .map(|(_, ready_at)| ready_at)
    }

    fn expired_count(&self, now: Instant) -> usize {
        self.daily
            .partition_point(|(at, _)| now.duration_since(*at) >= DAILY_WINDOW)
    }

    fn daily_deadline(&self, now: Instant) -> Option<DateTime<Utc>> {
        if self.daily.len() < self.settings.daily as usize {
            return None;
        }
        let &(at, wall) = self.daily.front()?;
        (now.duration_since(at) < DAILY_WINDOW).then(|| wall + time_delta(DAILY_WINDOW))
    }

    fn pace_backlog(&self, now: Instant) -> Duration {
        self.pace_cursor
            .map_or(Duration::ZERO, |(at, _)| at.saturating_duration_since(now))
    }

    fn pace_deadline(&self, backlog: Duration) -> Option<DateTime<Utc>> {
        let (_, wall) = self.pace_cursor?;
        let wait = backlog.saturating_sub(self.burst_credit);
        (wait > self.settings.refill.min(MAX_PACE_WAIT))
            .then(|| wall - time_delta(self.burst_credit))
    }

    pub fn record(
        &mut self,
        state: EngineState,
        now: Instant,
        wall: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        self.last_state = Some(state);
        match state {
            EngineState::Blocked => {
                self.consecutive_blocks = self.consecutive_blocks.saturating_add(1);
                let multiplier = 2_u32.saturating_pow(self.consecutive_blocks - 1);
                let duration = self.base_rest.saturating_mul(multiplier).min(MAX_REST);
                let ready_at = wall + time_delta(duration);
                self.rest = Some((now + duration, ready_at));
                Some(ready_at)
            }
            EngineState::Ok => {
                self.consecutive_blocks = 0;
                self.rest = None;
                None
            }
            EngineState::Empty
            | EngineState::Limited
            | EngineState::ParseFailed
            | EngineState::Error => None,
        }
    }
}

fn time_delta(duration: Duration) -> TimeDelta {
    TimeDelta::from_std(duration).expect("engine limit duration fits in a TimeDelta")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{advance, pause};

    const SHORT_REFILL: LimitSettings = LimitSettings {
        burst: 5,
        refill: Duration::from_secs(30),
        daily: 200,
    };

    #[test]
    fn releasing_an_unsent_request_refunds_its_daily_reservation() {
        let settings = LimitSettings {
            burst: 1,
            ..LimitSettings::default()
        };
        let mut limits = Limits::new(Duration::from_secs(600), settings);
        let now = Instant::now();
        let wall = Utc::now();
        assert!(matches!(limits.admit(now, wall), Admit::SendAt(at) if at == now));
        assert_eq!(limits.daily_count(now), 1);
        limits.release((now, wall));
        assert_eq!(limits.daily_count(now), 0);
        assert!(matches!(limits.admit(now, wall), Admit::Limited(_)));
    }

    #[tokio::test]
    async fn only_one_caller_can_reserve_the_next_pace_slot() {
        pause();
        let mut limits = Limits::new(Duration::from_secs(600), SHORT_REFILL);
        let now = Instant::now();
        let wall = DateTime::from_timestamp(1_800_000_000, 999_500_000).unwrap();
        for _ in 0..5 {
            assert!(matches!(limits.admit(now, wall), Admit::SendAt(at) if at == now));
        }
        assert_eq!(limits.peek(now), None);
        assert!(matches!(
            limits.admit(now, wall),
            Admit::SendAt(at) if at == now + Duration::from_secs(30)
        ));
        let ready_at = wall + TimeDelta::seconds(60);
        assert_eq!(limits.peek(now), Some(ready_at));
        assert!(matches!(limits.admit(now, wall), Admit::Limited(at) if at == ready_at));
        // A small change in the offset between clock samples can cross a wall-clock second.
        let later = now + Duration::from_millis(1);
        let later_wall = wall + TimeDelta::milliseconds(2);
        assert_eq!(limits.peek(later), Some(ready_at));
        assert!(matches!(limits.admit(later, later_wall), Admit::Limited(at) if at == ready_at));
        assert_eq!(limits.daily_count(later), 6);

        advance(Duration::from_secs(30)).await;
        assert_eq!(limits.peek(Instant::now()), None);
        assert_eq!(limits.confirm_send(Instant::now(), (now, wall)), None);
    }

    #[test]
    fn pace_wait_is_capped_at_thirty_seconds_with_a_longer_refill() {
        let settings = LimitSettings {
            burst: 1,
            refill: Duration::from_secs(60),
            ..LimitSettings::default()
        };
        let mut limits = Limits::new(Duration::from_secs(600), settings);
        let now = Instant::now();
        let wall = Utc::now();
        let ready_at = wall + TimeDelta::seconds(60);
        assert!(matches!(limits.admit(now, wall), Admit::SendAt(at) if at == now));

        let before = now + Duration::from_secs(29);
        assert_eq!(limits.peek(before), Some(ready_at));
        assert!(matches!(
            limits.admit(before, wall + TimeDelta::seconds(29)),
            Admit::Limited(at) if at == ready_at
        ));

        let boundary = now + Duration::from_secs(30);
        assert_eq!(limits.peek(boundary), None);
        assert!(matches!(
            limits.admit(boundary, wall + TimeDelta::seconds(30)),
            Admit::SendAt(at) if at == now + Duration::from_secs(60)
        ));
    }

    #[tokio::test]
    async fn rolling_daily_limit_expires_from_the_oldest_request() {
        pause();
        let mut limits = Limits::new(Duration::from_secs(600), SHORT_REFILL);
        let start = Instant::now();
        let wall = Utc::now();
        for count in 0..200 {
            let now = Instant::now();
            let wall = wall + TimeDelta::from_std(now - start).unwrap();
            assert!(matches!(limits.admit(now, wall), Admit::SendAt(at) if at == now));
            assert_eq!(limits.daily_count(now), count + 1);
            advance(Duration::from_secs(30)).await;
        }

        let ready_at = wall + TimeDelta::hours(24);
        let just_before_expiry = start + Duration::from_secs(24 * 3600 - 1);
        advance(just_before_expiry - Instant::now()).await;
        let now = Instant::now();
        assert_eq!(limits.peek(now), Some(ready_at));
        assert_eq!(limits.last_state(), None);
        assert!(matches!(
            limits.admit(now, ready_at - TimeDelta::seconds(1)),
            Admit::Limited(at) if at == ready_at
        ));
        assert_eq!(limits.last_state(), Some(EngineState::Limited));
        assert_eq!(limits.daily_count(now), 200);

        advance(Duration::from_secs(1)).await;
        let now = Instant::now();
        assert_eq!(limits.peek(now), None);
        assert_eq!(limits.daily_count(now), 199);
        assert!(matches!(limits.admit(now, ready_at), Admit::SendAt(at) if at == now));
        assert_eq!(limits.daily_count(now), 200);
        assert_eq!(limits.peek(now), Some(ready_at + TimeDelta::seconds(30)));
    }

    #[tokio::test]
    async fn refund_removes_the_delayed_reservation_even_after_a_newer_admission() {
        pause();
        let mut limits = Limits::new(Duration::from_secs(600), SHORT_REFILL);
        let now = Instant::now();
        let wall = Utc::now();
        for _ in 0..5 {
            assert!(matches!(limits.admit(now, wall), Admit::SendAt(at) if at == now));
        }

        advance(Duration::from_secs(1)).await;
        let reservation = (Instant::now(), wall + TimeDelta::seconds(1));
        assert!(matches!(limits.admit(reservation.0, reservation.1),
            Admit::SendAt(at) if at == now + Duration::from_secs(30)));
        advance(Duration::from_secs(29)).await;
        let later = Instant::now();
        let later_wall = wall + TimeDelta::seconds(30);
        assert!(matches!(limits.admit(later, later_wall), Admit::SendAt(_)));
        let rest = limits.record(EngineState::Blocked, later, later_wall);
        assert!(rest.is_some());
        assert_eq!(limits.confirm_send(later, reservation), rest);
        assert_eq!(limits.daily_count(later), 6);

        advance(Duration::from_secs(24 * 3600 - 30)).await;
        assert_eq!(limits.daily_count(Instant::now()), 1);
        advance(Duration::from_secs(1)).await;
        assert_eq!(limits.daily_count(Instant::now()), 1);
    }

    #[tokio::test]
    async fn consecutive_blocks_double_rest_up_to_four_hours() {
        pause();
        let mut limits = Limits::new(Duration::from_secs(3600), LimitSettings::default());
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
                Admit::SendAt(_)
            ));
            wall = ready_at;
        }
    }

    #[tokio::test]
    async fn ok_clears_rest_and_resets_the_block_count() {
        pause();
        let mut limits = Limits::new(Duration::from_secs(10), LimitSettings::default());
        let wall = Utc::now();
        limits.record(EngineState::Blocked, Instant::now(), wall);
        advance(Duration::from_secs(10)).await;
        let wall = wall + TimeDelta::seconds(10);
        limits.record(EngineState::Blocked, Instant::now(), wall);

        assert_eq!(limits.record(EngineState::Ok, Instant::now(), wall), None);
        assert!(matches!(
            limits.admit(Instant::now(), wall),
            Admit::SendAt(_)
        ));
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
            let mut limits = Limits::new(Duration::from_secs(10), LimitSettings::default());
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
                matches!(
                    limits.admit(Instant::now(), first_deadline),
                    Admit::SendAt(_)
                ),
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
