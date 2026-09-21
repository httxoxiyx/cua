//! macOS agent-cursor overlay — one transparent click-through window per display.
//!
//! ## Architecture
//!
//! The MCP/tokio server runs on a **background thread** (spawned in
//! `cua-driver/src/main.rs`).  AppKit MUST run on the **main thread**.
//! The two sides communicate through a bounded command channel:
//!
//! - MCP tool calls → `send_command(OverlayCommand)` → `CMD_TX` (SyncSender)
//! - main thread → `run_on_main_thread()` → owns AppKit windows and notifications
//! - render worker → drains `CMD_RX`, ticks once, paints each display
//!
//! The render loop uses a background thread at ~60 fps while animating. Each
//! tick paints per-display `tiny_skia::Pixmap`s, converts them to `CGImage`s,
//! and dispatches a bounded latest-frame batch back to the main queue.
//!
//! ## Coordinate system
//!
//! All coordinates are **screen points** with the **top-left origin**
//! (matching `OverlayCommand::MoveTo` and AX element coordinates).
//! Each NSWindow uses its NSScreen's AppKit frame. Rendering subtracts that
//! display's CG/AX global origin and applies its own backing scale. Display
//! reconfiguration publishes a new generation; queued old frames are discarded.
//!
//! ## Cross-platform note (2026-05 dedup audit)
//!
//! Animation state + render pipeline live in `cursor_overlay::render_state`
//! (`RenderStateCore`, `tick_swift_constants`, `apply_command_base`,
//! `render_frame`).  macOS uses the hardcoded Swift reference constants
//! (peakSpeed=900, springK=400, overshoot=0.8) and the sentinel-snap
//! variants of MoveTo / ClickPulse — see the wrapper around
//! `apply_command_base` below.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cursor_overlay::{
    CursorConfig, CursorKey, FocusRect, KeyedOverlayCommand, MotionConfig, OverlayCommand,
    OverlayMsg, RenderStateCore, ZOrderEnforcer,
};
use indexmap::IndexMap;

mod displays;
mod surfaces;
use displays::{DisplayGeometry, DisplayLayout};

// ── Arrival-signal channels (one waiter slot per cursor key) ──────────────
//
// Each session's `animate_cursor_to` registers an arrival oneshot keyed by its
// own cursor key. A new animation only supersedes the SAME key's prior waiter,
// so concurrent sessions never cross-cancel each other's arrivals.

static ARRIVAL_TX: Mutex<Option<HashMap<CursorKey, tokio::sync::oneshot::Sender<()>>>> =
    Mutex::new(None);

fn arrival_register(key: CursorKey, tx: tokio::sync::oneshot::Sender<()>) {
    let mut guard = ARRIVAL_TX.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    // Cancel only the same key's previous waiter (superseded by new animation).
    if let Some(old_tx) = map.insert(key, tx) {
        let _ = old_tx.send(());
    }
}

fn arrival_fire(key: &CursorKey) {
    if let Ok(mut guard) = ARRIVAL_TX.lock() {
        if let Some(map) = guard.as_mut() {
            if let Some(tx) = map.remove(key) {
                let _ = tx.send(());
            }
        }
    }
}

fn take_superseded_arrival(
    arrivals: &mut Option<HashMap<CursorKey, tokio::sync::oneshot::Sender<()>>>,
    key: &CursorKey,
    command_enqueued: bool,
) -> Option<tokio::sync::oneshot::Sender<()>> {
    command_enqueued
        .then(|| arrivals.as_mut().and_then(|map| map.remove(key)))
        .flatten()
}

// ── Global overlay state ──────────────────────────────────────────────────

enum RenderEvent {
    Cursor(OverlayMsg),
    DisplaysChanged,
}

static CMD_TX: OnceLock<std::sync::mpsc::SyncSender<RenderEvent>> = OnceLock::new();
// Single-consumer slot; receiver is moved into run_on_main_thread().
static CMD_RX_CELL: Mutex<Option<std::sync::mpsc::Receiver<RenderEvent>>> = Mutex::new(None);
static RENDER: Mutex<Option<RenderMap>> = Mutex::new(None);
static RENDER_LOOP_RUNNING: AtomicBool = AtomicBool::new(false);
const CURSOR_ARRIVAL_TIMEOUT: Duration = Duration::from_secs(10);

/// The keyed, insertion-ordered collection of owned cursors that the render
/// loop composites every frame. Insertion order = stable z-order (later keys
/// paint on top). Animation advances once, then each display paints this same
/// state. Layout changes never reassign, remove, or revive a session's cursor.
struct RenderMap {
    cursors: IndexMap<CursorKey, RenderState>,
    layout: DisplayLayout,
    /// Frozen launch-time config used as the template for lazily-created cursors.
    template: CursorConfig,
    /// Render-side tombstone of ended session cursor keys. A `Cmd`
    /// for a key in here is dropped WITHOUT get-or-create, so an in-flight
    /// click/move from another task that lands AFTER the owning session's
    /// `Remove` can never resurrect the just-removed cursor (the ghost-cursor
    /// resurrection race). An explicit owner-checked `start_session` revival
    /// clears this tombstone before the cursor is reused. "default" is never
    /// tombstoned.
    ended: std::collections::HashSet<CursorKey>,
}

fn publish_display_layout(layout: DisplayLayout) {
    if let Some(map) = RENDER.lock().unwrap().as_mut() {
        map.layout = layout;
    }
    if let Some(tx) = CMD_TX.get() {
        // A full queue already wakes the renderer, which also compares layout
        // generations. Do not block the AppKit notification/retirement path.
        let _ = tx.try_send(RenderEvent::DisplaysChanged);
    }
}

fn apply_render_event(map: &mut RenderMap, event: RenderEvent) -> Option<CursorKey> {
    match event {
        RenderEvent::Cursor(message) => apply_msg(map, message),
        RenderEvent::DisplaysChanged => None,
    }
}

/// Build the `RenderState` for a lazily-created session cursor from the
/// process launch template.
fn render_state_for_key(template: &CursorConfig, key: &str) -> RenderState {
    let mut config = template.clone();
    config.cursor_id = key.to_owned();
    RenderState::new(config)
}

/// Apply one inbound [`OverlayMsg`] to the render map (drain step). Factored
/// out as a pure function so the per-session ownership + removal lifecycle is
/// unit-testable without AppKit.
///
/// Returns the resolved cursor key for a `Cmd` (so the caller can track the
/// last-active key for z-order pinning); `None` for a lifecycle message.
fn apply_msg(map: &mut RenderMap, msg: OverlayMsg) -> Option<CursorKey> {
    match msg {
        OverlayMsg::Remove(key) => {
            // The "default" cursor backs the anonymous / one-shot path and
            // must survive every session_end + the daemon lifetime.
            if key != "default" {
                map.cursors.shift_remove(&key);
                if let Ok(mut guard) = ARRIVAL_TX.lock() {
                    if let Some(m) = guard.as_mut() {
                        m.remove(&key);
                    }
                }
                // Tombstone the key so a late in-flight Cmd from another task
                // (an animate/click racing the owning session's death) cannot
                // re-create the just-removed cursor. Never tombstone "default".
                map.ended.insert(key);
            }
            None
        }
        OverlayMsg::Revive(key) => {
            if key != "default" {
                map.ended.remove(&key);
            }
            None
        }
        OverlayMsg::Cmd(KeyedOverlayCommand { key, cmd }) => {
            // Drop a command for an already-ended session WITHOUT get-or-create
            // — this is the resurrection guard. Without it, a ClickPulse/MoveTo
            // landing after Remove would re-insert (and re-leak) the cursor.
            if map.ended.contains(&key) {
                return None;
            }
            let template = map.template.clone();
            let k = key.clone();
            let rs = map
                .cursors
                .entry(key)
                .or_insert_with(|| render_state_for_key(&template, &k));
            rs.apply_command(cmd);
            Some(k)
        }
    }
}

/// Initialise global overlay state (call once, before run_on_main_thread).
pub fn init(cfg: CursorConfig) {
    static INITIALIZED: OnceLock<()> = OnceLock::new();
    INITIALIZED.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        CMD_TX
            .set(tx)
            .expect("cursor overlay sender is initialized exactly once");
        *CMD_RX_CELL.lock().unwrap() = Some(rx);
        *ARRIVAL_TX.lock().unwrap() = Some(HashMap::new());
        let mut cursors = IndexMap::new();
        cursors.insert("default".to_owned(), RenderState::new(cfg.clone()));
        *RENDER.lock().unwrap() = Some(RenderMap {
            cursors,
            layout: DisplayLayout::default(),
            template: cfg,
            ended: std::collections::HashSet::new(),
        });
    });
    cua_driver_core::cursor_events::install_cursor_event_sink(std::sync::Arc::new(
        |event: cua_driver_core::cursor_events::CursorEvent| {
            use cua_driver_core::cursor_events::{CursorEvent, CursorEventPhase};
            let (session, cmd) = match event {
                CursorEvent::SetSessionLabel { session, label } => {
                    (session, OverlayCommand::SetSessionLabel(label))
                }
                CursorEvent::Action {
                    session,
                    phase: CursorEventPhase::Begin,
                    semantics,
                } => (
                    session,
                    OverlayCommand::BeginAction {
                        action: semantics.action,
                        delivery: semantics.delivery,
                        target: semantics.target,
                    },
                ),
                CursorEvent::Action {
                    session,
                    phase: CursorEventPhase::End,
                    semantics,
                } => (session, OverlayCommand::EndAction(semantics.action)),
                CursorEvent::SelectTheme { session, selection } => (
                    session,
                    OverlayCommand::SetTheme {
                        theme_id: selection.theme_id,
                        reduced_motion: selection.reduced_motion,
                    },
                ),
            };
            send_command(session, cmd);
        },
    ));
}

/// Send a keyed command from any thread (MCP tool, etc.). Non-blocking; returns
/// `false` if the cursor is disabled for this callsite, the renderer is absent,
/// or the bounded queue is full. Callers that suppress fallback feedback must
/// check the return value.
pub fn send_command(key: CursorKey, cmd: OverlayCommand) -> bool {
    // Empty key is the explicit no-cursor sentinel for direct platform calls
    // that bypass lifecycle dispatch.
    if key.is_empty() {
        return false;
    }
    if let Some(tx) = CMD_TX.get() {
        return tx
            .try_send(RenderEvent::Cursor(OverlayMsg::Cmd(KeyedOverlayCommand {
                key,
                cmd,
            })))
            .is_ok();
    }
    false
}

/// Convenience for callsites not yet threaded with a session key: drives the
/// seeded `"default"` cursor (the anonymous / one-shot identity).
pub fn send_command_default(cmd: OverlayCommand) {
    send_command("default".to_owned(), cmd);
}

/// Truthful render acknowledgement for lifecycle inspection. This never falls
/// back to the seeded default cursor: an absent, off-screen, disabled, or
/// idle-faded session cursor is not reported as visible.
pub fn is_visible_for_session(key: &str) -> bool {
    RENDER
        .lock()
        .ok()
        .and_then(|guard| {
            guard.as_ref().and_then(|map| {
                map.cursors
                    .get(key)
                    .map(|state| cursor_is_externally_visible(&map.layout, state))
            })
        })
        .unwrap_or(false)
}

/// Remove a session's owned cursor from the render collection (fired from the
/// `session_end` hook). The `"default"` key is guarded against removal on the
/// render side, so this is a no-op for it; removing an absent key (anonymous
/// session that never created a cursor) is a harmless no-op.
pub fn remove_cursor(key: CursorKey) {
    if key.is_empty() {
        return;
    }
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.try_send(RenderEvent::Cursor(OverlayMsg::Remove(key)));
    }
}

/// Clear the render-side tombstone after a successful explicit session
/// revival. Cursor recreation remains lazy until the next render command.
pub fn revive_cursor(key: CursorKey) {
    if key.is_empty() {
        return;
    }
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.try_send(RenderEvent::Cursor(OverlayMsg::Revive(key)));
    }
}

/// Return a snapshot of a cursor's current motion config (for use by
/// set_agent_cursor_motion to apply partial overrides without losing other
/// knobs). Reads the motion of the cursor `key`, falling back to the
/// `"default"` cursor's motion when that key has no own entry yet (e.g. a
/// session whose first motion call precedes any move/enable).
pub fn current_motion(key: &str) -> MotionConfig {
    let guard = RENDER.lock().unwrap();
    let Some(map) = guard.as_ref() else {
        return MotionConfig::default();
    };
    map.cursors
        .get(key)
        .or_else(|| map.cursors.get("default"))
        .map(|rs| rs.core.motion.clone())
        .unwrap_or_default()
}

/// Return the render-owned theme and semantic playback state for one cursor.
pub fn current_theme_state(
    key: &str,
) -> Option<(
    String,
    String,
    String,
    Option<String>,
    cursor_overlay::CursorVisualState,
)> {
    let guard = RENDER.lock().unwrap();
    let map = guard.as_ref()?;
    let state = map
        .cursors
        .get(key)
        .or_else(|| map.cursors.get("default"))?;
    let (id, version, profile, fallback) = state.core.active_theme_metadata();
    Some((id, version, profile, fallback, state.core.visual.clone()))
}

/// Seed a brand-new (sentinel-positioned) cursor at an on-screen start point
/// offset up-left of `(target_x, target_y)` so the immediately-following
/// `MoveTo` glides INTO the target instead of silently snapping. Without this,
/// a cursor's very first action (common on a pure-AX run — launch app, AX-press
/// a button) produces no visible motion: `animate_cursor_to` early-returned at
/// the sentinel and only `ClickPulse` snapped a static arrow, which is easy to
/// miss. See the AX-no-glide report.
///
/// No-op for an initialized cursor on a connected display. First placement or
/// recovery from an unplugged display seeds on/near the target's display.
fn seed_start_if_sentinel(key: &CursorKey, target_x: f64, target_y: f64) -> bool {
    let mut guard = RENDER.lock().unwrap();
    let Some(map) = guard.as_mut() else {
        return false;
    };
    seed_start_in_map(map, key, target_x, target_y)
}

/// Pure seed step operating on a borrowed [`RenderMap`] — factored out of
/// `seed_start_if_sentinel` so the get-or-create + clamp logic is unit-testable
/// without the global `RENDER` static or AppKit.
fn seed_start_in_map(map: &mut RenderMap, key: &CursorKey, target_x: f64, target_y: f64) -> bool {
    if !target_x.is_finite() || !target_y.is_finite() {
        return false;
    }
    // Respect the resurrection guard: never seed (and thus re-create) a cursor
    // whose session already ended.
    if map.ended.contains(key) {
        return false;
    }
    // Get-or-create the cursor so the very first AX action seeds + glides even
    // when the lazy render-thread creation hasn't drained the PinAbove yet
    // (the render loop's drain would otherwise win the race and the seed read
    // an absent cursor). Mirrors apply_msg's entry().or_insert_with.
    let template = map.template.clone();
    let k = key.clone();
    let rs = map
        .cursors
        .entry(key.clone())
        .or_insert_with(|| render_state_for_key(&template, &k));
    if !rs.core.cfg.enabled || (rs.core.has_position() && map.layout.contains(rs.core.pos)) {
        return false;
    }
    let seed = map
        .layout
        .seed_near((target_x, target_y))
        .unwrap_or((target_x - 140.0, target_y - 140.0));
    rs.core.set_position(seed);
    true
}

/// Animate the overlay cursor to `(x, y)` and suspend until the Dubins path
/// completes and the spring overshoot begins.
///
/// Mirrors Swift's `AgentCursor.shared.animateAndWait(to:)`.
/// Returns immediately (no animation) only when the overlay is disabled for
/// this cursor. A brand-new cursor still at the off-screen sentinel is first
/// seeded on-screen via [`seed_start_if_sentinel`] so its FIRST action glides
/// in (it previously snapped silently via `ClickPulse`, invisible on a pure-AX
/// run).
pub async fn animate_cursor_to(key: CursorKey, x: f64, y: f64) {
    // Empty key is the explicit no-cursor sentinel → nothing to animate.
    if key.is_empty() {
        return;
    }
    // Seed a sentinel cursor on-screen so the MoveTo below glides instead of
    // being short-circuited. After this the cursor is initialized, so the
    // should-animate check passes on the first action just like later ones.
    seed_start_if_sentinel(&key, x, y);

    // Check whether animation should run for THIS cursor. A disabled cursor
    // never animates; an absent cursor (seed found nothing to prime) is skipped.
    let should_animate = {
        let guard = RENDER.lock().unwrap();
        matches!(
            guard.as_ref().and_then(|m| m.cursors.get(&key)),
            Some(rs) if rs.core.cfg.enabled && rs.core.has_position()
        )
    };
    if !should_animate {
        return;
    }
    if !RENDER_LOOP_RUNNING.load(Ordering::Acquire) {
        tracing::warn!(
            target: "cursor",
            cursor = %key,
            "cursor renderer is unavailable; continuing without animation"
        );
        return;
    }

    // Create a one-shot channel; store the sender (keyed) so the render thread
    // can fire it when this cursor's path finishes.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    arrival_register(key.clone(), tx);

    // Send the MoveTo command (click offset applied inside apply_command).
    if !send_command(
        key.clone(),
        OverlayCommand::MoveTo {
            x,
            y,
            // Arrive pointing upper-left (45°), matching the macOS system-cursor
            // convention and Swift reference (`endAngleDegrees: 45`).
            end_heading_radians: std::f64::consts::FRAC_PI_4,
        },
    ) {
        // Do not make an unavailable/full renderer look like a ten-second
        // cursor animation. Resolve the waiter immediately and let the real
        // desktop action continue.
        arrival_fire(&key);
    }

    // Visual feedback must never hold the real desktop action indefinitely.
    // The renderer normally fires this promptly; the timeout is a final guard
    // against a stopped or wedged UI pump.
    if tokio::time::timeout(CURSOR_ARRIVAL_TIMEOUT, rx)
        .await
        .is_err()
    {
        tracing::warn!(
            target: "cursor",
            cursor = %key,
            "cursor animation timed out; continuing with the desktop action"
        );
    }
}

/// Present decorative click feedback without changing input-delivery timing.
///
/// The default synchronized mode preserves the historical contract by waiting
/// for the cursor glide; the caller then emits its ordinary `ClickPulse` at the
/// existing point in the action flow. With `--async-click-feedback`, enqueue
/// one render-thread-owned glide+arrival-pulse command and return immediately
/// so the real pointer action is not coupled to overlay frame cadence.
///
/// Returns `true` when the arrival pulse was queued. The caller must then skip
/// its ordinary immediate `ClickPulse` command.
pub fn async_click_feedback_enabled(key: &str) -> bool {
    {
        let guard = RENDER.lock().unwrap();
        guard
            .as_ref()
            .and_then(|map| map.cursors.get(key).map(|state| &state.core.cfg))
            .or_else(|| guard.as_ref().map(|map| &map.template))
            .is_some_and(|cfg| cfg.async_click_feedback)
    }
}

/// Queue a renderer-owned glide followed by a click pulse. Returns `true`
/// only when the command was accepted, so callers can retain an immediate
/// pulse fallback if the bounded queue is unavailable.
pub fn queue_async_click_feedback(key: CursorKey, x: f64, y: f64) -> bool {
    queue_async_cursor_feedback(key, x, y, true)
}

fn cursor_feedback_move(x: f64, y: f64, click_pulse: bool) -> OverlayCommand {
    let end_heading_radians = std::f64::consts::FRAC_PI_4;
    if click_pulse {
        OverlayCommand::MoveToThenClickPulse {
            x,
            y,
            end_heading_radians,
        }
    } else {
        OverlayCommand::MoveTo {
            x,
            y,
            end_heading_radians,
        }
    }
}

fn queue_async_cursor_feedback(key: CursorKey, x: f64, y: f64, click_pulse: bool) -> bool {
    if key.is_empty() {
        return false;
    }
    seed_start_if_sentinel(&key, x, y);
    let should_animate = {
        let guard = RENDER.lock().unwrap();
        matches!(
            guard.as_ref().and_then(|map| map.cursors.get(&key)),
            Some(state) if state.core.cfg.enabled && state.core.has_position()
        )
    };
    if !should_animate || !RENDER_LOOP_RUNNING.load(Ordering::Acquire) {
        return false;
    }
    // Hold the arrival map lock across enqueue + waiter removal. The render
    // thread also takes this lock when a path arrives, so it cannot mistake
    // this new asynchronous path's arrival for the superseded synchronous
    // move. If enqueue fails, leave the original waiter untouched.
    let superseded = {
        let mut arrivals = ARRIVAL_TX.lock().unwrap();
        let enqueued = send_command(key.clone(), cursor_feedback_move(x, y, click_pulse));
        if !enqueued {
            return false;
        }
        take_superseded_arrival(&mut arrivals, &key, true)
    };
    if let Some(sender) = superseded {
        let _ = sender.send(());
    }
    true
}

pub async fn animate_click_feedback(key: CursorKey, x: f64, y: f64) -> bool {
    let asynchronous = async_click_feedback_enabled(&key);

    if asynchronous {
        return queue_async_click_feedback(key, x, y);
    }

    animate_cursor_to(key, x, y).await;
    false
}

async fn animate_typing_feedback_using(
    asynchronous: bool,
    enqueue_move: impl FnOnce() -> bool,
    wait_for_arrival: impl std::future::Future<Output = ()>,
) {
    if asynchronous {
        // A missing/full renderer must not reintroduce an arrival wait.
        let _ = enqueue_move();
    } else {
        wait_for_arrival.await;
    }
}

/// Show element-targeted typing feedback without synthesizing a click pulse.
pub async fn animate_typing_feedback(key: CursorKey, x: f64, y: f64) {
    let wait_key = key.clone();
    animate_typing_feedback_using(
        async_click_feedback_enabled(&key),
        || queue_async_cursor_feedback(key, x, y, false),
        animate_cursor_to(wait_key, x, y),
    )
    .await;
}

/// Block the calling thread (must be the OS main thread) running the AppKit
/// event loop and the overlay window.  Never returns normally.
///
/// Call this from `main()` after spawning the tokio background thread.
pub fn run_on_main_thread() {
    // Take the receiver.
    let rx = match CMD_RX_CELL.lock().unwrap().take() {
        Some(r) => r,
        None => {
            // init() was never called — no overlay, just spin.
            loop {
                std::thread::park();
            }
        }
    };

    let cfg = {
        let guard = RENDER.lock().unwrap();
        match guard.as_ref() {
            Some(m) => m.template.clone(),
            None => return,
        }
    };

    if !cfg.enabled {
        loop {
            std::thread::park();
        }
    }

    // AppKit's `+[NSApplication sharedApplication]` registers the process with
    // the Window Server and ABORTS the whole process (SIGABRT in
    // `_RegisterApplication`) when there's no graphic-session access — e.g.
    // `mcp` run as a stdio child from SSH, a LaunchDaemon, or headless CI.
    // Detect that without touching AppKit and run headless: the MCP server
    // keeps serving on its background thread while this thread just parks,
    // exactly as it does when the overlay is disabled. See issue #1724.
    if !crate::session::has_graphic_access() {
        tracing::warn!(
            "no Window Server / graphic-session access — skipping cursor \
             overlay and running headless (issue #1724)"
        );
        loop {
            std::thread::park();
        }
    }

    // ------------------------------------------------------------------
    // AppKit setup (all on the main thread).
    // ------------------------------------------------------------------
    unsafe { run_appkit(cfg, rx) };
}

// ── Animation / render state ──────────────────────────────────────────────
//
// The platform-agnostic fields + tick + apply_command + render pipeline live
// in `cursor_overlay::render_state` (2026-05 dedup audit). What stays here
// is the macOS-specific NSScreen window dimensions and the focus-rect
// overlay (a macOS-only post-arrival element highlight).

struct RenderState {
    core: RenderStateCore,
    /// Focus-highlight rectangle `[x, y, w, h]` in screen coords; None = not shown.
    focus_rect: Option<[f64; 4]>,
    /// Fade progress for the focus rect: 0.0 = fully visible, 1.0 = gone.
    focus_rect_t: f64,
}

impl RenderState {
    fn new(cfg: CursorConfig) -> Self {
        RenderState {
            core: RenderStateCore::new(cfg),
            focus_rect: None,
            focus_rect_t: 1.0,
        }
    }

    /// Advance the animation by `dt`.  Uses the Swift reference constants
    /// (peakSpeed=900, springK=400, overshoot=0.8) — see
    /// [`RenderStateCore::tick_swift_constants`].  Returns true if an
    /// arrival signal should be fired (the path just ended).
    fn tick(&mut self, dt: f64) -> bool {
        let fire_arrival = self.core.tick_swift_constants(dt);

        // Advance focus-rect fade (fades out over ~600ms).  macOS-only —
        // the shared core has no focus_rect concept.
        if self.focus_rect.is_some() {
            self.focus_rect_t = (self.focus_rect_t + dt / 0.6).min(1.0);
            if self.focus_rect_t >= 1.0 {
                self.focus_rect = None;
                self.focus_rect_t = 1.0;
            }
        }

        fire_arrival
    }

    fn apply_command(&mut self, cmd: OverlayCommand) {
        // macOS snaps MoveTo / ClickPulse only before first placement:
        //   - MoveTo only snaps `self.pos` if the cursor is unpositioned
        //     (otherwise the path starts
        //     from the current position so the animation is continuous).
        //   - ClickPulse only updates `self.pos` if the cursor is still at
        //     unpositioned (otherwise the animation already landed it there).
        match cmd {
            OverlayCommand::ShowFocusRect(rect) => {
                self.focus_rect = rect;
                self.focus_rect_t = 0.0; // reset fade to fully visible
            }
            other => {
                let _ = self.core.apply_command_base(other, true, true);
            }
        }
    }

    /// True while the render loop must wake at frame cadence because the next
    /// tick can change pixels. A brand-new sentinel cursor is deliberately
    /// quiescent, so `serve` with no agent activity can block on the command
    /// channel instead of compositing an empty fullscreen pixmap at 60fps.
    fn needs_frame_tick(&self) -> bool {
        self.core.path.is_some()
            || self.core.spring.is_some()
            || self.core.click_t.is_some()
            || self.focus_rect.is_some()
            || self.core.session_badge_needs_frame_tick()
            || (self.core.motion.idle_hide_ms > 0.0
                && self.core.visible
                && self.core.has_position()
                && self.core.idle_alpha >= 0.004)
    }
}

fn render_map_needs_frame_tick(map: &RenderMap) -> bool {
    map.cursors.values().any(RenderState::needs_frame_tick)
}

// ── AppKit / CGImage plumbing ─────────────────────────────────────────────

unsafe fn run_appkit(_cfg: CursorConfig, rx: std::sync::mpsc::Receiver<RenderEvent>) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // ---- NSApplication ----
    // Verify main thread (MainThreadMarker is a zero-size compile-time token).
    let _mtm = objc2_foundation::MainThreadMarker::new()
        .expect("run_appkit must be called from the main thread");

    let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
    // NSApplicationActivationPolicyAccessory = 1 (no Dock icon, no menu bar)
    // setActivationPolicy: returns BOOL (success), not void.
    let _: bool = msg_send![app, setActivationPolicy: 1i64];
    // Finish launching without presenting a UI (needed for NSApp.run())
    let _: () = msg_send![app, finishLaunching];

    // Every display owns a surface at its own origin/backing scale. AppKit
    // notifications also handle later hotplug, arrangement and scale changes.
    surfaces::initialize();

    RENDER_LOOP_RUNNING.store(true, Ordering::Release);
    std::thread::spawn(move || {
        render_loop(rx);
        RENDER_LOOP_RUNNING.store(false, Ordering::Release);
    });

    // ---- NSApplication run loop (blocks until process exits) ----
    let _: () = msg_send![app, run];
}

fn render_loop(rx: std::sync::mpsc::Receiver<RenderEvent>) {
    let target_frame_ms = Duration::from_millis(16); // ~60 fps while pixels can change
    let hover_poll_ms = Duration::from_millis(80);
    let mut last_tick = Instant::now();
    let mut frame_tick_needed = false;
    let mut hover_poll_needed = false;
    // Repin bookkeeping: track last pinned wid and a frame counter for
    // the periodic defensive-repin (every ~60 active frames ≈ 1 s).
    let mut last_pinned: Option<u64> = None;
    let mut repin_frames: u32 = 0;
    let mut last_layout_generation = 0;

    loop {
        // When no cursor animation/fade is active, block until the MCP side
        // sends a command. This is the idle-server fast path: no fullscreen
        // pixmap allocation, no CGImage conversion, no 60fps wakeup.
        let (first_msg, hover_poll_tick) = if frame_tick_needed {
            (None, hover_poll_needed)
        } else if hover_poll_needed {
            match rx.recv_timeout(hover_poll_ms) {
                Ok(msg) => (Some(msg), true),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (None, true),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(msg) => (Some(msg), false),
                Err(_) => break,
            }
        };

        let woke_from_idle = first_msg.is_some();
        let now = Instant::now();
        let dt = if woke_from_idle {
            // The blocking recv() above can span an arbitrarily long idle period.
            // Do not charge that time to the first animation tick after a command;
            // let the wake-up frame render the newly-applied state at t=0.
            0.0
        } else {
            now.duration_since(last_tick).as_secs_f64().min(0.05)
        };
        last_tick = now;

        // ── Phase 1: drain + tick all cursors (one lock acquisition) ──────
        // Preserve the existing last-active-cursor z-order policy, now applied
        // to every display surface. Tick/arrival ownership remains per cursor,
        // not per display, so adding a monitor cannot double animation speed.
        let (
            pinned_wid,
            raise_unpinned,
            arrived,
            layout,
            had_msg,
            hover_changed,
            next_frame_tick_needed,
            next_hover_poll_needed,
        ) = {
            let mut guard = RENDER.lock().unwrap();
            match guard.as_mut() {
                Some(map) => {
                    // Drain via get-or-create; track the last-touched key so we
                    // can read its pinned_wid after ticking.
                    let mut last_key: Option<CursorKey> = None;
                    let mut had_msg = false;
                    if let Some(msg) = first_msg {
                        had_msg = true;
                        if let Some(k) = apply_render_event(map, msg) {
                            last_key = Some(k);
                        }
                    }
                    while let Ok(msg) = rx.try_recv() {
                        had_msg = true;
                        if let Some(k) = apply_render_event(map, msg) {
                            last_key = Some(k);
                        }
                    }
                    // Tick every cursor while an animation/fade is in progress
                    // or immediately after a command changed render state. The
                    // latter lets a just-created path/click/focus rect start on
                    // this frame without waiting for the next 16ms tick.
                    let mut arrived: Vec<CursorKey> = Vec::new();
                    if frame_tick_needed || had_msg {
                        for (k, rs) in map.cursors.iter_mut() {
                            if rs.tick(dt) {
                                arrived.push(k.clone());
                            }
                        }
                    }
                    let pointer = if hover_poll_tick
                        || map
                            .cursors
                            .values()
                            .any(|rs| rs.core.session_badge_needs_hover_poll())
                    {
                        hardware_cursor_position()
                    } else {
                        None
                    };
                    let mut hover_changed = false;
                    if pointer.is_some() || hover_poll_tick {
                        for rs in map.cursors.values_mut() {
                            hover_changed |= rs.core.update_session_badge_hover(pointer);
                        }
                    }
                    let pinned = last_key
                        .as_ref()
                        .and_then(|k| map.cursors.get(k))
                        .map(|rs| rs.core.pinned_wid)
                        .unwrap_or(last_pinned);
                    let raise_unpinned = last_key
                        .as_ref()
                        .and_then(|k| map.cursors.get(k))
                        .is_some_and(|state| cursor_is_externally_visible(&map.layout, state))
                        && pinned.is_none();
                    let raise_unpinned = raise_unpinned
                        || (pinned.is_none()
                            && map.layout.generation != last_layout_generation
                            && map
                                .cursors
                                .values()
                                .any(|state| cursor_is_externally_visible(&map.layout, state)));
                    let next_frame_tick_needed = render_map_needs_frame_tick(map);
                    let next_hover_poll_needed = map
                        .cursors
                        .values()
                        .any(|rs| rs.core.session_badge_needs_hover_poll());
                    (
                        pinned,
                        raise_unpinned,
                        arrived,
                        map.layout.clone(),
                        had_msg,
                        hover_changed,
                        next_frame_tick_needed,
                        next_hover_poll_needed,
                    )
                }
                None => break,
            }
        };

        // Fire arrival signals so each session's animate_cursor_to() unblocks.
        for k in &arrived {
            arrival_fire(k);
        }

        // Repin: immediately on target change, then defensive every ~1 s while
        // the render loop is active. When quiescent, z-order is left unchanged
        // until the next command wakes the loop.
        let layout_changed = layout.generation != last_layout_generation;
        if frame_tick_needed || had_msg || layout_changed {
            repin_frames += 1;
            let pin_changed = pinned_wid != last_pinned;
            last_pinned = pinned_wid;
            if pinned_wid.is_some() && (pin_changed || layout_changed || repin_frames >= 60) {
                MacZOrderEnforcer {
                    generation: layout.generation,
                }
                .reassert(pinned_wid);
                repin_frames = 0;
            } else if raise_unpinned {
                // A direct move_cursor has no target window to pin against.
                // Raise the normal-level, click-through overlay without
                // activating the driver so a later foreground application
                // cannot cover a standalone session cursor.
                surfaces::order_front(layout.generation);
                repin_frames = 0;
            } else if repin_frames >= 60 {
                repin_frames = 0;
            }
        }

        // ── Phase 2: paint the same animation state on each display ───────
        // Render only when a command arrived or the previous/next tick can
        // change pixels. A final frame is emitted as animations/fades finish so
        // the layer is left in the completed/cleared state before blocking.
        if had_msg || hover_changed || frame_tick_needed || next_frame_tick_needed || layout_changed
        {
            let pixmaps = {
                let guard = RENDER.lock().unwrap();
                if let Some(map) = guard.as_ref() {
                    layout
                        .displays
                        .iter()
                        .filter_map(|display| {
                            render_display(map, display).map(|pixmap| (display.id, pixmap))
                        })
                        .collect()
                } else {
                    break;
                }
            };
            surfaces::submit_frames(layout.generation, pixmaps);
        }

        last_layout_generation = layout.generation;
        frame_tick_needed = next_frame_tick_needed;
        hover_poll_needed = next_hover_poll_needed;
        if frame_tick_needed {
            // Sleep remainder of frame budget.
            let elapsed = Instant::now().duration_since(last_tick);
            if let Some(remaining) = target_frame_ms.checked_sub(elapsed) {
                std::thread::sleep(remaining);
            }
        }
    }
}

fn render_display(map: &RenderMap, display: &DisplayGeometry) -> Option<tiny_skia::Pixmap> {
    let (width, height) = display.pixel_size()?;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    for state in map.cursors.values() {
        let focus = state.focus_rect.map(|rect| FocusRect {
            rect,
            t: state.focus_rect_t,
        });
        cursor_overlay::paint_cursor(
            &mut pixmap,
            &state.core,
            display.bounds[0],
            display.bounds[1],
            focus,
            display.scale as f32,
        );
    }
    Some(pixmap)
}

fn hardware_cursor_position() -> Option<(f64, f64)> {
    use core_graphics::{
        event::CGEvent,
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let event = CGEvent::new(source).ok()?;
    let location = event.location();
    Some((location.x, location.y))
}

fn cursor_is_externally_visible(layout: &DisplayLayout, state: &RenderState) -> bool {
    state.core.cfg.enabled
        && state.core.visible
        && state.core.has_position()
        && layout.contains(state.core.pos)
        && state.core.idle_alpha >= 0.004
}

/// Order the overlay NSWindow just above `target_wid` in the global window
/// server list.  Called from the render thread; dispatches to the main queue
/// (AppKit must be used on the main thread).
///
/// `NSWindowAbove = 1`; `orderWindow:relativeTo:` accepts any CGWindowID as
/// the `relativeTo` argument — it works cross-application via CGS.
fn target_is_frontmost_visible_window(
    target_wid: u64,
    frontmost_pid: Option<i32>,
    windows: &[crate::windows::WindowInfo],
) -> bool {
    let Some(target) = windows
        .iter()
        .find(|window| u64::from(window.window_id) == target_wid)
    else {
        return false;
    };
    if !target.is_on_screen || target.layer != 0 || frontmost_pid != Some(target.pid) {
        return false;
    }

    windows
        .iter()
        .filter(|window| {
            window.is_on_screen
                && window.layer == 0
                && window.pid == target.pid
                && window.bounds.width > 1.0
                && window.bounds.height > 1.0
        })
        .max_by_key(|window| window.z_index)
        .is_some_and(|window| u64::from(window.window_id) == target_wid)
}

fn dispatch_pin_above(generation: u64, target_wid: u64) {
    let windows = crate::windows::visible_windows();
    let raise_front =
        target_is_frontmost_visible_window(target_wid, crate::apps::frontmost_pid(), &windows);
    surfaces::order_above(generation, target_wid, raise_front);
}

// ── Z-order enforcer (macOS impl of cursor_overlay::ZOrderEnforcer) ──────

/// macOS implementation of [`cursor_overlay::ZOrderEnforcer`].
///
/// Carries a layout generation rather than a raw NSWindow pointer. AppKit
/// resolves only that generation's owned windows on the main thread.
///
/// `target = None` is treated as a no-op here. Direct unpinned cursor commands
/// raise the overlay once in the render loop, while this enforcer remains
/// responsible only for target-relative ordering.
struct MacZOrderEnforcer {
    generation: u64,
}

impl ZOrderEnforcer for MacZOrderEnforcer {
    fn reassert(&self, target: Option<u64>) {
        if let Some(wid) = target {
            dispatch_pin_above(self.generation, wid);
        }
        // target = None → no-op; see struct doc comment.
    }
}

/// Create a `CGImage` from a `tiny_skia::Pixmap` (premultiplied RGBA).
/// Returns a `+1` retained pointer that the caller must release.
fn pixmap_to_cgimage(pixmap: &tiny_skia::Pixmap) -> Option<usize> {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    if w == 0 || h == 0 {
        return None;
    }

    let data = pixmap.data();
    let bytes_per_row = w * 4;

    // tiny-skia produces premultiplied RGBA with bytes in memory order [R, G, B, A].
    // CGImage flag breakdown (Apple CGBitmapInfo / CGImageAlphaInfo enums):
    //   kCGImageAlphaPremultipliedLast = 0x0001  → alpha is the LAST channel  (RGBA)
    //   kCGImageAlphaPremultipliedFirst = 0x0002 → alpha is the FIRST channel (ARGB)  ← NOT what we want
    //   kCGBitmapByteOrder32Big        = 0x4000  → big-endian 32-bit pixel,
    //     so memory order is the same as component order (bytes = [R, G, B, A]).
    // Combined: kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big = 0x4001
    // This correctly maps tiny-skia's [R, G, B, A] bytes to the display RGB channels.
    const BITMAP_INFO: u32 = 0x0001 | 0x4000; // kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big

    // Release callback: CGDataProvider calls this when it is done with the buffer.
    // `info` is the Box<Vec<u8>> we passed as the `info` argument below.
    unsafe extern "C" fn release_pixel_data(info: *mut c_void, _data: *const c_void, _size: usize) {
        // Re-box and drop to free the buffer.
        drop(Box::from_raw(info as *mut Vec<u8>));
    }

    unsafe {
        extern "C" {
            fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
            fn CGColorSpaceRelease(cs: *mut c_void);
            fn CGDataProviderCreateWithData(
                info: *mut c_void,
                data: *const c_void,
                size: usize,
                release_data: Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>,
            ) -> *mut c_void;
            fn CGDataProviderRelease(provider: *mut c_void);
            fn CGImageCreate(
                width: usize,
                height: usize,
                bits_per_component: usize,
                bits_per_pixel: usize,
                bytes_per_row: usize,
                color_space: *mut c_void,
                bitmap_info: u32,
                provider: *mut c_void,
                decode: *const f64,
                should_interpolate: bool,
                intent: u32,
            ) -> *mut c_void;
        }

        // Copy the pixel data into a heap Vec; the data provider will own it
        // and free it via release_pixel_data when the CGImage is released.
        let copied: Vec<u8> = data.to_vec();
        let len = copied.len();
        let ptr = copied.as_ptr();
        // Leak the Vec into a raw Box so we can pass it as the `info` opaque pointer.
        let copied_box: *mut Vec<u8> = Box::into_raw(Box::new(copied));

        let cs = CGColorSpaceCreateDeviceRGB();
        let provider = CGDataProviderCreateWithData(
            copied_box as *mut c_void,
            ptr as *const c_void,
            len,
            Some(release_pixel_data), // frees copied_box when provider is released
        );
        let img = CGImageCreate(
            w,
            h,
            8,  // bits_per_component
            32, // bits_per_pixel
            bytes_per_row,
            cs,
            BITMAP_INFO,
            provider,
            std::ptr::null(),
            false,
            0, // kCGRenderingIntentDefault
        );

        CGColorSpaceRelease(cs);
        CGDataProviderRelease(provider);
        // Do NOT drop copied_box here — release_pixel_data owns it now.

        if img.is_null() {
            None
        } else {
            Some(img as usize)
        }
    }
}

// ── Headless unit tests for the keyed render collection ───────────────────
//
// These prove the per-session ownership data model, the session_end removal
// lifecycle, the "default" guard, and per-key arrival isolation WITHOUT any
// AppKit / NSWindow. The on-screen rendering (CGImage / CALayer setContents)
// still needs a real display and is verified separately on the macOS VM.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn asynchronous_typing_feedback_uses_a_move_without_a_click_pulse() {
        assert!(matches!(
            cursor_feedback_move(12.0, 34.0, false),
            OverlayCommand::MoveTo {
                x: 12.0,
                y: 34.0,
                ..
            }
        ));
        assert!(matches!(
            cursor_feedback_move(12.0, 34.0, true),
            OverlayCommand::MoveToThenClickPulse {
                x: 12.0,
                y: 34.0,
                ..
            }
        ));
    }

    #[test]
    fn asynchronous_typing_feedback_does_not_wait_for_arrival() {
        use std::{
            cell::Cell,
            future::Future,
            task::{Context, Poll, Waker},
        };
        for accepted in [false, true] {
            let queued = Cell::new(false);
            let mut feedback = Box::pin(animate_typing_feedback_using(
                true,
                || {
                    queued.set(true);
                    accepted
                },
                std::future::pending(),
            ));
            let mut context = Context::from_waker(Waker::noop());
            assert!(matches!(
                feedback.as_mut().poll(&mut context),
                Poll::Ready(())
            ));
            assert!(queued.get(), "async mode must attempt one nonblocking move");
        }
    }

    #[test]
    fn synchronous_typing_feedback_still_waits_for_arrival() {
        use std::{
            future::Future,
            task::{Context, Poll, Waker},
        };
        let mut feedback = Box::pin(animate_typing_feedback_using(
            false,
            || panic!("synchronous mode must not enqueue an async move"),
            std::future::pending(),
        ));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            feedback.as_mut().poll(&mut context),
            Poll::Pending
        ));
    }

    #[test]
    fn keyed_render_state_carries_the_session_color_identity() {
        let state = render_state_for_key(&CursorConfig::default(), "session-blueprint");
        assert_eq!(state.core.cfg.cursor_id, "session-blueprint");
    }

    fn window(window_id: u32, pid: i32, z_index: usize) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid,
            app_name: format!("app-{pid}"),
            title: String::new(),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            layer: 0,
            z_index,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    fn empty_map() -> RenderMap {
        let mut cursors = IndexMap::new();
        cursors.insert(
            "default".to_owned(),
            RenderState::new(CursorConfig::default()),
        );
        RenderMap {
            cursors,
            layout: DisplayLayout {
                generation: 1,
                displays: vec![DisplayGeometry::new(
                    1,
                    [0.0, 0.0, 100.0, 100.0],
                    [0.0, 0.0, 100.0, 100.0],
                    1.0,
                )
                .unwrap()],
            },
            template: CursorConfig::default(),
            ended: std::collections::HashSet::new(),
        }
    }

    #[test]
    fn frontmost_target_can_raise_overlay_without_covering_another_app() {
        let target_pid = 100;
        let mut windows = vec![window(10, target_pid, 20), window(11, 200, 10)];
        // WindowServer may retain another app's window ahead in its global
        // list; the active app identity is the authoritative cross-app guard.
        windows[1].z_index = 30;
        assert!(target_is_frontmost_visible_window(
            10,
            Some(target_pid),
            &windows,
        ));

        // A different foreground app blocks the fallback raise.
        assert!(!target_is_frontmost_visible_window(10, Some(200), &windows,));

        // So does another visible window belonging to the active target app.
        windows.push(window(13, target_pid, 40));
        assert!(!target_is_frontmost_visible_window(
            10,
            Some(target_pid),
            &windows,
        ));
    }

    fn move_msg(key: &str, x: f64, y: f64) -> OverlayMsg {
        OverlayMsg::Cmd(KeyedOverlayCommand {
            key: key.to_owned(),
            cmd: OverlayCommand::MoveTo {
                x,
                y,
                end_heading_radians: 0.0,
            },
        })
    }

    #[test]
    fn two_sessions_produce_two_distinct_render_entries() {
        let mut map = empty_map();
        apply_msg(
            &mut map,
            OverlayMsg::Cmd(KeyedOverlayCommand {
                key: "sessA".to_owned(),
                cmd: OverlayCommand::SetEnabled(true),
            }),
        );
        apply_msg(&mut map, move_msg("sessB", 42.0, 24.0));
        // default + sessA + sessB = 3 distinct owned cursors (the core
        // regression today is that they would clobber to one).
        assert_eq!(map.cursors.len(), 3);
        assert!(map.cursors.contains_key("sessA"));
        assert!(map.cursors.contains_key("sessB"));
        assert!(map.cursors.contains_key("default"));
    }

    #[test]
    fn session_end_removes_only_that_session() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("sessA", 10.0, 10.0));
        apply_msg(&mut map, move_msg("sessB", 20.0, 20.0));
        assert_eq!(map.cursors.len(), 3);

        // session_end(A): A gone, B + default retained.
        apply_msg(&mut map, OverlayMsg::Remove("sessA".to_owned()));
        assert!(!map.cursors.contains_key("sessA"));
        assert!(map.cursors.contains_key("sessB"));
        assert!(map.cursors.contains_key("default"));
        assert_eq!(map.cursors.len(), 2);

        // Remove("default") is guarded — default survives.
        apply_msg(&mut map, OverlayMsg::Remove("default".to_owned()));
        assert!(map.cursors.contains_key("default"));

        // Remove of an absent key (anonymous session that never created a
        // cursor) is a harmless no-op.
        let before = map.cursors.len();
        apply_msg(&mut map, OverlayMsg::Remove("never-existed".to_owned()));
        assert_eq!(map.cursors.len(), before);
    }

    #[test]
    fn lazily_created_cursors_inherit_the_selected_theme() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("sessA", 10.0, 10.0));
        apply_msg(&mut map, move_msg("sessB", 20.0, 20.0));
        assert_eq!(
            map.cursors["sessA"].core.cfg.theme_id,
            map.cursors["default"].core.cfg.theme_id
        );
        assert_eq!(
            map.cursors["sessB"].core.cfg.theme_id,
            map.cursors["default"].core.cfg.theme_id
        );
    }

    #[test]
    fn insertion_order_is_stable_z_order() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("first", 1.0, 1.0));
        apply_msg(&mut map, move_msg("second", 2.0, 2.0));
        // Re-touching "first" must NOT move it to the back (IndexMap keeps the
        // original insertion slot), so z-order is stable frame to frame.
        apply_msg(&mut map, move_msg("first", 3.0, 3.0));
        let keys: Vec<&String> = map.cursors.keys().collect();
        assert_eq!(keys, vec!["default", "first", "second"]);
    }

    #[test]
    fn tombstone_blocks_resurrection_after_remove() {
        // The resurrection race: a Cmd for a session lands AFTER its Remove
        // (an in-flight click from another task as the session dies). The
        // tombstone must drop it so the just-removed cursor is NOT re-created.
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("sessA", 10.0, 10.0));
        assert_eq!(map.cursors.len(), 2); // default + sessA

        // Session ends → cursor removed, key tombstoned.
        apply_msg(&mut map, OverlayMsg::Remove("sessA".to_owned()));
        assert!(!map.cursors.contains_key("sessA"));
        assert_eq!(map.cursors.len(), 1);

        // A late in-flight Cmd for the ended session must be dropped WITHOUT
        // re-inserting (no get-or-create resurrection).
        let resolved = apply_msg(&mut map, move_msg("sessA", 99.0, 99.0));
        assert!(
            resolved.is_none(),
            "ended-session Cmd must be dropped, not resolved"
        );
        assert!(
            !map.cursors.contains_key("sessA"),
            "tombstone must block resurrection"
        );
        assert_eq!(
            map.cursors.len(),
            1,
            "render map length must stay at default only"
        );
    }

    #[test]
    fn explicit_revival_clears_tombstone_and_recreates_lazily() {
        let mut map = empty_map();
        apply_msg(&mut map, move_msg("sessA", 10.0, 10.0));
        apply_msg(&mut map, OverlayMsg::Remove("sessA".to_owned()));
        assert!(apply_msg(&mut map, move_msg("sessA", 20.0, 20.0)).is_none());

        apply_msg(&mut map, OverlayMsg::Revive("sessA".to_owned()));
        assert!(!map.cursors.contains_key("sessA"));
        assert!(!map.ended.contains("sessA"));

        let resolved = apply_msg(&mut map, move_msg("sessA", 30.0, 30.0));
        assert_eq!(resolved.as_deref(), Some("sessA"));
        assert!(map.cursors.contains_key("sessA"));
    }

    #[test]
    fn default_is_never_tombstoned() {
        // Remove("default") is guarded, so default is never tombstoned and a
        // subsequent Cmd on default still renders.
        let mut map = empty_map();
        apply_msg(&mut map, OverlayMsg::Remove("default".to_owned()));
        assert!(map.cursors.contains_key("default"));
        assert!(!map.ended.contains("default"));

        let resolved = apply_msg(&mut map, move_msg("default", 5.0, 5.0));
        assert_eq!(resolved.as_deref(), Some("default"));
        assert!(map.cursors.contains_key("default"));
    }

    #[test]
    fn seed_moves_sentinel_cursor_on_screen_for_first_action() {
        // BUG 2 regression: a brand-new session cursor at the sentinel must be
        // seeded on-screen (pos.0 > -50) so the immediately-following MoveTo
        // glides instead of silently snapping via ClickPulse.
        let mut map = empty_map(); // 100x100 frame
                                   // No "sessA" cursor exists yet — the seed must get-or-create it.
        let seeded = seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        assert!(seeded, "sentinel cursor must be seeded");
        let pos = map.cursors["sessA"].core.pos;
        assert!(
            pos.0 > -50.0 && pos.1 > -50.0,
            "seed must be on-screen, got {pos:?}"
        );
        // And it must be a DIFFERENT point from the target so there is a glide.
        assert!(
            (pos.0 - 60.0).abs() > 4.0 || (pos.1 - 60.0).abs() > 4.0,
            "seed must differ from target to produce a visible glide, got {pos:?}"
        );
    }

    #[test]
    fn seed_is_noop_when_cursor_already_on_screen() {
        // A second action: the cursor already landed somewhere on-screen, so the
        // seed must NOT move it (the MoveTo path should start from where it is).
        let mut map = empty_map();
        // Put sessA on-screen first.
        seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        map.cursors
            .get_mut("sessA")
            .unwrap()
            .core
            .set_position((30.0, 30.0));
        let seeded_again = seed_start_in_map(&mut map, &"sessA".to_owned(), 80.0, 80.0);
        assert!(!seeded_again, "on-screen cursor must not be re-seeded");
        assert_eq!(
            map.cursors["sessA"].core.pos,
            (30.0, 30.0),
            "pos must be untouched"
        );
    }

    #[test]
    fn seed_does_not_resurrect_ended_session() {
        // The seed shares the resurrection guard: it must not re-create a cursor
        // whose session already ended.
        let mut map = empty_map();
        map.ended.insert("sessA".to_owned());
        let seeded = seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        assert!(!seeded, "ended session must not be seeded");
        assert!(
            !map.cursors.contains_key("sessA"),
            "ended session must not be resurrected"
        );
    }

    #[test]
    fn sentinel_default_cursor_does_not_require_frame_ticks() {
        // Regression for idle CPU: a freshly-started serve daemon seeds only the
        // off-screen default cursor. With no commands in flight, the render loop
        // should be able to block on rx.recv() instead of repainting at 60fps.
        let map = empty_map();
        assert!(!render_map_needs_frame_tick(&map));
    }

    #[test]
    fn only_enabled_on_screen_cursor_is_externally_visible() {
        let mut map = empty_map();
        assert!(!cursor_is_externally_visible(
            &map.layout,
            &map.cursors["default"]
        ));

        seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        assert!(cursor_is_externally_visible(
            &map.layout,
            &map.cursors["sessA"]
        ));

        map.cursors.get_mut("sessA").unwrap().core.cfg.enabled = false;
        assert!(!cursor_is_externally_visible(
            &map.layout,
            &map.cursors["sessA"]
        ));
    }

    fn reported_dual_screen_map() -> RenderMap {
        let mut map = empty_map();
        map.layout.replace(vec![
            DisplayGeometry::new(
                1,
                [0.0, 0.0, 1728.0, 1117.0],
                [0.0, 0.0, 1728.0, 1117.0],
                2.0,
            )
            .unwrap(),
            DisplayGeometry::new(
                3,
                [-192.0, 1117.0, 1920.0, 1080.0],
                [-192.0, -1080.0, 1920.0, 1080.0],
                2.0,
            )
            .unwrap(),
        ]);
        map
    }

    #[test]
    fn secondary_display_seed_and_pixels_are_not_clamped_to_primary() {
        let mut map = reported_dual_screen_map();
        let key = "external".to_owned();
        assert!(seed_start_in_map(&mut map, &key, 768.0, -540.0));
        let seed = map.cursors[&key].core.pos;
        assert!(map.layout.displays[1].contains(seed));
        assert!(cursor_is_externally_visible(
            &map.layout,
            &map.cursors[&key]
        ));
        assert!(
            !seed_start_in_map(&mut map, &key, 900.0, -500.0),
            "negative initialized positions must not be reseeded every action"
        );
        let primary = render_display(&map, &map.layout.displays[0]).unwrap();
        let external = render_display(&map, &map.layout.displays[1]).unwrap();
        assert!(primary.data().iter().all(|value| *value == 0));
        assert!(external.data().chunks_exact(4).any(|pixel| pixel[3] > 96));
    }

    #[test]
    fn unplugging_a_display_preserves_session_ownership_and_reseeds_next_action() {
        let mut map = reported_dual_screen_map();
        let key = "external".to_owned();
        seed_start_in_map(&mut map, &key, 768.0, -540.0);
        apply_msg(&mut map, OverlayMsg::Remove("ended".to_owned()));
        let before = map.cursors[&key].core.pos;
        map.layout.replace(vec![map.layout.displays[0].clone()]);
        apply_render_event(&mut map, RenderEvent::DisplaysChanged);
        assert_eq!(
            map.cursors[&key].core.pos, before,
            "layout events do not mutate cursor ownership/state"
        );
        assert!(!cursor_is_externally_visible(
            &map.layout,
            &map.cursors[&key]
        ));
        assert!(seed_start_in_map(&mut map, &key, 800.0, 500.0));
        assert!(cursor_is_externally_visible(
            &map.layout,
            &map.cursors[&key]
        ));
        assert!(!seed_start_in_map(
            &mut map,
            &"ended".to_owned(),
            800.0,
            500.0
        ));
        assert!(map.ended.contains("ended"));
    }

    #[test]
    fn rendering_multiple_displays_does_not_tick_or_finish_an_animation_twice() {
        let mut map = empty_map();
        let secondary = DisplayGeometry::new(
            3,
            [-100.0, 0.0, 100.0, 100.0],
            [-100.0, 0.0, 100.0, 100.0],
            2.0,
        )
        .unwrap();
        map.layout
            .replace(vec![map.layout.displays[0].clone(), secondary]);
        let key = "crossing".to_owned();
        seed_start_in_map(&mut map, &key, -60.0, 40.0);
        apply_msg(&mut map, move_msg(&key, 60.0, 40.0));
        map.cursors.get_mut(&key).unwrap().tick(0.016);
        let before = (map.cursors[&key].core.pos, map.cursors[&key].core.dist);
        for display in &map.layout.displays {
            render_display(&map, display).unwrap();
        }
        assert_eq!(
            (map.cursors[&key].core.pos, map.cursors[&key].core.dist),
            before
        );
        assert!(map.cursors[&key].core.path.is_some());
    }

    #[test]
    fn active_or_fading_cursor_requires_frame_ticks() {
        let mut map = empty_map();
        seed_start_in_map(&mut map, &"sessA".to_owned(), 60.0, 60.0);
        apply_msg(&mut map, move_msg("sessA", 80.0, 80.0));
        assert!(
            render_map_needs_frame_tick(&map),
            "planned path should tick"
        );

        let rs = map.cursors.get_mut("sessA").unwrap();
        rs.core.path = None;
        rs.core.spring = None;
        rs.core.click_t = None;
        rs.focus_rect = None;
        rs.core.idle_alpha = 0.0;
        assert!(
            !render_map_needs_frame_tick(&map),
            "fully hidden idle cursor should quiesce"
        );
    }

    #[test]
    fn per_key_arrival_isolation() {
        // Two concurrent waiters keyed A and B; firing A must not cancel B.
        // This mirrors the ARRIVAL_TX HashMap logic in isolation (no statics).
        let mut waiters: HashMap<CursorKey, tokio::sync::oneshot::Sender<()>> = HashMap::new();
        let (txa, mut rxa) = tokio::sync::oneshot::channel::<()>();
        let (txb, mut rxb) = tokio::sync::oneshot::channel::<()>();
        waiters.insert("A".to_owned(), txa);
        waiters.insert("B".to_owned(), txb);

        // Fire A's arrival.
        if let Some(tx) = waiters.remove("A") {
            let _ = tx.send(());
        }
        // A resolved, B still pending.
        assert!(matches!(rxa.try_recv(), Ok(())));
        assert!(matches!(
            rxb.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn async_enqueue_supersedes_only_after_command_acceptance() {
        let mut waiters: Option<HashMap<CursorKey, tokio::sync::oneshot::Sender<()>>> =
            Some(HashMap::new());
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        waiters.as_mut().unwrap().insert("session".to_owned(), tx);

        assert!(take_superseded_arrival(&mut waiters, &"session".to_owned(), false).is_none());
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        let sender = take_superseded_arrival(&mut waiters, &"session".to_owned(), true)
            .expect("accepted async move must supersede the old waiter");
        let _ = sender.send(());
        assert!(matches!(rx.try_recv(), Ok(())));
    }
}
