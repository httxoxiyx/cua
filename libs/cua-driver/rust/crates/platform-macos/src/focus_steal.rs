//! Layer-3 focus-steal preventer — Rust port of Swift's
//! `SystemFocusStealPreventer.swift` plus the PR #1521 4-layer hardening
//! (closure scope, RAII lease, 5s monotonic deadline, 1s janitor).
//!
//! ## What this protects against
//!
//! `NSWorkspace.OpenConfiguration.activates = false` tells LaunchServices
//! "don't activate the target on launch". LaunchServices honors that.
//! What it does NOT do is stop the launched app from calling
//! `NSApp.activate(ignoringOtherApps:)` in its own
//! `applicationDidFinishLaunching`. Chrome, Electron, Safari, Calculator
//! all do exactly that — so a "background" launch flashes the target on
//! top of the user's work for a few frames.
//!
//! The preventer subscribes to
//! `NSWorkspace.didActivateApplicationNotification` and, when an activation
//! matches a registered suppression entry, submits an exact prior-window
//! restore only while native activity coverage and generation remain valid.
//! Human/unknown input or a monitoring gap invalidates that restoration lease.
//!
//! ## Layered design (matches PR #1521)
//!
//! 1. **Closure API** — `with_suppression(target, restore_to, origin, f)`
//!    begins an entry, awaits `f`, ends the entry. Use this when the
//!    suppression scope is a single async block.
//! 2. **RAII API** — `begin_suppression(target, restore_to, origin)`
//!    returns a `SuppressionLease`. `Drop` ends the entry synchronously
//!    (no awaiting). Use this when the caller needs to hold the lease
//!    across multiple branches or when async cancellation may interrupt
//!    the closure path.
//! 3. **5s monotonic deadline** — every entry stamps an
//!    `Instant::now() + 5s`. The observer prunes expired entries before
//!    matching, so a leaked lease can't cause a stale entry to keep
//!    re-activating the prior frontmost app forever.
//! 4. **1s janitor** — a tokio interval task wakes up every second
//!    while the dispatcher is non-empty, prunes expired entries, and
//!    stops when the map drains. Re-starts when the next entry is
//!    added. Coordinated via `tokio::sync::watch`.
//!
//! ## Singleton
//!
//! `FocusStealPreventer::shared()` returns a process-wide
//! `Arc<FocusStealPreventer>`. The observer registration happens inside
//! `OnceLock::get_or_init`, so it's safe to call from any thread without
//! racing on observer install.
//!
//! ## Why a fresh background `NSOperationQueue` (not `mainQueue`)
//!
//! NSWorkspace's block-based observer fires on the queue you give it. If
//! the queue is `nil` (Swift default) or `mainQueue`, the block runs on
//! the main thread — which means it requires a live main run loop.
//! `cua-driver call` (one-shot subcommand) and `--no-overlay` mode don't
//! have one, so the activation observer would never fire. A fresh
//! background `NSOperationQueue` sidesteps that — the block runs on the
//! queue's own thread regardless of run-loop state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2_app_kit::{
    NSWorkspace, NSWorkspaceApplicationKey, NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::NSOperationQueue;
use uuid::Uuid;

/// Per-entry deadline. After this much wall-clock time the dispatcher's
/// observer (and the janitor) treats the entry as leaked and prunes it
/// without firing. Mirrors Swift PR #1521.
const ENTRY_DEADLINE: Duration = Duration::from_secs(5);

/// Janitor tick interval. The task wakes up this often while the
/// dispatcher is non-empty and prunes expired entries.
const JANITOR_TICK: Duration = Duration::from_secs(1);

/// Identifier for a suppression. `with_suppression` and `begin_suppression`
/// hand one of these back; `end_suppression` consumes it.
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct SuppressionHandle(Uuid);

/// Dispatcher-internal entry shape.
#[derive(Debug)]
struct Entry {
    activity_restore: Option<crate::foreground_activity::RestoreEvidence>,
    /// `Some(pid)` matches only that pid's activations. `None` is a
    /// wildcard — matches any activation whose pid != `restore_to`.
    /// The wildcard variant is used while a launch is in flight and the
    /// real pid isn't known yet.
    target_pid: Option<i32>,
    /// One intentional activation that a wildcard entry must allow through.
    ///
    /// Raw background pixel clicks use the focus-without-raise recipe: the
    /// target must become AppKit-active long enough for its event queue to
    /// accept the click, while activations of every *other* app should still
    /// be suppressed as cross-app side effects.
    allowed_pid: Option<i32>,
    /// Pid to restore focus to when an activation matches this entry.
    restore_to: i32,
    /// Monotonic deadline. After this, the entry is pruned without
    /// firing.
    deadline: Instant,
    /// An ordinary background action has returned, but this target-only
    /// protection tail remains until its original bounded settle deadline.
    returned_tail: bool,
    /// Provenance for tracing — e.g. `"LaunchAppTool.pre"`.
    #[allow(dead_code)]
    origin: &'static str,
}

/// Keep the authorizing entry identity until the restore is actually submitted.
#[derive(Clone, Copy, Debug)]
struct RestoreCandidate {
    handle: SuppressionHandle,
    activated_pid: i32,
    restore_to: i32,
}

/// Singleton focus-steal preventer.
///
/// Constructed lazily on first `shared()` call. Owns the dispatcher state
/// (Sync via the inner Mutex); the NSWorkspace observer + queue are
/// intentionally retained-and-forgotten on install so their lifetime is
/// the whole process and we don't need to thread `!Send` Cocoa handles
/// through this struct.
pub struct FocusStealPreventer {
    dispatcher: Arc<Dispatcher>,
}

impl FocusStealPreventer {
    /// Return (or initialize on first call) the process-wide singleton.
    pub fn shared() -> Arc<Self> {
        static SINGLETON: OnceLock<Arc<FocusStealPreventer>> = OnceLock::new();
        SINGLETON
            .get_or_init(|| {
                let dispatcher = Arc::new(Dispatcher::new());
                install_observer(&dispatcher);
                Arc::new(FocusStealPreventer { dispatcher })
            })
            .clone()
    }

    /// Begin suppressing focus-steals targeting `target_pid` (or any pid
    /// when `None`, the wildcard). Returns a `SuppressionLease` whose
    /// `Drop` ends the entry synchronously.
    ///
    /// `restore_to` is the pid the preventer re-activates if it matches
    /// a notification. `origin` is a static label for tracing.
    pub fn begin_suppression(
        target_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
    ) -> SuppressionLease {
        let shared = Self::shared();
        let handle = shared.dispatcher.add(target_pid, restore_to, origin);
        shared
            .dispatcher
            .attach_activity_restore(handle, restore_to);
        SuppressionLease {
            handle,
            dispatcher: Arc::clone(&shared.dispatcher),
            released: false,
        }
    }

    /// Begin wildcard suppression while allowing one intentional activation.
    ///
    /// This is narrower than disabling suppression altogether: activation of
    /// `allowed_pid` is ignored, but any other pid still restores
    /// `restore_to`.
    pub fn begin_suppression_allowing(
        allowed_pid: i32,
        restore_to: i32,
        origin: &'static str,
    ) -> SuppressionLease {
        let shared = Self::shared();
        let handle = shared
            .dispatcher
            .add_allowing(allowed_pid, restore_to, origin);
        shared
            .dispatcher
            .attach_activity_restore(handle, restore_to);
        SuppressionLease {
            handle,
            dispatcher: Arc::clone(&shared.dispatcher),
            released: false,
        }
    }

    /// Run `f` with a suppression entry active. Equivalent to
    /// `begin_suppression(...)` + run `f` + drop the lease — but expressed
    /// as a single async call site.
    pub async fn with_suppression<R, F, Fut>(
        target_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
        f: F,
    ) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let _lease = Self::begin_suppression(target_pid, restore_to, origin);
        f().await
    }
}

/// Begin-suppression convenience that bounces through the singleton.
pub fn begin_suppression(
    target_pid: Option<i32>,
    restore_to: i32,
    origin: &'static str,
) -> SuppressionLease {
    FocusStealPreventer::begin_suppression(target_pid, restore_to, origin)
}

/// Begin wildcard suppression while permitting `allowed_pid` to activate.
pub fn begin_suppression_allowing(
    allowed_pid: i32,
    restore_to: i32,
    origin: &'static str,
) -> SuppressionLease {
    FocusStealPreventer::begin_suppression_allowing(allowed_pid, restore_to, origin)
}

/// End only returned background tails for an intentionally foregrounded target.
/// Active actions, wildcard launch protection, and other targets are unchanged.
pub fn cancel_deferred_suppression(target_pid: i32) {
    FocusStealPreventer::shared()
        .dispatcher
        .cancel_deferred(target_pid);
}

/// RAII lease. `Drop` ends the entry synchronously, so the entry is
/// removed even if a future is cancelled mid-await.
pub struct SuppressionLease {
    handle: SuppressionHandle,
    dispatcher: Arc<Dispatcher>,
    released: bool,
}

impl SuppressionLease {
    /// Explicit release. Useful if the caller wants to drop the lease
    /// before its scope ends without taking the `Drop` path.
    pub fn release(mut self) {
        self.dispatcher.remove(self.handle);
        self.released = true;
    }

    /// Keep a returned action's target-only protection off its response path.
    /// The original five-second cap still applies. No runtime, a non-targeted
    /// entry, or an already elapsed deadline falls back to normal immediate
    /// Drop. Runtime shutdown also drops the timer-owned lease.
    pub fn defer_release(self, deadline: Instant) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Some(deadline) = self.dispatcher.mark_deferred(self.handle, deadline) else {
            return;
        };
        runtime.spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            drop(self);
        });
    }
}

impl Drop for SuppressionLease {
    fn drop(&mut self) {
        if !self.released {
            self.dispatcher.remove(self.handle);
        }
    }
}

// ── Dispatcher ──────────────────────────────────────────────────────────────

/// Holds the suppression entries plus the janitor lifecycle.
///
/// `entries` is a `HashMap<Uuid, Entry>` so add/remove are O(1) by handle.
/// Lookups by `(target_pid, restore_to)` during a notification are O(N) —
/// N is at most a handful of in-flight launches at a time so a linear
/// scan is fine.
pub(crate) struct Dispatcher {
    entries: Mutex<HashMap<Uuid, Entry>>,
    /// `true` while the janitor task should keep running. The janitor
    /// loop watches for transitions to detect when to start/stop.
    janitor_active: tokio::sync::watch::Sender<bool>,
    janitor_started: Mutex<bool>,
}

impl Dispatcher {
    fn attach_activity_restore(&self, handle: SuppressionHandle, pid: i32) {
        let evidence = crate::foreground_activity::capture_restore(pid);
        if let Some(entry) = self.entries.lock().unwrap().get_mut(&handle.0) {
            entry.activity_restore = evidence;
        }
    }

    fn new() -> Self {
        let (tx, _rx) = tokio::sync::watch::channel(false);
        Self {
            entries: Mutex::new(HashMap::new()),
            janitor_active: tx,
            janitor_started: Mutex::new(false),
        }
    }

    /// Add an entry, return its handle. Always attempts to start the
    /// janitor task — `kick_janitor()` is idempotent and is the only
    /// reliable path to recover if the very first add happened before
    /// a tokio runtime was ready. Gating the kick on "map was empty"
    /// (as we used to) lost the janitor permanently in that case:
    /// subsequent adds would skip the kick and the janitor never
    /// started, leaving deadline-reaping entirely up to the
    /// `snapshot_matches` reap fallback (only fires on an activation).
    fn add(
        self: &Arc<Self>,
        target_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
    ) -> SuppressionHandle {
        self.add_entry(target_pid, None, restore_to, origin)
    }

    /// Add a wildcard entry that ignores one intentional target activation.
    fn add_allowing(
        self: &Arc<Self>,
        allowed_pid: i32,
        restore_to: i32,
        origin: &'static str,
    ) -> SuppressionHandle {
        self.add_entry(None, Some(allowed_pid), restore_to, origin)
    }

    fn add_entry(
        self: &Arc<Self>,
        target_pid: Option<i32>,
        allowed_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
    ) -> SuppressionHandle {
        let id = Uuid::new_v4();
        let entry = Entry {
            activity_restore: None,
            target_pid,
            allowed_pid,
            restore_to,
            deadline: Instant::now() + ENTRY_DEADLINE,
            returned_tail: false,
            origin,
        };
        {
            let mut guard = self.entries.lock().unwrap();
            guard.insert(id, entry);
        }
        // Always kick — idempotent if the task is already running.
        self.kick_janitor();
        // Signal the janitor that there's work to do (it will start a
        // fresh tokio interval on the next tick).
        let _ = self.janitor_active.send(true);
        SuppressionHandle(id)
    }

    /// Remove an entry. When the map drains to empty, signals the janitor
    /// to stop until the next add.
    fn remove(&self, handle: SuppressionHandle) {
        let now_empty = {
            let mut guard = self.entries.lock().unwrap();
            guard.remove(&handle.0);
            guard.is_empty()
        };
        if now_empty {
            let _ = self.janitor_active.send(false);
        }
    }

    fn mark_deferred(&self, handle: SuppressionHandle, deadline: Instant) -> Option<Instant> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(&handle.0)?;
        let deadline = deadline.min(entry.deadline);
        if entry.target_pid.is_none() || deadline <= Instant::now() {
            return None;
        }
        entry.returned_tail = true;
        entry.deadline = deadline;
        Some(deadline)
    }

    fn cancel_deferred(&self, target_pid: i32) {
        let now_empty = {
            let mut entries = self.entries.lock().unwrap();
            entries
                .retain(|_, entry| !(entry.returned_tail && entry.target_pid == Some(target_pid)));
            entries.is_empty()
        };
        if now_empty {
            let _ = self.janitor_active.send(false);
        }
    }

    /// Snapshot the entries (cloned to a small Vec) — used by tests
    /// and the activation handler to evaluate matches without holding
    /// the lock across the restore call.
    fn snapshot_restore_candidates(
        &self,
        activated_pid: i32,
        current_front: Option<i32>,
    ) -> Vec<RestoreCandidate> {
        let mut guard = self.entries.lock().unwrap();
        // Reap expired entries first — keeps the dispatcher honest even
        // if the janitor hasn't ticked yet.
        let now = Instant::now();
        guard.retain(|_, e| e.deadline > now);
        if current_front != Some(activated_pid) {
            // NSWorkspace notifications can arrive after a newer activation.
            // A stale notification is not evidence for either adoption or
            // restoration, so leave every lease unchanged.
            return Vec::new();
        }
        let mut candidates = Vec::new();
        for (id, entry) in guard.iter_mut() {
            if entry.allowed_pid == Some(activated_pid) {
                continue;
            }
            match entry.target_pid {
                Some(target_pid)
                    if target_pid == activated_pid && entry.restore_to != activated_pid =>
                {
                    candidates.push(RestoreCandidate {
                        handle: SuppressionHandle(*id),
                        activated_pid,
                        restore_to: entry.restore_to,
                    });
                }
                Some(target_pid) if target_pid == activated_pid => {}
                Some(_) if activated_pid != entry.restore_to => {
                    // The lease covers only its target. An unrelated
                    // activation is therefore user/external state that must be
                    // preserved. Track it as the new restore destination so a
                    // later reflex activation of the target cannot pull the
                    // user back to the app that happened to be frontmost when
                    // the action started.
                    entry.restore_to = activated_pid;
                }
                Some(_) => {}
                // Wildcard: match any activation except the restore_to pid
                // (don't fight ourselves when we re-activate the prior
                // frontmost).
                None if activated_pid != entry.restore_to => {
                    candidates.push(RestoreCandidate {
                        handle: SuppressionHandle(*id),
                        activated_pid,
                        restore_to: entry.restore_to,
                    });
                }
                None => {}
            }
        }
        candidates
    }

    #[cfg(test)]
    fn snapshot_matches(&self, activated_pid: i32, current_front: Option<i32>) -> Vec<i32> {
        self.snapshot_restore_candidates(activated_pid, current_front)
            .into_iter()
            .map(|candidate| candidate.restore_to)
            .collect()
    }

    fn submit_restore_if_current(
        &self,
        candidate: RestoreCandidate,
        current_front: impl FnOnce() -> Option<i32>,
        submit: impl FnOnce(i32),
    ) -> bool {
        // Serialize the final restore submission with cancellation. A candidate
        // may have been queued before a foreground action cancelled its tail,
        // before expiration, or before a newer user activation changed the
        // restore destination. None of those stale candidates may act.
        let entries = self.entries.lock().unwrap();
        let Some(entry) = entries.get(&candidate.handle.0) else {
            return false;
        };
        if entry.deadline <= Instant::now()
            || entry.restore_to != candidate.restore_to
            || entry.restore_to == candidate.activated_pid
            || entry.allowed_pid == Some(candidate.activated_pid)
            || entry
                .target_pid
                .is_some_and(|pid| pid != candidate.activated_pid)
            || !should_restore_after_activation(current_front(), candidate.activated_pid)
        {
            return false;
        }
        // The production closure only submits NSRunningApplication activation;
        // it must not wait for activation or re-enter this dispatcher. Our
        // observer is on its own serial NSOperationQueue, so a resulting
        // notification is queued rather than synchronously re-entering here.
        submit(candidate.restore_to);
        true
    }

    /// Number of entries (for tests).
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Reap entries whose deadline is past. Returns the number reaped.
    fn reap_expired(&self) -> usize {
        let mut guard = self.entries.lock().unwrap();
        let now = Instant::now();
        let before = guard.len();
        guard.retain(|_, e| e.deadline > now);
        let after = guard.len();
        let reaped = before - after;
        if after == 0 && reaped > 0 {
            let _ = self.janitor_active.send(false);
        }
        reaped
    }

    /// Start the janitor task on the current tokio runtime (idempotent).
    ///
    /// Safe to call from `add()` on every entry — returns immediately
    /// when the task is already up. If the spawn cannot proceed (no
    /// tokio runtime available — e.g. the first `add()` raced the
    /// binary's runtime init), the `started` flag is intentionally left
    /// `false` so the next add from a tokio-aware caller retries.
    /// Without that retry path the janitor could go permanently
    /// un-spawned and deadline-reaping would degrade to the
    /// `snapshot_matches` reap fallback (only runs on an activation).
    fn kick_janitor(self: &Arc<Self>) {
        let mut started = self.janitor_started.lock().unwrap();
        if *started {
            return;
        }
        // If there's no tokio runtime available (e.g. the binary is in
        // the middle of an init path that runs before Tokio is up), skip
        // — the next add from a tokio-aware caller will retry. We do NOT
        // set `*started = true` in this branch so the retry actually
        // takes the spawn path.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        // Mark started ONLY after a successful `tokio::spawn`. The
        // spawn itself is infallible under the current API but we still
        // sequence the flag update after the spawn so any future
        // panic-from-spawn path would leave `started = false` and the
        // next add would retry.
        let weak = Arc::downgrade(self);
        let mut rx = self.janitor_active.subscribe();
        tokio::spawn(async move {
            loop {
                // Block until the dispatcher is non-empty.
                if !*rx.borrow_and_update() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                    continue;
                }
                let mut tick = tokio::time::interval(JANITOR_TICK);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                tick.tick().await; // immediate first tick
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            let Some(d) = weak.upgrade() else { return };
                            let _ = d.reap_expired();
                            if d.len() == 0 {
                                // Map drained — break to outer select, wait
                                // for next add.
                                break;
                            }
                        }
                        ch = rx.changed() => {
                            if ch.is_err() { return; }
                            // Active flag may have flipped; loop top will
                            // re-check via `borrow_and_update`.
                            break;
                        }
                    }
                }
            }
        });
        *started = true;
    }
}

// ── Observer registration ────────────────────────────────────────────────────

/// Register the NSWorkspace.didActivateApplicationNotification observer.
///
/// The returned token and the queue are intentionally `mem::forget`-leaked
/// — the singleton is process-lifetime, so we never tear the observer
/// down. Forgetting avoids having to thread `!Send` `Retained<...>`
/// handles through `FocusStealPreventer` (which lives in `Arc<...>` /
/// `OnceLock<...>` and therefore needs to be `Send + Sync`).
fn install_observer(dispatcher: &Arc<Dispatcher>) {
    use block2::RcBlock;
    use objc2_foundation::NSNotification;
    use std::ptr::NonNull;

    let ws = unsafe { NSWorkspace::sharedWorkspace() };
    let center = unsafe { ws.notificationCenter() };

    // Fresh background NSOperationQueue. Critical: with `nil` queue,
    // AppKit delivers synchronously on the posting thread (typically main);
    // with `mainQueue`, the block requires a running main run loop. A
    // fresh queue runs the block on a private background thread no matter
    // what run loop the binary has up.
    let queue = unsafe { NSOperationQueue::new() };
    // setMaxConcurrentOperationCount: 1 means activations are processed
    // serially — they're cheap so contention isn't a worry, but serial
    // processing keeps the restore order deterministic if two come in
    // back to back.
    unsafe { queue.setMaxConcurrentOperationCount(1) };

    let dispatcher_clone = Arc::clone(dispatcher);
    let block = RcBlock::new(move |note_ptr: NonNull<NSNotification>| {
        // SAFETY: AppKit gives us a borrowed NSNotification for the
        // duration of the block. We don't escape the reference.
        let note = unsafe { note_ptr.as_ref() };
        handle_activation(&dispatcher_clone, note);
    });

    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidActivateApplicationNotification),
            None,
            Some(&queue),
            &block,
        )
    };

    // Intentionally leak both — the observer needs to outlive any
    // particular `Arc<FocusStealPreventer>` and the singleton has
    // process lifetime.
    std::mem::forget(token);
    std::mem::forget(queue);
}

/// Match a single activation notification against the dispatcher and,
/// for each matching entry, re-activate the entry's `restore_to` pid.
///
/// Runs on the observer queue's background thread — safe to call
/// blocking system APIs.
fn handle_activation(dispatcher: &Arc<Dispatcher>, note: &objc2_foundation::NSNotification) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let activated_pid: i32 = unsafe {
        let info = match note.userInfo() {
            Some(i) => i,
            None => return,
        };
        // userInfo[NSWorkspaceApplicationKey] -> NSRunningApplication*.
        // We go through a raw msg_send to avoid Retained generic
        // bookkeeping for the cross-cast.
        let app_ptr: *mut AnyObject = msg_send![&*info, objectForKey: NSWorkspaceApplicationKey];
        if app_ptr.is_null() {
            return;
        }
        let pid: libc::pid_t = msg_send![app_ptr, processIdentifier];
        pid as i32
    };

    let current_front = crate::apps::frontmost_pid();
    let candidates = dispatcher.snapshot_restore_candidates(activated_pid, current_front);
    for candidate in candidates {
        // Notification delivery and restoration are asynchronous. If another
        // application is already frontmost, the user (or an unrelated system
        // event) won the race; restoring the stale pid would steal focus from
        // that newer foreground. Compare immediately before each restore and
        // fail safe by leaving the newer foreground alone.
        let evidence = dispatcher
            .entries
            .lock()
            .unwrap()
            .get(&candidate.handle.0)
            .and_then(|entry| entry.activity_restore);
        if let Some(evidence) = evidence {
            dispatcher.submit_restore_if_current(candidate, crate::apps::frontmost_pid, |pid| {
                crate::foreground_activity::restore_background_focus(evidence, pid)
            });
        }
    }
}

fn should_restore_after_activation(current_front: Option<i32>, activated_pid: i32) -> bool {
    current_front == Some(activated_pid)
}

/// Re-activate `pid` if it's still running. Safe to call from any
/// thread — Apple documents `activateWithOptions:` as thread-safe.

// ── Tests ────────────────────────────────────────────────────────────────────
//
// These tests exercise the pure-Rust dispatcher half — no Cocoa
// observers, no real notifications. They run under `cargo test
// -p platform-macos focus_steal::`.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Dispatcher::add returns a handle, the entry is reachable by
    /// match, and remove() drops it.
    #[test]
    fn dispatcher_add_match_remove() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(42), 7, "test.add");
        assert_eq!(d.len(), 1);
        let matches = d.snapshot_matches(42, Some(42));
        assert_eq!(matches, vec![7]);
        // Non-matching pid: no restore candidates.
        assert!(d.snapshot_matches(99, Some(99)).is_empty());
        d.remove(h);
        assert_eq!(d.len(), 0);
    }

    /// A target-only lease must not treat unrelated activation as a focus
    /// steal. That activation can be the user switching apps while a
    /// background action is still in its snapshot→detect window.
    #[test]
    fn targeted_lease_does_not_match_third_party_activation() {
        let d = Arc::new(Dispatcher::new());
        let _lease = d.add(Some(42), 7, "test.target_only");

        assert_eq!(d.snapshot_matches(42, Some(42)), vec![7]);
        assert!(
            d.snapshot_matches(99, Some(99)).is_empty(),
            "third-party activation must remain frontmost"
        );
        assert!(
            d.snapshot_matches(7, Some(7)).is_empty(),
            "restore target must not recursively match"
        );
    }

    /// A third-party activation during a target-only lease becomes the latest
    /// foreground to preserve. If the automated target activates afterwards,
    /// it must restore that new app rather than the stale snapshot-time app.
    #[test]
    fn targeted_lease_tracks_latest_unrelated_foreground() {
        let d = Arc::new(Dispatcher::new());
        let _lease = d.add(Some(42), 7, "test.target_tracks_user");

        assert!(d.snapshot_matches(99, Some(99)).is_empty());
        assert_eq!(d.snapshot_matches(42, Some(42)), vec![99]);
    }

    #[test]
    fn targeted_lease_does_not_restore_target_to_itself() {
        let d = Arc::new(Dispatcher::new());
        let _lease = d.add(Some(42), 42, "test.target_already_front");

        assert!(d.snapshot_matches(42, Some(42)).is_empty());
        assert!(d.snapshot_matches(99, Some(99)).is_empty());
        assert_eq!(d.snapshot_matches(42, Some(42)), vec![99]);
    }

    #[test]
    fn stale_activation_notification_does_not_replace_restore_target() {
        let d = Arc::new(Dispatcher::new());
        let _lease = d.add(Some(42), 7, "test.target_ignores_stale");

        assert!(d.snapshot_matches(99, Some(123)).is_empty());
        assert_eq!(d.snapshot_matches(42, Some(42)), vec![7]);
    }

    #[test]
    fn restore_requires_target_to_still_be_frontmost() {
        assert!(should_restore_after_activation(Some(42), 42));
        assert!(!should_restore_after_activation(Some(99), 42));
        assert!(!should_restore_after_activation(None, 42));
    }

    /// Wildcard entries (`target_pid = None`) match every activation
    /// except the entry's own restore_to pid.
    #[test]
    fn wildcard_matches_all_but_restore_to() {
        let d = Arc::new(Dispatcher::new());
        let _h = d.add(None, 7, "test.wild");
        // pid 99 != restore_to 7 → should match.
        assert_eq!(d.snapshot_matches(99, Some(99)), vec![7]);
        // pid 7 == restore_to → must NOT match (don't fight ourselves).
        assert!(d.snapshot_matches(7, Some(7)).is_empty());
    }

    /// A background pixel click intentionally makes its target AppKit-active
    /// without raising it. The wildcard guard must allow that one pid while
    /// continuing to suppress unrelated cross-app activations.
    #[test]
    fn wildcard_can_allow_intentional_target_activation() {
        let d = Arc::new(Dispatcher::new());
        let _h = d.add_allowing(42, 7, "test.allow");

        assert!(
            d.snapshot_matches(42, Some(42)).is_empty(),
            "intentional target activation must not be restored before the click"
        );
        assert_eq!(
            d.snapshot_matches(99, Some(99)),
            vec![7],
            "unrelated activations must remain suppressed"
        );
        assert!(
            d.snapshot_matches(7, Some(7)).is_empty(),
            "restoring the original foreground must never recurse"
        );
    }

    /// Lease Drop is the standard remove path.
    #[test]
    fn lease_drop_removes_entry() {
        // Use a private dispatcher to avoid singleton coupling.
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(1), 2, "test.lease");
        let lease = SuppressionLease {
            handle: h,
            dispatcher: Arc::clone(&d),
            released: false,
        };
        assert_eq!(d.len(), 1);
        drop(lease);
        assert_eq!(d.len(), 0);
    }

    /// Explicit release() short-circuits the Drop path.
    #[test]
    fn lease_release_removes_entry() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(1), 2, "test.lease");
        let lease = SuppressionLease {
            handle: h,
            dispatcher: Arc::clone(&d),
            released: false,
        };
        lease.release();
        assert_eq!(d.len(), 0);
    }

    /// Force a leaked entry whose deadline is already past, then call
    /// reap_expired and snapshot_matches — both must purge it.
    #[test]
    fn deadline_reaps_leaked_entry() {
        let d = Arc::new(Dispatcher::new());
        // Insert a handle manually with a past deadline.
        let id = Uuid::new_v4();
        {
            let mut guard = d.entries.lock().unwrap();
            guard.insert(
                id,
                Entry {
                    activity_restore: None,
                    target_pid: Some(42),
                    allowed_pid: None,
                    restore_to: 7,
                    deadline: Instant::now() - Duration::from_secs(1),
                    returned_tail: false,
                    origin: "test.leak",
                },
            );
        }
        assert_eq!(d.len(), 1);
        // snapshot_matches reaps expired entries before matching.
        let matches = d.snapshot_matches(42, Some(42));
        assert!(matches.is_empty(), "expired entry should not fire");
        assert_eq!(d.len(), 0, "snapshot_matches should purge expired");
    }

    /// Janitor lifecycle: starts on first add, stops when empty,
    /// restarts on next add. Spin up a tokio runtime to host the task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn janitor_starts_stops_restarts() {
        let d = Arc::new(Dispatcher::new());
        // First add → janitor starts.
        let h1 = d.add(Some(1), 2, "test.j1");
        d.kick_janitor();
        // Give the janitor task time to spin up.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The dispatcher should still hold the entry.
        assert_eq!(d.len(), 1);
        // Now remove → janitor goes idle.
        d.remove(h1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(d.len(), 0);
        // Add again — same kick, same lifecycle. (start_janitor is
        // idempotent — already-started task picks up new adds via watch.)
        let _h2 = d.add(Some(3), 4, "test.j2");
        d.kick_janitor();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(d.len(), 1);
    }

    /// Verifies the CodeRabbit #2 fix: `add()` always calls
    /// `kick_janitor()`, regardless of whether the map was empty.
    ///
    /// Scenario: first `add()` happens outside a tokio runtime —
    /// `kick_janitor()` short-circuits via `Handle::try_current()` and
    /// leaves `started = false`. A second `add()` from a tokio-aware
    /// caller (the more common case in practice) must retry the spawn.
    /// The old code skipped the kick because the map was non-empty,
    /// stranding the janitor forever.
    #[test]
    fn add_always_kicks_janitor_after_initial_runtime_miss() {
        let d = Arc::new(Dispatcher::new());
        // Outside any tokio runtime — kick_janitor's `try_current` guard
        // returns Err, the function returns without setting started.
        let h1 = d.add(Some(1), 2, "test.no_runtime");
        assert_eq!(d.len(), 1);
        assert!(
            !*d.janitor_started.lock().unwrap(),
            "kick without a runtime must leave started=false so the next \
             add retries"
        );

        // Now spin up a tokio runtime and add a second entry. The fix
        // is that this *second* add still calls kick_janitor (the old
        // code skipped because the map was already non-empty). Verify
        // by asserting started flips to true.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime");
        let d_in = Arc::clone(&d);
        rt.block_on(async move {
            let _h2 = d_in.add(Some(3), 4, "test.with_runtime");
            assert_eq!(d_in.len(), 2);
            assert!(
                *d_in.janitor_started.lock().unwrap(),
                "second add from inside the runtime must retry the spawn \
                 (regression guard for CodeRabbit #2)"
            );
        });

        // Clean up so the dispatcher doesn't outlive the runtime with
        // a still-armed entry — not strictly needed (Dispatcher is
        // `Send + Sync` and the spawned task holds only a Weak ref),
        // but keeps the test self-contained.
        d.remove(h1);
    }

    /// Snapshot ordering doesn't matter, but the restore pid set
    /// must contain every match. Multiple concurrent suppressions
    /// targeting the same pid should both fire.
    #[test]
    fn multiple_entries_match_independently() {
        let d = Arc::new(Dispatcher::new());
        let _a = d.add(Some(42), 1, "test.m1");
        let _b = d.add(Some(42), 2, "test.m2");
        let matches = d.snapshot_matches(42, Some(42));
        assert_eq!(matches.len(), 2);
        // Set equality — order is HashMap-dependent.
        assert!(matches.contains(&1));
        assert!(matches.contains(&2));
    }

    fn private_lease(d: &Arc<Dispatcher>, handle: SuppressionHandle) -> SuppressionLease {
        SuppressionLease {
            handle,
            dispatcher: Arc::clone(d),
            released: false,
        }
    }

    #[test]
    fn deferred_cancel_preserves_active_wildcard_and_other_targets() {
        let d = Arc::new(Dispatcher::new());
        let tail = d.add(Some(42), 7, "test.tail");
        let active = d.add(Some(42), 7, "test.active");
        let wildcard = d.add(None, 7, "test.wildcard");
        let allowing = d.add_allowing(42, 7, "test.allowing");
        let other = d.add(Some(99), 7, "test.other");
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(d.mark_deferred(tail, deadline).is_some());
        assert!(d.mark_deferred(other, deadline).is_some());
        assert!(d.mark_deferred(wildcard, deadline).is_none());
        assert!(d.mark_deferred(allowing, deadline).is_none());
        d.cancel_deferred(42);
        let entries = d.entries.lock().unwrap();
        assert!(!entries.contains_key(&tail.0));
        for handle in [active, wildcard, allowing, other] {
            assert!(entries.contains_key(&handle.0));
        }
        assert!(!entries[&active.0].returned_tail);
        assert!(!entries[&wildcard.0].returned_tail);
    }

    #[test]
    fn deferred_cancel_invalidates_already_queued_restore() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(42), 7, "test.queued");
        d.mark_deferred(h, Instant::now() + Duration::from_secs(1));
        let candidate = d.snapshot_restore_candidates(42, Some(42))[0];
        d.cancel_deferred(42);
        let mut submitted = Vec::new();
        assert!(!d.submit_restore_if_current(candidate, || Some(42), |pid| submitted.push(pid)));
        assert!(submitted.is_empty());
    }

    #[test]
    fn deferred_restore_rechecks_expiry_without_a_janitor_tick() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(42), 7, "test.queued_expiry");
        let candidate = d.snapshot_restore_candidates(42, Some(42))[0];
        d.entries.lock().unwrap().get_mut(&h.0).unwrap().deadline = Instant::now();
        let mut submitted = Vec::new();
        assert!(!d.submit_restore_if_current(candidate, || Some(42), |pid| submitted.push(pid)));
        assert!(submitted.is_empty());
    }

    #[test]
    fn deferred_restore_rechecks_target_and_latest_restore_anchor() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(42), 7, "test.queued_anchor");
        let candidate = d.snapshot_restore_candidates(42, Some(42))[0];
        assert!(d.snapshot_restore_candidates(99, Some(99)).is_empty());
        let mut submitted = Vec::new();
        assert!(!d.submit_restore_if_current(candidate, || Some(42), |pid| submitted.push(pid)));
        assert!(submitted.is_empty());

        let current = d.snapshot_restore_candidates(42, Some(42))[0];
        assert!(d.submit_restore_if_current(current, || Some(42), |pid| submitted.push(pid)));
        assert_eq!(submitted, vec![99]);
        d.entries.lock().unwrap().get_mut(&h.0).unwrap().target_pid = Some(123);
        assert!(!d.submit_restore_if_current(current, || Some(42), |_| {}));
    }

    #[test]
    fn deferred_restore_submission_and_cancellation_share_entries_lock() {
        let d = Arc::new(Dispatcher::new());
        let _h = d.add(Some(42), 7, "test.submit_lock");
        let candidate = d.snapshot_restore_candidates(42, Some(42))[0];
        assert!(d.submit_restore_if_current(
            candidate,
            || {
                assert!(
                    d.entries.try_lock().is_err(),
                    "foreground check must hold entries lock"
                );
                Some(42)
            },
            |_| assert!(
                d.entries.try_lock().is_err(),
                "restore submission must hold entries lock"
            ),
        ));
        assert!(!d.submit_restore_if_current(candidate, || Some(99), |_| panic!("stale front")));
    }

    #[test]
    fn deferred_old_cleanup_cannot_remove_a_new_same_pid_lease() {
        let d = Arc::new(Dispatcher::new());
        let old = d.add(Some(42), 7, "test.old_tail");
        let old_lease = private_lease(&d, old);
        d.mark_deferred(old, Instant::now() + Duration::from_secs(1));
        d.cancel_deferred(42);
        let new = d.add(Some(42), 99, "test.new_action");
        drop(old_lease);
        assert!(d.entries.lock().unwrap().contains_key(&new.0));
        assert_eq!(d.snapshot_matches(42, Some(42)), vec![99]);
    }

    #[test]
    fn deferred_deadline_never_extends_original_five_second_cap() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(42), 7, "test.cap");
        let original = d.entries.lock().unwrap()[&h.0].deadline;
        assert_eq!(
            d.mark_deferred(h, original + Duration::from_secs(30)),
            Some(original)
        );
        let shorter = Instant::now() + Duration::from_millis(100);
        assert_eq!(d.mark_deferred(h, shorter), Some(shorter));
        assert_eq!(d.mark_deferred(h, original), Some(shorter));
        assert!(d.mark_deferred(h, Instant::now()).is_none());
    }

    #[tokio::test]
    async fn deferred_timer_holds_then_drops_only_its_lease() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(42), 7, "test.timer");
        private_lease(&d, h).defer_release(Instant::now() + Duration::from_millis(30));
        assert!(d.entries.lock().unwrap()[&h.0].returned_tail);
        tokio::time::timeout(Duration::from_secs(1), async {
            while d.entries.lock().unwrap().contains_key(&h.0) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("deferred lease was not released at its deadline");
    }

    #[test]
    fn deferred_runtime_shutdown_drops_timer_owned_lease() {
        let d = Arc::new(Dispatcher::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let h = d.add(Some(42), 7, "test.shutdown");
            private_lease(&d, h).defer_release(Instant::now() + Duration::from_secs(4));
            assert_eq!(d.len(), 1);
        });
        drop(runtime);
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn deferred_without_runtime_drops_immediately() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(42), 7, "test.no_runtime_tail");
        private_lease(&d, h).defer_release(Instant::now() + Duration::from_secs(1));
        assert_eq!(d.len(), 0);
    }
}
