//! Content-free activity evidence for bounded foreground input.
//!
//! The platform adapter must report continuous coverage and distinguish only
//! input it can actually attribute. Unknown events revoke an episode just as
//! human input does, but are never labelled as human activity.

pub const IDLE_REQUIRED_MS: u64 = 5_000;
pub const MAX_HEARTBEAT_GAP_MS: u64 = 250;
pub const MAX_EPISODE_MS: u64 = 120_000;

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

/// Immutable evidence for one synchronous foreground operation. Fresh idle
/// evidence after an interruption cannot revive an earlier operation.
#[derive(Clone, Copy, Debug)]
pub struct EpisodeLease {
    generation: u64,
    started_ms: u64,
}

impl EpisodeLease {
    pub fn begin(now_ms: u64, snapshot: Snapshot) -> Option<Self> {
        (snapshot.reliable && snapshot.state == State::Idle && snapshot.idle_ms >= IDLE_REQUIRED_MS)
            .then_some(Self {
                generation: snapshot.generation,
                started_ms: now_ms,
            })
    }

    pub fn permits(&self, now_ms: u64, snapshot: Snapshot) -> bool {
        now_ms >= self.started_ms
            && now_ms - self.started_ms < MAX_EPISODE_MS
            && snapshot.reliable
            && snapshot.state == State::Idle
            && snapshot.idle_ms >= IDLE_REQUIRED_MS
            && snapshot.generation == self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputControl {
    Key(u16),
    Mouse(u8),
}

/// Only controls whose down transition belongs to this operation may be
/// released by its cleanup. Store a preallocated release before posting down;
/// teardown must neither allocate native events nor release unrelated input.
pub struct PressedInputs<R> {
    entries: Vec<(InputControl, R)>,
}

impl<R> Default for PressedInputs<R> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<R> PressedInputs<R> {
    pub fn press(&mut self, control: InputControl, release: R) -> Result<(), R> {
        if self.entries.iter().any(|(held, _)| *held == control) {
            return Err(release);
        }
        self.entries.push((control, release));
        Ok(())
    }

    pub fn update_release(&mut self, control: InputControl, release: R) {
        if let Some((_, current)) = self.entries.iter_mut().find(|(held, _)| *held == control) {
            *current = release;
        }
    }

    pub fn release(&mut self, control: InputControl) -> Option<R> {
        let index = self.entries.iter().position(|(held, _)| *held == control)?;
        Some(self.entries.remove(index).1)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn drain_reversed(&mut self) -> impl Iterator<Item = R> + '_ {
        self.entries.drain(..).rev().map(|(_, release)| release)
    }
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

    #[test]
    fn foreground_episode_never_revives_or_extends_its_deadline() {
        let mut activity = Activity::default();
        healthy_through(&mut activity, 0, 5_000);
        let lease = EpisodeLease::begin(5_000, activity.snapshot(5_000)).unwrap();
        assert!(lease.permits(5_000, activity.snapshot(5_000)));
        assert!(!lease.permits(4_999, activity.snapshot(5_000)));
        healthy_through(&mut activity, 5_100, 125_000);
        assert!(!lease.permits(125_000, activity.snapshot(125_000)));
        activity.event(125_000, Source::Unknown);
        healthy_through(&mut activity, 125_100, 130_000);
        assert!(EpisodeLease::begin(130_000, activity.snapshot(130_000)).is_some());
        assert!(!lease.permits(130_000, activity.snapshot(130_000)));
    }

    #[test]
    fn foreground_episode_requires_reliable_idle_not_just_elapsed_time() {
        for (reliable, state, idle_ms) in [
            (false, State::Idle, 5_000),
            (true, State::Active, 5_000),
            (true, State::Unknown, 5_000),
            (true, State::Idle, 4_999),
        ] {
            assert!(EpisodeLease::begin(
                5_000,
                Snapshot {
                    reliable,
                    state,
                    idle_ms,
                    generation: 0
                }
            )
            .is_none());
        }
    }

    #[test]
    fn cleanup_releases_only_posted_controls_once_in_reverse_order() {
        let mut held = PressedInputs::default();
        assert!(held.release(InputControl::Key(55)).is_none());
        held.press(InputControl::Key(55), "cmd-up").unwrap();
        held.press(InputControl::Key(56), "shift-up").unwrap();
        held.press(InputControl::Key(36), "return-up").unwrap();
        assert!(held.press(InputControl::Key(36), "duplicate-up").is_err());
        assert_eq!(held.release(InputControl::Key(36)), Some("return-up"));
        assert_eq!(held.release(InputControl::Key(36)), None);
        assert_eq!(
            held.drain_reversed().collect::<Vec<_>>(),
            ["shift-up", "cmd-up"]
        );
        assert!(held.is_empty());
    }

    #[test]
    fn interrupted_drag_cleanup_uses_last_dispatched_point() {
        let mut held = PressedInputs::default();
        held.press(InputControl::Mouse(0), (1, 2)).unwrap();
        held.update_release(InputControl::Mouse(0), (3, 4));
        held.update_release(InputControl::Mouse(1), (99, 99));
        assert_eq!(held.drain_reversed().collect::<Vec<_>>(), [(3, 4)]);
    }
}
