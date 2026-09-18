//! Content-free activity evidence for bounded foreground input.
//!
//! The platform adapter must report continuous coverage and distinguish only
//! input it can actually attribute. Unknown events revoke an episode just as
//! human input does, but are never labelled as human activity.

pub const IDLE_REQUIRED_MS: u64 = 5_000;
pub const MAX_HEARTBEAT_GAP_MS: u64 = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    OwnGenerated,
    Human,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Unknown,
    Active,
    Idle,
}

#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub reliable: bool,
    pub state: State,
    pub idle_ms: u64,
    pub generation: u64,
}

#[derive(Debug, Default)]
pub struct Activity {
    generation: u64,
    coverage_since: Option<u64>,
    last_heartbeat: Option<u64>,
    last_external: Option<(u64, Source)>,
}

impl Activity {
    pub fn health(&mut self, now_ms: u64, reliable: bool) {
        let gap = self
            .last_heartbeat
            .is_some_and(|last| now_ms < last || now_ms - last > MAX_HEARTBEAT_GAP_MS);
        if !reliable || gap {
            self.invalidate();
        }
        self.last_heartbeat = reliable.then_some(now_ms);
        if reliable && self.coverage_since.is_none() {
            self.coverage_since = Some(now_ms);
        }
    }

    pub fn event(&mut self, now_ms: u64, source: Source) {
        if source != Source::OwnGenerated {
            self.generation = self.generation.wrapping_add(1);
            self.last_external = Some((now_ms, source));
        }
    }

    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.coverage_since = None;
        self.last_heartbeat = None;
    }

    pub fn snapshot(&self, now_ms: u64) -> Snapshot {
        let unknown = Snapshot {
            reliable: false,
            state: State::Unknown,
            idle_ms: 0,
            generation: self.generation,
        };
        let (Some(since), Some(last)) = (self.coverage_since, self.last_heartbeat) else {
            return unknown;
        };
        if now_ms < last || now_ms - last > MAX_HEARTBEAT_GAP_MS || now_ms < since {
            return unknown;
        }
        let quiet_since = self.last_external.map_or(since, |(at, _)| at.max(since));
        let idle_ms = now_ms.saturating_sub(quiet_since);
        let state = if idle_ms >= IDLE_REQUIRED_MS {
            State::Idle
        } else if self
            .last_external
            .is_some_and(|(at, source)| at >= since && source == Source::Human)
        {
            State::Active
        } else {
            State::Unknown
        };
        Snapshot {
            reliable: true,
            state,
            idle_ms,
            generation: self.generation,
        }
    }

    pub fn permits(&self, now_ms: u64, generation: u64) -> bool {
        let current = self.snapshot(now_ms);
        current.state == State::Idle && current.generation == generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_through(activity: &mut Activity, start: u64, end: u64) {
        for time in (start..=end).step_by(100) {
            activity.health(time, true);
        }
    }

    #[test]
    fn startup_requires_continuous_five_seconds() {
        let mut activity = Activity::default();
        assert_eq!(activity.snapshot(50_000).state, State::Unknown);
        healthy_through(&mut activity, 0, 4_900);
        assert_ne!(activity.snapshot(4_999).state, State::Idle);
        activity.health(5_000, true);
        assert_eq!(activity.snapshot(5_000).state, State::Idle);
    }

    #[test]
    fn own_events_do_not_mask_external_input_or_reset_idle() {
        let mut activity = Activity::default();
        healthy_through(&mut activity, 0, 5_000);
        let generation = activity.snapshot(5_000).generation;
        activity.event(5_000, Source::OwnGenerated);
        assert!(activity.permits(5_000, generation));
        activity.event(5_000, Source::Human);
        assert_eq!(activity.snapshot(5_000).state, State::Active);
        assert!(!activity.permits(5_000, generation));
        healthy_through(&mut activity, 5_100, 10_000);
        assert_eq!(activity.snapshot(10_000).state, State::Idle);
        assert!(
            !activity.permits(10_000, generation),
            "revoked episodes never revive"
        );
    }

    #[test]
    fn unknown_source_is_not_reported_as_human() {
        let mut activity = Activity::default();
        healthy_through(&mut activity, 0, 5_000);
        let generation = activity.snapshot(5_000).generation;
        activity.event(5_000, Source::Unknown);
        assert_eq!(activity.snapshot(5_000).state, State::Unknown);
        assert!(!activity.permits(5_000, generation));
    }

    #[test]
    fn coverage_gap_revokes_old_episode_after_recovery() {
        let mut activity = Activity::default();
        healthy_through(&mut activity, 0, 5_000);
        let generation = activity.snapshot(5_000).generation;
        assert!(!activity.permits(5_251, generation));
        healthy_through(&mut activity, 5_300, 10_300);
        assert_eq!(activity.snapshot(10_300).state, State::Idle);
        assert!(!activity.permits(10_300, generation));
    }

    #[test]
    fn permission_secure_input_and_session_loss_reset_coverage() {
        let mut activity = Activity::default();
        healthy_through(&mut activity, 0, 5_000);
        activity.health(5_000, false);
        assert_eq!(activity.snapshot(5_000).state, State::Unknown);
        activity.health(5_001, true);
        assert_ne!(activity.snapshot(10_000).state, State::Idle);
    }
}
