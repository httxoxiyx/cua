//! Pure ownership and settlement rules for one cross-RPC foreground batch.
//!
//! The platform registry serializes access to `Segment` and owns the OS episode,
//! input-release ledger and restoration proof. Owners come exclusively from the
//! trusted runtime/transport context, never request arguments. IDs are opaque,
//! freshly minted and deduplicated by that registry; this module does not mint
//! capabilities, inspect the desktop, synchronize threads or perform cleanup.
//!
//! A call reservation outlives the async response waiter: settle it only after
//! every worker that can touch native input has actually exited. Cancellation or
//! owner destruction must call `revoke` before waiting for those workers. Neither
//! dropping a reservation nor a successful action response proves OS cleanup.

use std::sync::Arc;

use crate::background_input::ExactWindowTarget;
use crate::foreground_activity::{EpisodeLease, Snapshot, MAX_EPISODE_MS};

pub const MAX_SEGMENT_CALLS: u32 = 64;
pub const MAX_SEGMENT_STEPS: u32 = 20;
pub const MAX_SEGMENT_ID_BYTES: usize = 128;
pub const MAX_OWNER_COMPONENT_BYTES: usize = 1_024;

/// Server-derived identity. A segment freezes a copy; none of its components
/// may be replaced by an identically named session on a different transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub runtime_scope: String,
    pub session_id: String,
    pub transport_session_id: String,
}

/// Exact immutable binding stored at begin. The ID alone is not authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub owner: Owner,
    pub target: ExactWindowTarget,
    pub id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub duration_ms: u64,
    pub max_calls: u32,
    pub max_steps: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            duration_ms: MAX_EPISODE_MS,
            max_calls: MAX_SEGMENT_CALLS,
            max_steps: MAX_SEGMENT_STEPS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Open,
    Closing,
    Revoked,
    CleanupUnknown,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallKind {
    /// Optional platform-internal first call, never a public mutation step.
    /// Begin itself need not activate the target.
    Activation,
    Observation,
    Mutation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleanup {
    /// The platform proved worker settlement, release of owned input and the
    /// appropriate exact-window restoration (or safe non-reclamation).
    Confirmed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    InvalidBinding,
    InvalidLimits,
    ActivityUnavailable,
    OwnerMismatch,
    TargetMismatch,
    IdMismatch,
    NotOpen(State),
    Busy,
    LeaseRevoked,
    CallLimit,
    StepLimit,
    ActivationAlreadyAttempted,
    ReservationMismatch,
    NotClosing,
    CleanupUnconfirmed,
}

/// Unique in-memory ticket. It is neither cloneable nor constructible by a
/// caller. A dropped ticket deliberately leaves the segment occupied: dropping
/// an async waiter must never admit another action while its worker may run.
#[derive(Debug)]
#[must_use = "settle only after all native workers for this reservation have exited"]
pub struct Reservation {
    instance: Arc<()>,
    sequence: u32,
}

/// No Clone implementation: ownership, counters and revocation cannot fork.
#[derive(Debug)]
pub struct Segment {
    binding: Binding,
    lease: EpisodeLease,
    deadline_ms: u64,
    limits: Limits,
    state: State,
    instance: Arc<()>,
    in_flight: Option<u32>,
    calls_reserved: u32,
    steps_reserved: u32,
}

impl Segment {
    pub fn begin(
        binding: Binding,
        now_ms: u64,
        snapshot: Snapshot,
        limits: Limits,
    ) -> Result<Self, Refusal> {
        let owner = &binding.owner;
        if [
            &owner.runtime_scope,
            &owner.session_id,
            &owner.transport_session_id,
        ]
        .into_iter()
        .any(|part| {
            part.is_empty()
                || part.len() > MAX_OWNER_COMPONENT_BYTES
                || part.chars().any(char::is_control)
        }) || binding.target.pid <= 0
            || binding.target.window_id == 0
            || binding.id.is_empty()
            || binding.id.len() > MAX_SEGMENT_ID_BYTES
            || !binding
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
        {
            return Err(Refusal::InvalidBinding);
        }
        if !(1..=MAX_EPISODE_MS).contains(&limits.duration_ms)
            || !(1..=MAX_SEGMENT_CALLS).contains(&limits.max_calls)
            || !(1..=MAX_SEGMENT_STEPS).contains(&limits.max_steps)
        {
            return Err(Refusal::InvalidLimits);
        }
        let deadline_ms = now_ms
            .checked_add(limits.duration_ms)
            .ok_or(Refusal::InvalidLimits)?;
        let lease = EpisodeLease::begin(now_ms, snapshot).ok_or(Refusal::ActivityUnavailable)?;
        Ok(Self {
            binding,
            lease,
            deadline_ms,
            limits,
            state: State::Open,
            instance: Arc::new(()),
            in_flight: None,
            calls_reserved: 0,
            steps_reserved: 0,
        })
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    pub fn target(&self) -> ExactWindowTarget {
        self.binding.target
    }

    /// The original lease, not newly acquired idle evidence. The platform must
    /// also check segment revocation/ownership before every native write.
    pub fn activity_lease(&self) -> EpisodeLease {
        self.lease
    }

    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    pub fn calls_reserved(&self) -> u32 {
        self.calls_reserved
    }

    pub fn steps_reserved(&self) -> u32 {
        self.steps_reserved
    }

    fn matches(&self, binding: &Binding) -> Result<(), Refusal> {
        if binding.owner != self.binding.owner {
            return Err(Refusal::OwnerMismatch);
        }
        if binding.target != self.binding.target {
            return Err(Refusal::TargetMismatch);
        }
        if binding.id != self.binding.id {
            return Err(Refusal::IdMismatch);
        }
        Ok(())
    }

    /// Recheck an already reserved call before a native write, or before
    /// reservation. A fresh idle period never revives a revoked lease. An
    /// unrelated caller mismatch denies that caller without closing this owner.
    pub fn check(
        &mut self,
        binding: &Binding,
        now_ms: u64,
        snapshot: Snapshot,
    ) -> Result<(), Refusal> {
        self.matches(binding)?;
        if self.state != State::Open {
            return Err(Refusal::NotOpen(self.state));
        }
        if now_ms >= self.deadline_ms || !self.lease.permits(now_ms, snapshot) {
            self.state = State::Revoked;
            return Err(Refusal::LeaseRevoked);
        }
        Ok(())
    }

    pub fn reserve(
        &mut self,
        binding: &Binding,
        now_ms: u64,
        snapshot: Snapshot,
        kind: CallKind,
    ) -> Result<Reservation, Refusal> {
        self.check(binding, now_ms, snapshot)?;
        if self.in_flight() {
            return Err(Refusal::Busy);
        }
        if kind == CallKind::Activation && self.calls_reserved != 0 {
            return Err(Refusal::ActivationAlreadyAttempted);
        }
        if self.calls_reserved >= self.limits.max_calls {
            self.state = State::Closing;
            return Err(Refusal::CallLimit);
        }
        if kind == CallKind::Mutation && self.steps_reserved >= self.limits.max_steps {
            self.state = State::Closing;
            return Err(Refusal::StepLimit);
        }
        self.calls_reserved += 1;
        self.steps_reserved += u32::from(kind == CallKind::Mutation);
        self.in_flight = Some(self.calls_reserved);
        Ok(Reservation {
            instance: Arc::clone(&self.instance),
            sequence: self.calls_reserved,
        })
    }

    /// This does not prove cleanup, restore focus or reopen admission. It also
    /// works after revoke/close so the final worker can release its reservation.
    pub fn settle(&mut self, reservation: Reservation) -> Result<(), Refusal> {
        if !Arc::ptr_eq(&reservation.instance, &self.instance)
            || self.in_flight != Some(reservation.sequence)
        {
            return Err(Refusal::ReservationMismatch);
        }
        self.in_flight = None;
        Ok(())
    }

    /// Explicit end stops admission immediately, including while a worker runs.
    pub fn request_close(&mut self, binding: &Binding) -> Result<(), Refusal> {
        self.matches(binding)?;
        match self.state {
            State::Open => self.state = State::Closing,
            State::Closing | State::Revoked => {}
            State::CleanupUnknown => return Err(Refusal::CleanupUnconfirmed),
            State::Closed => return Err(Refusal::NotOpen(State::Closed)),
        }
        Ok(())
    }

    /// Trusted owner cancellation/destruction, activity interruption or native
    /// failure. Call before dropping a response waiter; never clear in-flight.
    pub fn revoke(&mut self) {
        if matches!(self.state, State::Open | State::Closing) {
            self.state = State::Revoked;
        }
    }

    pub fn cleanup_allowed(&self) -> bool {
        !self.in_flight() && matches!(self.state, State::Closing | State::Revoked)
    }

    pub fn finish_cleanup(&mut self, cleanup: Cleanup) -> Result<(), Refusal> {
        if self.in_flight() {
            return Err(Refusal::Busy);
        }
        if self.state == State::CleanupUnknown {
            return Err(Refusal::CleanupUnconfirmed);
        }
        if !self.cleanup_allowed() {
            return Err(Refusal::NotClosing);
        }
        self.state = match cleanup {
            Cleanup::Confirmed => State::Closed,
            Cleanup::Unknown => State::CleanupUnknown,
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foreground_activity::State as ActivityState;

    const START: u64 = 5_000;

    fn binding() -> Binding {
        Binding {
            owner: Owner {
                runtime_scope: "runtime-a".into(),
                session_id: "session-a".into(),
                transport_session_id: "transport-a".into(),
            },
            target: ExactWindowTarget {
                pid: 42,
                window_id: 7,
            },
            id: "platform-minted-unique-id".into(),
        }
    }

    fn idle() -> Snapshot {
        Snapshot {
            reliable: true,
            state: ActivityState::Idle,
            idle_ms: 5_000,
            generation: 9,
        }
    }

    fn segment() -> Segment {
        Segment::begin(binding(), START, idle(), Limits::default()).unwrap()
    }

    fn reserve(segment: &mut Segment, kind: CallKind) -> Result<Reservation, Refusal> {
        segment.reserve(&binding(), START, idle(), kind)
    }

    #[test]
    fn invalid_or_unbounded_binding_cannot_begin() {
        let mut cases = Vec::new();
        for owner_field in 0..3 {
            let mut bad = binding();
            match owner_field {
                0 => bad.owner.runtime_scope.clear(),
                1 => bad.owner.session_id.clear(),
                _ => bad.owner.transport_session_id.clear(),
            }
            cases.push(bad);
        }
        for id in [
            String::new(),
            "x".repeat(MAX_SEGMENT_ID_BYTES + 1),
            "bad\nid".into(),
        ] {
            let mut bad = binding();
            bad.id = id;
            cases.push(bad);
        }
        let mut bad = binding();
        bad.target.pid = 0;
        cases.push(bad);
        let mut bad = binding();
        bad.target.window_id = 0;
        cases.push(bad);
        let mut bad = binding();
        bad.owner.session_id = "x".repeat(MAX_OWNER_COMPONENT_BYTES + 1);
        cases.push(bad);
        for bad in cases {
            assert_eq!(
                Segment::begin(bad, START, idle(), Limits::default()).unwrap_err(),
                Refusal::InvalidBinding
            );
        }
    }

    #[test]
    fn limits_and_deadline_arithmetic_are_bounded() {
        for limits in [
            Limits {
                duration_ms: 0,
                ..Limits::default()
            },
            Limits {
                duration_ms: MAX_EPISODE_MS + 1,
                ..Limits::default()
            },
            Limits {
                max_calls: 0,
                ..Limits::default()
            },
            Limits {
                max_calls: MAX_SEGMENT_CALLS + 1,
                ..Limits::default()
            },
            Limits {
                max_steps: 0,
                ..Limits::default()
            },
            Limits {
                max_steps: MAX_SEGMENT_STEPS + 1,
                ..Limits::default()
            },
        ] {
            assert_eq!(
                Segment::begin(binding(), START, idle(), limits).unwrap_err(),
                Refusal::InvalidLimits
            );
        }
        assert_eq!(
            Segment::begin(binding(), u64::MAX, idle(), Limits::default()).unwrap_err(),
            Refusal::InvalidLimits
        );
    }

    #[test]
    fn begin_requires_continuous_healthy_idle() {
        for snapshot in [
            Snapshot {
                reliable: false,
                ..idle()
            },
            Snapshot {
                state: ActivityState::Active,
                ..idle()
            },
            Snapshot {
                idle_ms: 4_999,
                ..idle()
            },
        ] {
            assert_eq!(
                Segment::begin(binding(), START, snapshot, Limits::default()).unwrap_err(),
                Refusal::ActivityUnavailable
            );
        }
    }

    #[test]
    fn begin_freezes_owner_target_id_and_original_lease() {
        let mut original = binding();
        let segment = Segment::begin(original.clone(), START, idle(), Limits::default()).unwrap();
        original.owner.session_id = "different".into();
        original.target.window_id += 1;
        original.id.clear();
        assert_eq!(segment.binding(), &binding());
        assert_eq!(segment.target(), binding().target);
        assert_eq!(segment.deadline_ms(), START + MAX_EPISODE_MS);
        assert!(segment.activity_lease().permits(START, idle()));
        assert!(!segment.activity_lease().permits(
            START,
            Snapshot {
                generation: 10,
                ..idle()
            }
        ));
    }

    #[test]
    fn every_binding_component_is_required_without_disturbing_other_owner() {
        for field in 0..6 {
            let mut segment = segment();
            let mut wrong = binding();
            let expected = match field {
                0 => {
                    wrong.owner.runtime_scope.push('x');
                    Refusal::OwnerMismatch
                }
                1 => {
                    wrong.owner.session_id.push('x');
                    Refusal::OwnerMismatch
                }
                2 => {
                    wrong.owner.transport_session_id.push('x');
                    Refusal::OwnerMismatch
                }
                3 => {
                    wrong.target.pid += 1;
                    Refusal::TargetMismatch
                }
                4 => {
                    wrong.target.window_id += 1;
                    Refusal::TargetMismatch
                }
                _ => {
                    wrong.id.push('x');
                    Refusal::IdMismatch
                }
            };
            assert_eq!(
                segment
                    .reserve(&wrong, START, idle(), CallKind::Mutation)
                    .unwrap_err(),
                expected
            );
            assert_eq!(segment.request_close(&wrong), Err(expected));
            assert_eq!(segment.state(), State::Open);
            assert_eq!(segment.calls_reserved(), 0);
            let call = reserve(&mut segment, CallKind::Mutation).unwrap();
            segment.settle(call).unwrap();
        }
    }

    #[test]
    fn reservation_prevents_overlap_including_observation_and_reentrancy() {
        let mut segment = segment();
        let first = reserve(&mut segment, CallKind::Mutation).unwrap();
        for kind in [
            CallKind::Activation,
            CallKind::Observation,
            CallKind::Mutation,
        ] {
            assert_eq!(reserve(&mut segment, kind).unwrap_err(), Refusal::Busy);
        }
        assert_eq!((segment.calls_reserved(), segment.steps_reserved()), (1, 1));
        segment.settle(first).unwrap();
        let second = reserve(&mut segment, CallKind::Observation).unwrap();
        assert_eq!((segment.calls_reserved(), segment.steps_reserved()), (2, 1));
        segment.settle(second).unwrap();
    }

    #[test]
    fn optional_activation_is_first_call_and_not_a_mutation_step() {
        let mut segment = segment();
        let call = reserve(&mut segment, CallKind::Activation).unwrap();
        assert_eq!((segment.calls_reserved(), segment.steps_reserved()), (1, 0));
        segment.settle(call).unwrap();
        assert_eq!(
            reserve(&mut segment, CallKind::Activation).unwrap_err(),
            Refusal::ActivationAlreadyAttempted
        );
        let call = reserve(&mut segment, CallKind::Mutation).unwrap();
        segment.settle(call).unwrap();
    }

    #[test]
    fn mutation_budget_allows_final_observation_but_never_an_extra_step() {
        let mut segment = segment();
        for _ in 0..MAX_SEGMENT_STEPS {
            let call = reserve(&mut segment, CallKind::Mutation).unwrap();
            segment.settle(call).unwrap();
        }
        let final_observation = reserve(&mut segment, CallKind::Observation).unwrap();
        segment.settle(final_observation).unwrap();
        assert_eq!(
            reserve(&mut segment, CallKind::Mutation).unwrap_err(),
            Refusal::StepLimit
        );
        assert_eq!(segment.state(), State::Closing);
        assert_eq!(segment.steps_reserved(), MAX_SEGMENT_STEPS);
    }

    #[test]
    fn observations_cannot_extend_call_budget() {
        let mut segment = segment();
        for _ in 0..MAX_SEGMENT_CALLS {
            let call = reserve(&mut segment, CallKind::Observation).unwrap();
            segment.settle(call).unwrap();
        }
        assert_eq!(
            reserve(&mut segment, CallKind::Mutation).unwrap_err(),
            Refusal::CallLimit
        );
        assert_eq!(
            (segment.calls_reserved(), segment.steps_reserved()),
            (MAX_SEGMENT_CALLS, 0)
        );
        assert!(segment.cleanup_allowed());
    }

    #[test]
    fn deadline_is_fixed_across_calls_and_new_idle_evidence() {
        let mut segment = segment();
        let original_deadline = segment.deadline_ms();
        let call = segment
            .reserve(
                &binding(),
                original_deadline - 1,
                idle(),
                CallKind::Observation,
            )
            .unwrap();
        segment.settle(call).unwrap();
        assert_eq!(segment.deadline_ms(), original_deadline);
        assert_eq!(
            segment.check(&binding(), original_deadline, idle()),
            Err(Refusal::LeaseRevoked)
        );
        assert_eq!(segment.state(), State::Revoked);
        assert_eq!(
            segment.check(&binding(), START, idle()),
            Err(Refusal::NotOpen(State::Revoked))
        );
    }

    #[test]
    fn shorter_deadline_is_enforced_without_renewing_activity_lease() {
        let mut segment = Segment::begin(
            binding(),
            START,
            idle(),
            Limits {
                duration_ms: 10,
                ..Limits::default()
            },
        )
        .unwrap();
        assert_eq!(segment.check(&binding(), START + 9, idle()), Ok(()));
        assert_eq!(
            segment.check(&binding(), START + 10, idle()),
            Err(Refusal::LeaseRevoked)
        );
    }

    #[test]
    fn activity_generation_change_never_revives_after_later_idle() {
        let mut segment = segment();
        let call = reserve(&mut segment, CallKind::Mutation).unwrap();
        let later_idle = Snapshot {
            generation: 10,
            idle_ms: 50_000,
            ..idle()
        };
        assert_eq!(
            segment.check(&binding(), START + 1, later_idle),
            Err(Refusal::LeaseRevoked)
        );
        segment.settle(call).unwrap();
        assert_eq!(
            segment
                .reserve(&binding(), START + 2, later_idle, CallKind::Mutation)
                .unwrap_err(),
            Refusal::NotOpen(State::Revoked)
        );
        assert!(segment.cleanup_allowed());
    }

    #[test]
    fn unreliable_active_or_backward_time_evidence_irreversibly_revokes() {
        for (now, snapshot) in [
            (
                START,
                Snapshot {
                    reliable: false,
                    ..idle()
                },
            ),
            (
                START,
                Snapshot {
                    state: ActivityState::Active,
                    ..idle()
                },
            ),
            (
                START,
                Snapshot {
                    idle_ms: 4_999,
                    ..idle()
                },
            ),
            (START - 1, idle()),
        ] {
            let mut segment = segment();
            assert_eq!(
                segment.check(&binding(), now, snapshot),
                Err(Refusal::LeaseRevoked)
            );
            assert_eq!(
                segment.check(&binding(), START, idle()),
                Err(Refusal::NotOpen(State::Revoked))
            );
        }
    }

    #[test]
    fn explicit_end_closes_admission_before_in_flight_worker_settles() {
        let mut segment = segment();
        let call = reserve(&mut segment, CallKind::Mutation).unwrap();
        segment.request_close(&binding()).unwrap();
        assert_eq!(segment.state(), State::Closing);
        assert!(segment.in_flight());
        assert_eq!(
            segment.finish_cleanup(Cleanup::Confirmed),
            Err(Refusal::Busy)
        );
        assert_eq!(
            reserve(&mut segment, CallKind::Mutation).unwrap_err(),
            Refusal::NotOpen(State::Closing)
        );
        segment.settle(call).unwrap();
        assert!(segment.cleanup_allowed());
        segment.finish_cleanup(Cleanup::Confirmed).unwrap();
        assert_eq!(segment.state(), State::Closed);
    }

    #[test]
    fn cancellation_or_owner_destruction_revokes_before_last_worker_settlement() {
        let mut segment = segment();
        let call = reserve(&mut segment, CallKind::Mutation).unwrap();
        segment.revoke();
        segment.request_close(&binding()).unwrap();
        assert_eq!(segment.state(), State::Revoked);
        assert_eq!(segment.finish_cleanup(Cleanup::Unknown), Err(Refusal::Busy));
        assert_eq!(
            reserve(&mut segment, CallKind::Mutation).unwrap_err(),
            Refusal::NotOpen(State::Revoked)
        );
        segment.settle(call).unwrap();
        segment.finish_cleanup(Cleanup::Confirmed).unwrap();
        assert_eq!(segment.state(), State::Closed);
    }

    #[test]
    fn even_same_textual_id_cannot_settle_a_different_instance() {
        let mut first = segment();
        let mut second = segment();
        let first_call = reserve(&mut first, CallKind::Mutation).unwrap();
        let second_call = reserve(&mut second, CallKind::Mutation).unwrap();
        assert_eq!(first.settle(second_call), Err(Refusal::ReservationMismatch));
        assert!(first.in_flight());
        assert!(second.in_flight());
        first.settle(first_call).unwrap();
        assert!(!first.in_flight());
    }

    #[test]
    fn dropping_ticket_is_not_worker_settlement_or_cleanup_proof() {
        let mut segment = segment();
        drop(reserve(&mut segment, CallKind::Mutation).unwrap());
        assert!(segment.in_flight());
        assert_eq!(
            reserve(&mut segment, CallKind::Mutation).unwrap_err(),
            Refusal::Busy
        );
        segment.revoke();
        assert_eq!(
            segment.finish_cleanup(Cleanup::Confirmed),
            Err(Refusal::Busy)
        );
    }

    #[test]
    fn cleanup_unknown_is_sticky_and_cannot_be_reopened_or_upgraded() {
        let mut segment = segment();
        segment.request_close(&binding()).unwrap();
        segment.finish_cleanup(Cleanup::Unknown).unwrap();
        assert_eq!(segment.state(), State::CleanupUnknown);
        segment.revoke();
        assert_eq!(segment.state(), State::CleanupUnknown);
        assert_eq!(
            segment.finish_cleanup(Cleanup::Confirmed),
            Err(Refusal::CleanupUnconfirmed)
        );
        assert_eq!(
            segment.request_close(&binding()),
            Err(Refusal::CleanupUnconfirmed)
        );
        assert_eq!(
            reserve(&mut segment, CallKind::Mutation).unwrap_err(),
            Refusal::NotOpen(State::CleanupUnknown)
        );
    }

    #[test]
    fn action_settlement_alone_does_not_prove_cleanup() {
        let mut segment = segment();
        let call = reserve(&mut segment, CallKind::Mutation).unwrap();
        segment.settle(call).unwrap();
        assert!(!segment.cleanup_allowed());
        assert_eq!(
            segment.finish_cleanup(Cleanup::Confirmed),
            Err(Refusal::NotClosing)
        );
        assert_eq!(segment.state(), State::Open);
    }

    #[test]
    fn closed_summary_remains_available_without_any_revival() {
        let mut segment = segment();
        let call = reserve(&mut segment, CallKind::Mutation).unwrap();
        segment.settle(call).unwrap();
        segment.request_close(&binding()).unwrap();
        segment.finish_cleanup(Cleanup::Confirmed).unwrap();
        segment.revoke();
        assert_eq!(segment.state(), State::Closed);
        assert_eq!(segment.binding(), &binding());
        assert_eq!((segment.calls_reserved(), segment.steps_reserved()), (1, 1));
        assert_eq!(
            reserve(&mut segment, CallKind::Mutation).unwrap_err(),
            Refusal::NotOpen(State::Closed)
        );
        assert_eq!(
            segment.finish_cleanup(Cleanup::Confirmed),
            Err(Refusal::NotClosing)
        );
    }
}
