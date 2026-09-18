//! Keep an already-visible preview stable during a scoped temporary activation.
//!
//! Platform adapters supply a monotonic time and foreground observations. Ending
//! the scope restores normal visibility rules immediately; the time limit only
//! bounds a leaked scope and does not delay any visibility transition.

/// Maximum lifetime of a temporary activation hold, as a leak safeguard.
pub const MAX_HOLD_MS: u64 = 30_000;

/// The logical application and exact physical window receiving an action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivationTarget {
    pub app_pid: i64,
    pub pid: i64,
    pub window_id: u64,
}

#[derive(Clone)]
struct Hold {
    token: u64,
    target: ActivationTarget,
    previous_frontmost_pid: i64,
    started_ms: u64,
}

impl Hold {
    fn is_live_at(&self, now_ms: u64) -> bool {
        now_ms
            .checked_sub(self.started_ms)
            .is_some_and(|elapsed| elapsed < MAX_HOLD_MS)
    }
}

/// Tracks independent activation scopes without consulting platform state.
#[derive(Clone, Default)]
pub struct TemporaryActivationTracker {
    last_token: u64,
    holds: Vec<Hold>,
}

impl TemporaryActivationTracker {
    /// Begin a hold only for an already-visible, currently background target.
    /// Tokens are never reused, including after cancellation.
    pub fn begin(
        &mut self,
        target: ActivationTarget,
        previous_frontmost_pid: Option<i64>,
        was_visible: bool,
        now_ms: u64,
    ) -> Option<u64> {
        let previous_frontmost_pid = previous_frontmost_pid?;
        if !was_visible
            || target.app_pid <= 0
            || target.pid <= 0
            || target.window_id == 0
            || previous_frontmost_pid <= 0
            || previous_frontmost_pid == target.app_pid
            || previous_frontmost_pid == target.pid
        {
            return None;
        }

        let token = self.last_token.checked_add(1)?;
        self.last_token = token;
        self.holds.retain(|hold| hold.is_live_at(now_ms));
        self.holds.push(Hold {
            token,
            target,
            previous_frontmost_pid,
            started_ms: now_ms,
        });
        Some(token)
    }

    /// End exactly one scope. Other nested scopes retain their own lifetimes.
    pub fn end(&mut self, token: u64) -> bool {
        let Some(index) = self.holds.iter().position(|hold| hold.token == token) else {
            return false;
        };
        self.holds.swap_remove(index);
        true
    }

    /// Permanently revoke expired scopes or scopes interrupted by another app.
    /// An unknown foreground is insufficient evidence to retain a hold.
    pub fn observe_foreground(&mut self, current: Option<i64>, now_ms: u64) {
        self.holds.retain(|hold| {
            hold.is_live_at(now_ms)
                && current.is_some_and(|pid| {
                    pid == hold.previous_frontmost_pid
                        || pid == hold.target.app_pid
                        || pid == hold.target.pid
                })
        });
    }

    /// Whether a live scope preserves this exact target's existing visibility.
    pub fn keeps_visible(&self, target: ActivationTarget, now_ms: u64) -> bool {
        self.holds
            .iter()
            .any(|hold| hold.target == target && hold.is_live_at(now_ms))
    }

    /// Original foreground apps that should remain hidden during live scopes.
    pub fn original_foreground_pids(&self, now_ms: u64) -> Vec<i64> {
        let mut pids: Vec<_> = self
            .holds
            .iter()
            .filter(|hold| hold.is_live_at(now_ms))
            .map(|hold| hold.previous_frontmost_pid)
            .collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    /// Discard every scope, for example when the preview is disabled or replaced.
    pub fn cancel_all(&mut self) -> bool {
        let changed = !self.holds.is_empty();
        self.holds.clear();
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: ActivationTarget = ActivationTarget {
        app_pid: 20,
        pid: 21,
        window_id: 100,
    };
    const PREVIOUS: i64 = 10;

    #[test]
    fn activation_and_restore_keep_visibility_only_until_scope_end() {
        let mut tracker = TemporaryActivationTracker::default();
        let token = tracker.begin(TARGET, Some(PREVIOUS), true, 0).unwrap();

        for (now_ms, pid) in [PREVIOUS, TARGET.app_pid, TARGET.pid, PREVIOUS]
            .into_iter()
            .enumerate()
        {
            tracker.observe_foreground(Some(pid), now_ms as u64);
            assert!(tracker.keeps_visible(TARGET, now_ms as u64));
        }

        assert!(tracker.end(token));
        assert!(!tracker.keeps_visible(TARGET, 3));
        assert!(!tracker.end(token));
    }

    #[test]
    fn hidden_unknown_and_already_foreground_targets_cannot_start_a_hold() {
        let mut tracker = TemporaryActivationTracker::default();
        for (previous, visible) in [
            (None, true),
            (Some(0), true),
            (Some(-1), true),
            (Some(TARGET.app_pid), true),
            (Some(TARGET.pid), true),
            (Some(PREVIOUS), false),
        ] {
            assert_eq!(tracker.begin(TARGET, previous, visible, 0), None);
            assert!(!tracker.keeps_visible(TARGET, 0));
        }
    }

    #[test]
    fn invalid_target_identity_cannot_start_a_hold() {
        let mut tracker = TemporaryActivationTracker::default();
        for target in [
            ActivationTarget {
                app_pid: 0,
                ..TARGET
            },
            ActivationTarget { pid: -1, ..TARGET },
            ActivationTarget {
                window_id: 0,
                ..TARGET
            },
        ] {
            assert_eq!(tracker.begin(target, Some(PREVIOUS), true, 0), None);
            assert!(!tracker.keeps_visible(target, 0));
        }
    }

    #[test]
    fn a_hold_does_not_follow_another_app_process_or_window() {
        let mut tracker = TemporaryActivationTracker::default();
        tracker.begin(TARGET, Some(PREVIOUS), true, 0).unwrap();
        for other in [
            ActivationTarget {
                app_pid: 30,
                ..TARGET
            },
            ActivationTarget { pid: 31, ..TARGET },
            ActivationTarget {
                window_id: 101,
                ..TARGET
            },
        ] {
            assert!(!tracker.keeps_visible(other, 1));
        }
        assert!(tracker.keeps_visible(TARGET, 1));
    }

    #[test]
    fn third_party_or_unknown_foreground_permanently_revokes_a_hold() {
        for takeover in [Some(30), None] {
            let mut tracker = TemporaryActivationTracker::default();
            let token = tracker.begin(TARGET, Some(PREVIOUS), true, 0).unwrap();
            tracker.observe_foreground(Some(TARGET.pid), 1);
            tracker.observe_foreground(takeover, 2);
            assert!(!tracker.keeps_visible(TARGET, 2));

            for pid in [PREVIOUS, TARGET.app_pid, TARGET.pid] {
                tracker.observe_foreground(Some(pid), 3);
                assert!(!tracker.keeps_visible(TARGET, 3));
            }
            assert!(!tracker.end(token));
        }
    }

    #[test]
    fn leaked_hold_expires_without_waiting_for_scope_end() {
        let mut tracker = TemporaryActivationTracker::default();
        let token = tracker.begin(TARGET, Some(PREVIOUS), true, 7).unwrap();
        assert!(tracker.keeps_visible(TARGET, 7 + MAX_HOLD_MS - 1));
        assert!(!tracker.keeps_visible(TARGET, 7 + MAX_HOLD_MS));

        tracker.observe_foreground(Some(TARGET.pid), 7 + MAX_HOLD_MS);
        tracker.observe_foreground(Some(PREVIOUS), 8 + MAX_HOLD_MS);
        assert!(!tracker.keeps_visible(TARGET, 8 + MAX_HOLD_MS));
        assert!(!tracker.end(token));
    }

    #[test]
    fn nested_scopes_can_end_in_either_order() {
        for reverse in [false, true] {
            let mut tracker = TemporaryActivationTracker::default();
            let first = tracker.begin(TARGET, Some(PREVIOUS), true, 0).unwrap();
            let second = tracker.begin(TARGET, Some(PREVIOUS), true, 1).unwrap();
            assert!(second > first);
            let (ended, remaining) = if reverse {
                (second, first)
            } else {
                (first, second)
            };

            assert!(!tracker.end(second + 1));
            assert!(tracker.end(ended));
            assert!(!tracker.end(ended));
            assert!(tracker.keeps_visible(TARGET, 2));
            assert!(tracker.end(remaining));
            assert!(!tracker.keeps_visible(TARGET, 2));
        }
    }

    #[test]
    fn nested_scopes_expire_independently() {
        let mut tracker = TemporaryActivationTracker::default();
        let first = tracker.begin(TARGET, Some(PREVIOUS), true, 0).unwrap();
        let second = tracker.begin(TARGET, Some(PREVIOUS), true, 1).unwrap();

        tracker.observe_foreground(Some(TARGET.app_pid), MAX_HOLD_MS);
        assert!(!tracker.end(first));
        assert!(tracker.keeps_visible(TARGET, MAX_HOLD_MS));
        assert!(tracker.end(second));
        assert!(!tracker.keeps_visible(TARGET, MAX_HOLD_MS));
    }

    #[test]
    fn cancellation_clears_every_target_without_reusing_tokens() {
        let mut tracker = TemporaryActivationTracker::default();
        let other = ActivationTarget {
            window_id: 101,
            ..TARGET
        };
        let first = tracker.begin(TARGET, Some(PREVIOUS), true, 0).unwrap();
        let second = tracker.begin(other, Some(PREVIOUS), true, 0).unwrap();
        assert!(tracker.cancel_all());
        assert!(!tracker.cancel_all());
        assert!(!tracker.keeps_visible(TARGET, 0));
        assert!(!tracker.keeps_visible(other, 0));

        let fresh = tracker.begin(TARGET, Some(PREVIOUS), true, 1).unwrap();
        assert!(fresh > second);
        assert!(!tracker.end(first));
        assert!(!tracker.end(second));
        assert!(tracker.keeps_visible(TARGET, 1));
        assert!(tracker.end(fresh));
    }

    #[test]
    fn original_foregrounds_deduplicate_nested_scopes() {
        let mut tracker = TemporaryActivationTracker::default();
        let other = tracker.begin(TARGET, Some(30), true, 0).unwrap();
        let first = tracker.begin(TARGET, Some(PREVIOUS), true, 0).unwrap();
        let second = tracker.begin(TARGET, Some(PREVIOUS), true, 1).unwrap();
        assert_eq!(tracker.original_foreground_pids(1), vec![PREVIOUS, 30]);

        assert!(tracker.end(first));
        assert_eq!(tracker.original_foreground_pids(1), vec![PREVIOUS, 30]);
        assert!(tracker.end(second));
        assert_eq!(tracker.original_foreground_pids(1), vec![30]);
        assert!(tracker.end(other));
        assert!(tracker.original_foreground_pids(1).is_empty());
    }

    #[test]
    fn original_foregrounds_disappear_when_scopes_expire_end_cancel_or_revoke() {
        let mut tracker = TemporaryActivationTracker::default();
        tracker.begin(TARGET, Some(PREVIOUS), true, 0).unwrap();
        assert_eq!(
            tracker.original_foreground_pids(MAX_HOLD_MS - 1),
            vec![PREVIOUS]
        );
        assert!(tracker.original_foreground_pids(MAX_HOLD_MS).is_empty());

        let token = tracker
            .begin(TARGET, Some(PREVIOUS), true, MAX_HOLD_MS)
            .unwrap();
        assert!(tracker.end(token));
        assert!(tracker.original_foreground_pids(MAX_HOLD_MS).is_empty());

        tracker
            .begin(TARGET, Some(PREVIOUS), true, MAX_HOLD_MS)
            .unwrap();
        assert!(tracker.cancel_all());
        assert!(tracker.original_foreground_pids(MAX_HOLD_MS).is_empty());

        for takeover in [Some(30), None] {
            tracker
                .begin(TARGET, Some(PREVIOUS), true, MAX_HOLD_MS)
                .unwrap();
            tracker.observe_foreground(takeover, MAX_HOLD_MS);
            tracker.observe_foreground(Some(PREVIOUS), MAX_HOLD_MS);
            assert!(tracker.original_foreground_pids(MAX_HOLD_MS).is_empty());
        }
    }
}
