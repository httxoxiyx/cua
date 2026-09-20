//! Native macOS Picture-in-Picture stack for Computer Use.
//!
//! Exact native windows are captured continuously, one app owns one bounded
//! card, and all cards live inside one borderless native stack. The daemon is
//! in-process, so it does not need Codex's cross-process CAContext transport.

mod window_activation;

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pip_preview::temporary_activation::{ActivationTarget, TemporaryActivationTracker};
use pip_preview::{
    PipBackend, PipBackendFactory, PipConfig, PipFrame, PipGeometry, PipModelChange, PipViewModel,
    MAX_VISIBLE_PIP_CARDS,
};
use screencapturekit::prelude::{
    CMSampleBufferExt, CMSampleBufferSCExt, CMTime, SCContentFilter, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamOutputType,
};

#[repr(C)]
struct CGColor {
    _opaque: [u8; 0],
}

#[repr(C)]
struct NativeCGImage {
    _opaque: [u8; 0],
}

unsafe impl objc2::RefEncode for CGColor {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("CGColor", &[]));
}

unsafe impl objc2::RefEncode for NativeCGImage {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("CGImage", &[]));
}

struct NativeHandles {
    window: usize,
    canvas: usize,
    delegate: usize,
    global_mouse_monitor: usize,
    local_mouse_monitor: usize,
}

#[derive(Clone, Copy)]
struct NativeCardHandles {
    card: usize,
    image_view: usize,
    controls: usize,
    resting_rect: CardRect,
    layout_rect: CardRect,
    has_live_frame: bool,
}

struct LiveStreamEntry {
    target_pid: i64,
    window_id: u64,
    stream: Option<SCStream>,
    resize: LiveCaptureResizeState,
    cancelled: Arc<AtomicBool>,
    frame_pending: Arc<AtomicBool>,
    retry_at: Option<Instant>,
}

#[derive(Clone)]
struct ObservationRetry {
    target: pip_preview::PipTarget,
    generation: u64,
    retry_at_ms: u64,
    error: String,
    attempts: u32,
}

#[derive(Default)]
struct LiveCaptureResizeState {
    configured: Option<(u32, u32)>,
    pending: Option<(u32, u32)>,
}

impl LiveCaptureResizeState {
    fn begin(&mut self, requested: (u32, u32)) -> bool {
        if self.pending.is_some() || self.configured == Some(requested) {
            return false;
        }
        self.pending = Some(requested);
        true
    }

    fn finish(&mut self, requested: (u32, u32), succeeded: bool) {
        if self.pending != Some(requested) {
            return;
        }
        if succeeded {
            self.configured = Some(requested);
        }
        self.pending = None;
    }
}

struct LiveFrame {
    app_pid: i64,
    target_pid: i64,
    window_id: u64,
    image: LiveFrameImage,
    frame_pending: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone)]
struct ClickedTarget {
    target: pip_preview::PipTarget,
    layout_rect: CardRect,
}

struct ForegroundVisibilityState {
    suppressed_pids: HashSet<i64>,
    epoch: u64,
    temporary_activations: TemporaryActivationTracker,
    activation_lifetime: u64,
}

impl Default for ForegroundVisibilityState {
    fn default() -> Self {
        Self {
            suppressed_pids: HashSet::new(),
            epoch: next_foreground_visibility_epoch(),
            temporary_activations: TemporaryActivationTracker::default(),
            activation_lifetime: next_foreground_visibility_epoch(),
        }
    }
}

impl ForegroundVisibilityState {
    fn invalidate_pending(&mut self) {
        self.epoch = next_foreground_visibility_epoch();
    }
}

fn next_foreground_visibility_epoch() -> u64 {
    // Do not reuse an epoch after a PiP backend shutdown/reinitialization.
    // The state mutex protects contents; this counter only allocates identities.
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A visibility-only lease for an input helper's activate/action/restore scope.
/// It grants no input or preview authority and never changes OS focus itself.
/// Drop also invalidates samples taken while the temporary activation was live.
pub(crate) struct TemporaryActivationGuard(Option<(u64, u64)>);

fn finish_temporary_activation(
    state: &mut ForegroundVisibilityState,
    lease: Option<(u64, u64)>,
) -> bool {
    let changed = lease.is_some_and(|(lifetime, token)| {
        lifetime == state.activation_lifetime && state.temporary_activations.end(token)
    });
    if changed {
        state.invalidate_pending();
    }
    changed
}

impl Drop for TemporaryActivationGuard {
    fn drop(&mut self) {
        let changed = {
            let mut state = FOREGROUND_VISIBILITY_STATE.lock().unwrap();
            finish_temporary_activation(&mut state, self.0)
        };
        if changed {
            LAST_FOREGROUND_CHECK_MS.store(0, Ordering::Release);
            schedule_foreground_visibility_refresh();
        }
    }
}

fn activation_target(target: &pip_preview::PipTarget) -> ActivationTarget {
    ActivationTarget {
        app_pid: target.app_key_pid(),
        pid: target.pid,
        window_id: target.window_id,
    }
}

pub(crate) fn begin_temporary_activation(
    target_pid: i32,
    window_id: u32,
    logical_host_pid: Option<i32>,
) -> TemporaryActivationGuard {
    let monitors_ready = HANDLES.lock().unwrap().as_ref().is_some_and(|handles| {
        temporary_activation_monitors_ready(
            handles.global_mouse_monitor,
            handles.local_mouse_monitor,
        )
    });
    if !monitors_ready {
        // Without user-input observation, a same-app takeover cannot revoke
        // the hold. Preserve the ordinary foreground hiding behavior instead.
        return TemporaryActivationGuard(None);
    }
    let target = ActivationTarget {
        app_pid: i64::from(logical_host_pid.unwrap_or(target_pid)),
        pid: i64::from(target_pid),
        window_id: u64::from(window_id),
    };
    // Never create/show a card just because an action is being assisted. Only
    // preserve an existing exact preview, still subject to normal authority.
    let exact_frame_exists = VIEW_MODEL
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|model| model.frame_for_app(target.app_pid))
        .is_some_and(|frame| activation_target(&frame.target) == target);
    let was_visible = exact_frame_exists
        && CARD_HANDLES.lock().unwrap().contains_key(&target.app_pid)
        && !HIDDEN_APPS.lock().unwrap().contains(&target.app_pid);
    let previous = crate::apps::frontmost_pid().map(i64::from);
    let mut state = FOREGROUND_VISIBILITY_STATE.lock().unwrap();
    let was_visible = was_visible && !state.suppressed_pids.contains(&target.app_pid);
    let token = state
        .temporary_activations
        .begin(target, previous, was_visible, monotonic_ms());
    if token.is_some() {
        TEMPORARY_ACTIVATION_STARTS.fetch_add(1, Ordering::Relaxed);
        state.invalidate_pending();
    }
    TemporaryActivationGuard(token.map(|token| (state.activation_lifetime, token)))
}

fn preserve_temporarily_activated_cards(
    state: &mut ForegroundVisibilityState,
    snapshot: &[PipFrame],
    foreground_pid: Option<i32>,
    suppressed: &mut HashSet<i64>,
    now: u64,
) {
    state
        .temporary_activations
        .observe_foreground(foreground_pid.map(i64::from), now);
    // Keep the original foreground card hidden as well: otherwise a temporary
    // assist can reveal it and reorder the stack until focus is restored.
    for pid in state.temporary_activations.original_foreground_pids(now) {
        if let Some(app_pid) = i32::try_from(pid)
            .ok()
            .and_then(|pid| snapshot_candidate_pid(snapshot, Some(pid)))
        {
            suppressed.insert(app_pid);
        }
    }
    for frame in snapshot {
        if state
            .temporary_activations
            .keeps_visible(activation_target(&frame.target), now)
        {
            suppressed.remove(&frame.target.app_key_pid());
        }
    }
}

fn temporary_activation_monitors_ready(global: usize, local: usize) -> bool {
    global != 0 && local != 0
}

struct ForegroundVisibilityWatcher {
    cancelled: Arc<AtomicBool>,
}

struct ForegroundVisibilityRefresh {
    observed_epoch: u64,
    suppressed_pids: HashSet<i64>,
    candidates_changed: bool,
}

#[derive(Clone)]
struct DelegationFrameProof {
    target_pid: i64,
    window_id: u64,
    epoch: u64,
    publication_generation: u64,
    session_id: Option<String>,
}

#[derive(Clone)]
struct PipPublicationReservation {
    generation: u64,
    session_id: Option<String>,
    target_pid: i64,
    window_id: u64,
    target_epoch: u64,
    awaiting_frame: bool,
    pending_observation: Option<pip_preview::PipTarget>,
    menu_image: Option<crate::ax::application_menu::ApplicationMenuImage>,
}

struct VerifiedPipFrame {
    frame: PipFrame,
    publication_generation: u64,
}

#[derive(Default)]
struct DelegationProofState {
    target_epochs: HashMap<(i64, u64), u64>,
    frame_proofs: HashMap<i64, DelegationFrameProof>,
    latest_publications: HashMap<i64, PipPublicationReservation>,
}

#[derive(Clone)]
struct CardGesture {
    target: Option<ClickedTarget>,
    front_pid: Option<i64>,
    start_mouse: objc2_foundation::NSPoint,
    start_window_origin: objc2_foundation::NSPoint,
    dragged: bool,
}

enum LiveFrameImage {
    CgImage(screencapturekit::CGImage),
}

static HANDLES: Mutex<Option<NativeHandles>> = Mutex::new(None);
static CARD_HANDLES: LazyLock<Mutex<HashMap<i64, NativeCardHandles>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// AppKit-only presentation identities. Pixel updates do not change this list.
static RENDERED_CARDS: Mutex<Vec<(pip_preview::PipTarget, CardRect)>> = Mutex::new(Vec::new());
static CARD_VIEW_REBUILDS: AtomicU64 = AtomicU64::new(0);
static CARD_VIEW_REUSES: AtomicU64 = AtomicU64::new(0);
static TEMPORARY_ACTIVATION_STARTS: AtomicU64 = AtomicU64::new(0);
static EXTERNAL_INPUT_HOLD_CANCELLATIONS: AtomicU64 = AtomicU64::new(0);
static CARD_VIEW_PIDS: LazyLock<Mutex<HashMap<usize, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RESIZE_VIEW_DIRECTIONS: LazyLock<Mutex<HashMap<usize, isize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static VIEW_MODEL: Mutex<Option<PipViewModel>> = Mutex::new(None);
static HIDDEN_APPS: LazyLock<Mutex<HashSet<i64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static HOVERED_APP: Mutex<Option<i64>> = Mutex::new(None);
static CARD_GESTURE: Mutex<Option<CardGesture>> = Mutex::new(None);
static CURSOR_REFRESH_PENDING: AtomicBool = AtomicBool::new(false);
static FOREGROUND_REFRESH_PENDING: AtomicBool = AtomicBool::new(false);
static LAST_FOREGROUND_CHECK_MS: AtomicU64 = AtomicU64::new(0);
static FOREGROUND_CLOCK_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
static FOREGROUND_VISIBILITY_STATE: LazyLock<Mutex<ForegroundVisibilityState>> =
    LazyLock::new(|| Mutex::new(ForegroundVisibilityState::default()));
static FOREGROUND_VISIBILITY_WATCHER: Mutex<Option<ForegroundVisibilityWatcher>> = Mutex::new(None);
static LIVE_STREAMS: LazyLock<Mutex<HashMap<i64, LiveStreamEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static DELEGATION_PROOF_STATE: LazyLock<Mutex<DelegationProofState>> =
    LazyLock::new(|| Mutex::new(DelegationProofState::default()));
static NEXT_PIP_PUBLICATION_GENERATION: AtomicU64 = AtomicU64::new(1);
static CARD_ACTIVATION_GENERATION: AtomicU64 = AtomicU64::new(0);
static LAST_CARD_ACTIVATION: Mutex<Option<serde_json::Value>> = Mutex::new(None);
static OBSERVATION_RETRIES: LazyLock<Mutex<HashMap<i64, ObservationRetry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const LIVE_CAPTURE_FPS: i32 = 12;
const LIVE_CAPTURE_MAX_SIDE: f64 = 960.0;
const LIVE_CAPTURE_WINDOW_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const FOREGROUND_VISIBILITY_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const PREVIEW_RETRY_INTERVAL_MS: u64 = 1_000;
const STACK_INSET: f64 = 8.0;
const STACK_CARD_OFFSET_X: f64 = 16.0;
const STACK_CARD_OFFSET_Y: f64 = 18.0;
const STACK_MIN_CARD_HEIGHT: f64 = 120.0;
const STACK_HOVER_LIFT_X: f64 = 8.0;
const STACK_HOVER_LIFT_Y: f64 = 5.0;
const CARD_RADIUS: f64 = 12.0;
const CONTROL_SIZE: f64 = 24.0;
const RESIZE_HIT_INSET: f64 = 20.0;
const DEFAULT_SCREEN_FRACTION: f64 = 0.20;
const MINIMUM_PIP_WIDTH: f64 = 280.0;
const MINIMUM_PIP_HEIGHT: f64 = 180.0;
const MAXIMUM_DEFAULT_PIP_WIDTH: f64 = 480.0;
const MAXIMUM_DEFAULT_PIP_HEIGHT: f64 = 300.0;
const CLICK_DRAG_THRESHOLD: f64 = 4.0;
const PIP_WINDOW_LEVEL: i64 = 3;

const RESIZE_LEFT: isize = 1;
const RESIZE_RIGHT: isize = 2;
const RESIZE_BOTTOM: isize = 4;
const RESIZE_TOP: isize = 8;

#[link(name = "dispatch", kind = "dylib")]
extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
    fn dispatch_sync_f(
        queue: *const c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}

fn dispatch_to_main<T: Send + 'static>(payload: T, cb: unsafe extern "C" fn(*mut c_void)) {
    let boxed = Box::new(payload);
    unsafe {
        let main_queue = &raw const _dispatch_main_q as *const c_void;
        dispatch_async_f(main_queue, Box::into_raw(boxed) as *mut c_void, cb);
    }
}

fn dispatch_to_main_sync<T>(payload: T, cb: unsafe extern "C" fn(*mut c_void)) {
    let context = Box::into_raw(Box::new(payload)) as *mut c_void;
    unsafe {
        if libc::pthread_main_np() != 0 {
            cb(context);
        } else {
            let main_queue = &raw const _dispatch_main_q as *const c_void;
            dispatch_sync_f(main_queue, context, cb);
        }
    }
}

pub struct MacosPipBackend;

impl PipBackend for MacosPipBackend {
    fn push_frame(&self, mut frame: PipFrame) {
        let app_pid = frame.target.app_key_pid();
        if frame.target.pid <= 0
            || frame.target.window_id == 0
            || !pip_target_session_is_live(&frame.target)
            || HIDDEN_APPS.lock().unwrap().contains(&app_pid)
        {
            return;
        }
        let publication_generation = {
            let mut model = VIEW_MODEL.lock().unwrap();
            let model = model.get_or_insert_with(|| PipViewModel::new(MAX_VISIBLE_PIP_CARDS));
            let change = model.select_target(&frame.target);
            let generation = reserve_pip_publication(&frame.target);
            if !change.is_empty() {
                dispatch_to_main(change, apply_model_change_cb);
            }
            generation
        };
        let target_for_failure = frame.target.clone();
        if std::thread::Builder::new()
            .name(format!("cua-pip-seed-{app_pid}"))
            .spawn(move || {
                if !pip_target_session_is_live(&frame.target)
                    || !pip_target_is_selected(&frame.target)
                    || !pip_publication_is_current(&frame.target, publication_generation)
                    || !pip_delegation_is_live(&frame.target)
                {
                    cancel_pip_publication(&frame.target, publication_generation);
                    return;
                }

                let Ok(pid) = i32::try_from(frame.target.pid) else {
                    cancel_pip_publication(&frame.target, publication_generation);
                    return;
                };
                let window = if is_application_menu_target(&frame.target) {
                    // The proof above, not an app-wide window union, authorizes
                    // this one accessory-layer menu source.
                    u32::try_from(frame.target.window_id)
                        .ok()
                        .and_then(crate::windows::window_info_by_id)
                        .filter(|window| window.pid == pid)
                } else {
                    crate::windows::all_windows().into_iter().find(|window| {
                        window.pid == pid && u64::from(window.window_id) == frame.target.window_id
                    })
                };
                let Some(window) = window else {
                    dispatch_to_main(
                        (
                            frame.target.clone(),
                            publication_generation,
                            "preview window metadata unavailable".to_owned(),
                        ),
                        fail_observation_preview_cb,
                    );
                    return;
                };
                frame.target.app_name = i32::try_from(app_pid)
                    .ok()
                    .and_then(crate::apps::get_app_name_for_pid)
                    .unwrap_or(window.app_name);
                frame.target.window_title =
                    (!window.title.trim().is_empty()).then_some(window.title);
                if frame.target.app_name.trim().is_empty() {
                    frame.target.app_name = format!("App {app_pid}");
                }
                if !register_delegation_frame_proof(&frame.target, publication_generation) {
                    cancel_pip_publication(&frame.target, publication_generation);
                    return;
                }

                dispatch_to_main(
                    VerifiedPipFrame {
                        frame,
                        publication_generation,
                    },
                    push_frame_cb,
                );
            })
            .is_err()
        {
            dispatch_to_main(
                (
                    target_for_failure,
                    publication_generation,
                    "preview worker could not start".to_owned(),
                ),
                fail_observation_preview_cb,
            );
        }
    }

    fn ensure_target(&self, target: pip_preview::PipTarget) {
        let app_pid = target.app_key_pid();
        if target.pid <= 0
            || target.window_id == 0
            || !pip_target_session_is_live(&target)
            || HIDDEN_APPS.lock().unwrap().contains(&app_pid)
        {
            return;
        }

        let seed_policy = {
            let model = VIEW_MODEL.lock().unwrap();
            pip_target_seed_policy(
                model
                    .as_ref()
                    .and_then(|model| model.frame_for_app(app_pid)),
                &target,
            )
        };
        if seed_policy == PipTargetSeedPolicy::ReuseObservationAndLiveStream {
            // The observation seed already started native live capture. The
            // stream owns subsequent frames, so there is nothing to recapture.
            return;
        }
        // Generic ensures include mutations and cannot authorize a new seed.
        // A successful observation either supplies its image or uses the
        // distinct observe_target path to authorize a private preview.
        tracing::debug!(
            target: "pip",
            pid = target.pid,
            window_id = target.window_id,
            "PiP target has no observation seed yet; retaining existing cards"
        );
    }

    fn observe_target(&self, target: pip_preview::PipTarget) {
        if target.pid <= 0
            || target.window_id == 0
            || !pip_target_session_is_live(&target)
            || HIDDEN_APPS.lock().unwrap().contains(&target.app_key_pid())
        {
            return;
        }
        let generation = {
            let mut model = VIEW_MODEL.lock().unwrap();
            let model = model.get_or_insert_with(|| PipViewModel::new(MAX_VISIBLE_PIP_CARDS));
            let change = model.select_target(&target);
            let generation = reserve_pip_observation(&target);
            if !change.is_empty() {
                dispatch_to_main(change, apply_model_change_cb);
            }
            generation
        };
        let Some(generation) = generation else {
            return;
        };
        spawn_observation_capture(target, generation);
    }

    fn end_session(&self, session_id: &str) {
        OBSERVATION_RETRIES
            .lock()
            .unwrap()
            .retain(|_, retry| retry.target.session_id.as_deref() != Some(session_id));
        remove_session_delegation_frame_proofs(session_id);
        dispatch_to_main(session_id.to_owned(), end_session_cb);
    }

    fn set_input_passthrough(&self, passthrough: bool) -> anyhow::Result<()> {
        dispatch_to_main_sync(passthrough, set_input_passthrough_cb);
        Ok(())
    }

    fn shutdown(self: Box<Self>) {
        CARD_ACTIVATION_GENERATION.fetch_add(1, Ordering::AcqRel);
        stop_foreground_visibility_watcher();
        stop_all_live_capture();
        *DELEGATION_PROOF_STATE.lock().unwrap() = DelegationProofState::default();
        dispatch_to_main((), shutdown_cb);
    }
}

fn exact_target_matches(frame: &PipFrame, target: &pip_preview::PipTarget) -> bool {
    frame.target.app_key_pid() == target.app_key_pid()
        && frame.target.pid == target.pid
        && frame.target.window_id == target.window_id
}

fn pip_target_is_selected(target: &pip_preview::PipTarget) -> bool {
    VIEW_MODEL
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|model| model.accepts_target(target))
}

fn spawn_observation_capture(target: pip_preview::PipTarget, generation: u64) {
    let failed_target = target.clone();
    if std::thread::Builder::new()
        .name(format!("cua-pip-observe-{}", target.app_key_pid()))
        .spawn(move || {
            if let Err(error) = refresh_observed_preview(&target, generation) {
                dispatch_to_main(
                    (target, generation, error.to_string()),
                    fail_observation_preview_cb,
                );
            }
        })
        .is_err()
    {
        dispatch_to_main(
            (
                failed_target,
                generation,
                "preview worker could not start".to_owned(),
            ),
            fail_observation_preview_cb,
        );
    }
}

/// A target switch is a lifecycle change, not a tool-end animation. Keep the
/// current app's card/stream across calls, and retire only abandoned claims.
unsafe extern "C" fn apply_model_change_cb(ctx: *mut c_void) {
    let change = *Box::from_raw(ctx as *mut PipModelChange);
    apply_model_change(&change);
    render_snapshot(&current_snapshot());
}

fn apply_model_change(change: &PipModelChange) {
    for &pid in &change.removed_pids {
        if VIEW_MODEL
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|model| model.frame_for_app(pid).is_some())
        {
            continue; // A newer publication already replaced this queued removal.
        }
        remove_expired_card_proof(pid);
        stop_live_capture_for(pid);
    }
    for &pid in &change.changed_pids {
        republish_retained_frame(pid);
    }
    let mut model = VIEW_MODEL.lock().unwrap();
    if let Some(model) = model.as_mut() {
        OBSERVATION_RETRIES.lock().unwrap().retain(|_, retry| {
            model.accepts_target(&retry.target) && pip_target_session_is_live(&retry.target)
        });
    }
}

/// Restoring another session's retained frame is not a new observation. Reserve
/// only while it is still the selected representative, and never supersede an
/// observation whose full-image or tree-only publication is still in flight.
fn republish_retained_frame(app_pid: i64) {
    let (frame, generation) = {
        let model = VIEW_MODEL.lock().unwrap();
        let Some(model) = model.as_ref() else {
            return;
        };
        let Some(frame) = model.frame_for_app(app_pid) else {
            return;
        };
        if !model.accepts_target(&frame.target) || !pip_target_session_is_live(&frame.target) {
            return;
        }
        let current = {
            let state = DELEGATION_PROOF_STATE.lock().unwrap();
            if state
                .latest_publications
                .get(&app_pid)
                .is_some_and(|reservation| reservation.awaiting_frame)
            {
                return;
            }
            state
                .latest_publications
                .get(&app_pid)
                .map(|reservation| reservation.generation)
        };
        if current.is_some_and(|generation| pip_publication_is_current(&frame.target, generation))
            && delegation_frame_proof_is_live(&frame.target)
        {
            // A queued model change may already have been superseded by a
            // successful publication. Keep its existing views and live stream.
            return;
        }
        let generation = reserve_pip_publication(&frame.target);
        (frame.clone(), generation)
    };
    let failed_target = frame.target.clone();
    if std::thread::Builder::new()
        .name(format!("cua-pip-restore-{app_pid}"))
        .spawn(move || {
            // Native window/AX validation must not block AppKit's main queue.
            if !pip_target_session_is_live(&frame.target)
                || !pip_target_is_selected(&frame.target)
                || !pip_publication_is_current(&frame.target, generation)
                || !physical_target_is_live(frame.target.pid, frame.target.window_id)
                || !pip_delegation_is_live(&frame.target)
                || !register_delegation_frame_proof(&frame.target, generation)
            {
                cancel_pip_publication(&frame.target, generation);
                return;
            }
            dispatch_to_main(
                VerifiedPipFrame {
                    frame,
                    publication_generation: generation,
                },
                push_frame_cb,
            );
        })
        .is_err()
    {
        cancel_pip_publication(&failed_target, generation);
    }
}

pub(crate) fn invalidate_app_context_target(target: crate::ax::app_context::AppContextTarget) {
    let key = (i64::from(target.pid), u64::from(target.window_id));
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    let epoch = state.target_epochs.entry(key).or_default();
    *epoch = epoch.wrapping_add(1).max(1);
}

#[cfg(test)]
fn delegation_target_epoch(target: &pip_preview::PipTarget) -> u64 {
    DELEGATION_PROOF_STATE
        .lock()
        .unwrap()
        .target_epochs
        .get(&(target.pid, target.window_id))
        .copied()
        .unwrap_or(0)
}

fn reserve_pip_publication(target: &pip_preview::PipTarget) -> u64 {
    let generation = NEXT_PIP_PUBLICATION_GENERATION.fetch_add(1, Ordering::AcqRel);
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    let target_epoch = state
        .target_epochs
        .get(&(target.pid, target.window_id))
        .copied()
        .unwrap_or(0);
    state.latest_publications.insert(
        target.app_key_pid(),
        PipPublicationReservation {
            generation,
            session_id: target.session_id.clone(),
            target_pid: target.pid,
            window_id: target.window_id,
            target_epoch,
            awaiting_frame: true,
            pending_observation: None,
            menu_image: None,
        },
    );
    generation
}

fn same_observation_target(left: &pip_preview::PipTarget, right: &pip_preview::PipTarget) -> bool {
    left.pid == right.pid
        && left.window_id == right.window_id
        && left.logical_pid == right.logical_pid
        && left.delegation == right.delegation
        && left.session_id == right.session_id
}

fn reserve_pip_observation(target: &pip_preview::PipTarget) -> Option<u64> {
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    if state
        .latest_publications
        .get(&target.app_key_pid())
        .and_then(|reservation| reservation.pending_observation.as_ref())
        .is_some_and(|pending| same_observation_target(pending, target))
    {
        return None;
    }
    let generation = NEXT_PIP_PUBLICATION_GENERATION.fetch_add(1, Ordering::AcqRel);
    // Keep the same menu card live while its next exact-host observation is
    // resolving. The stored proof is still revalidated before every display.
    let previous_menu = state
        .latest_publications
        .get(&target.app_key_pid())
        .filter(|previous| previous.session_id == target.session_id)
        .and_then(|previous| previous.menu_image.as_ref())
        .filter(|menu| {
            target.delegation.is_none()
                && target.logical_pid.is_none()
                && i64::from(menu.pid) == target.pid
                && u64::from(menu.document_window_id) == target.window_id
        })
        .cloned();
    let target_epoch = state
        .target_epochs
        .get(&(target.pid, target.window_id))
        .copied()
        .unwrap_or(0);
    state.latest_publications.insert(
        target.app_key_pid(),
        PipPublicationReservation {
            generation,
            session_id: target.session_id.clone(),
            target_pid: target.pid,
            window_id: target.window_id,
            target_epoch,
            awaiting_frame: true,
            pending_observation: Some(target.clone()),
            menu_image: previous_menu,
        },
    );
    Some(generation)
}

fn finish_pip_observation(target: &pip_preview::PipTarget, generation: u64) {
    if let Some(reservation) = DELEGATION_PROOF_STATE
        .lock()
        .unwrap()
        .latest_publications
        .get_mut(&target.app_key_pid())
        .filter(|reservation| reservation.generation == generation)
    {
        reservation.awaiting_frame = false;
        reservation.pending_observation = None;
    }
}

fn clear_completed_observation_retry(app_pid: i64, generation: u64) {
    OBSERVATION_RETRIES
        .lock()
        .unwrap()
        .retain(|pid, retry| *pid != app_pid || retry.generation > generation);
}

fn select_observation_preview_source(
    observed: &pip_preview::PipTarget,
    source: &pip_preview::PipTarget,
    menu: Option<crate::ax::application_menu::ApplicationMenuImage>,
    generation: u64,
) -> bool {
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    let source_epoch = state
        .target_epochs
        .get(&(source.pid, source.window_id))
        .copied()
        .unwrap_or(0);
    let Some(reservation) = state
        .latest_publications
        .get_mut(&observed.app_key_pid())
        .filter(|reservation| {
            reservation.generation == generation
                && reservation
                    .pending_observation
                    .as_ref()
                    .is_some_and(|pending| same_observation_target(pending, observed))
        })
    else {
        return false;
    };
    reservation.target_pid = source.pid;
    reservation.window_id = source.window_id;
    reservation.target_epoch = source_epoch;
    // This is PiP-only proof. It never changes model screenshot coordinates,
    // resize mappings, or the application-menu pointer-scope registry.
    reservation.menu_image = menu;
    true
}

fn pip_publication_is_current(target: &pip_preview::PipTarget, generation: u64) -> bool {
    let state = DELEGATION_PROOF_STATE.lock().unwrap();
    let current_epoch = state
        .target_epochs
        .get(&(target.pid, target.window_id))
        .copied()
        .unwrap_or(0);
    state
        .latest_publications
        .get(&target.app_key_pid())
        .is_some_and(|reservation| {
            reservation.generation == generation
                && reservation.session_id == target.session_id
                && reservation.target_pid == target.pid
                && reservation.window_id == target.window_id
                && (target.delegation.is_none() || reservation.target_epoch == current_epoch)
        })
}

fn cancel_pip_publication(target: &pip_preview::PipTarget, generation: u64) {
    let app_pid = target.app_key_pid();
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    if state
        .latest_publications
        .get(&app_pid)
        .is_some_and(|reservation| reservation.generation == generation)
    {
        state.latest_publications.remove(&app_pid);
        state.frame_proofs.remove(&app_pid);
    }
}

fn register_delegation_frame_proof(
    target: &pip_preview::PipTarget,
    publication_generation: u64,
) -> bool {
    let app_pid = target.app_key_pid();
    let Some(_) = target.delegation.as_ref() else {
        let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
        let current = state
            .latest_publications
            .get(&app_pid)
            .is_some_and(|reservation| {
                reservation.generation == publication_generation
                    && reservation.session_id == target.session_id
                    && reservation.target_pid == target.pid
                    && reservation.window_id == target.window_id
            });
        if current {
            state.frame_proofs.remove(&app_pid);
        }
        return current;
    };
    if !pip_delegation_is_live(target) {
        return false;
    }
    commit_delegation_frame_proof(target, publication_generation)
}

fn commit_delegation_frame_proof(
    target: &pip_preview::PipTarget,
    publication_generation: u64,
) -> bool {
    let app_pid = target.app_key_pid();
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    let current_epoch = state
        .target_epochs
        .get(&(target.pid, target.window_id))
        .copied()
        .unwrap_or(0);
    let publication_current = state
        .latest_publications
        .get(&app_pid)
        .is_some_and(|reservation| {
            reservation.generation == publication_generation
                && reservation.session_id == target.session_id
                && reservation.target_pid == target.pid
                && reservation.window_id == target.window_id
                && reservation.target_epoch == current_epoch
        });
    if !publication_current {
        return false;
    }
    state.frame_proofs.insert(
        app_pid,
        DelegationFrameProof {
            target_pid: target.pid,
            window_id: target.window_id,
            epoch: current_epoch,
            publication_generation,
            session_id: target.session_id.clone(),
        },
    );
    true
}

fn delegation_frame_proof_is_live(target: &pip_preview::PipTarget) -> bool {
    if target.delegation.is_none() {
        return true;
    }
    let state = DELEGATION_PROOF_STATE.lock().unwrap();
    let current_epoch = state
        .target_epochs
        .get(&(target.pid, target.window_id))
        .copied()
        .unwrap_or(0);
    let app_pid = target.app_key_pid();
    let reservation = state.latest_publications.get(&app_pid);
    state.frame_proofs.get(&app_pid).is_some_and(|proof| {
        proof.target_pid == target.pid
            && proof.window_id == target.window_id
            && proof.epoch == current_epoch
            && proof.session_id == target.session_id
            && reservation.is_some_and(|reservation| {
                reservation.generation == proof.publication_generation
                    && reservation.session_id == proof.session_id
                    && reservation.target_pid == proof.target_pid
                    && reservation.window_id == proof.window_id
                    && reservation.target_epoch == proof.epoch
            })
    })
}

fn remove_delegation_frame_proof(app_pid: i64) {
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    state.frame_proofs.remove(&app_pid);
    state.latest_publications.remove(&app_pid);
}

fn remove_expired_card_proof(app_pid: i64) {
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    // The verifier can retire the previous card while its replacement is
    // capturing. It must not revoke that newer observation's reservation.
    if state
        .latest_publications
        .get(&app_pid)
        .is_some_and(|reservation| reservation.awaiting_frame)
    {
        return;
    }
    state.frame_proofs.remove(&app_pid);
    state.latest_publications.remove(&app_pid);
}

fn remove_session_delegation_frame_proofs(session_id: &str) {
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    state
        .frame_proofs
        .retain(|_, proof| proof.session_id.as_deref() != Some(session_id));
    state
        .latest_publications
        .retain(|_, reservation| reservation.session_id.as_deref() != Some(session_id));
}

fn remove_ended_session_card_proof(app_pid: i64, session_id: &str) {
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    // This UI callback can follow a newer session's observation reservation.
    // It may retire the old card, but not that replacement's capture proof.
    state
        .frame_proofs
        .retain(|pid, proof| *pid != app_pid || proof.session_id.as_deref() != Some(session_id));
    state.latest_publications.retain(|pid, reservation| {
        *pid != app_pid || reservation.session_id.as_deref() != Some(session_id)
    });
}

fn pip_delegation_is_live(target: &pip_preview::PipTarget) -> bool {
    if !pip_target_session_is_live(target) {
        return false;
    }
    if is_application_menu_target(target) {
        return pip_application_menu_image(target).is_some();
    }
    let Some(delegation) = target.delegation.as_ref() else {
        return true;
    };
    if delegation.kind != "trusted_macos_open_save_panel"
        || target.logical_pid != Some(delegation.host_pid)
        || target.app_key_pid() != delegation.host_pid
    {
        return false;
    }
    let (Ok(host_pid), Ok(target_pid), Ok(target_window_id)) = (
        i32::try_from(delegation.host_pid),
        i32::try_from(target.pid),
        u32::try_from(target.window_id),
    ) else {
        return false;
    };
    let panel_kind = match delegation.panel_kind.as_str() {
        "open" => crate::ax::app_context::OpenSavePanelKind::Open,
        "save" => crate::ax::app_context::OpenSavePanelKind::Save,
        _ => return false,
    };
    let expected = crate::ax::app_context::ExpectedAppIdentity {
        bundle_id: delegation.expected_bundle_id.clone(),
        app_name: delegation.expected_app_name.clone(),
    };
    if expected.bundle_id.is_none() && expected.app_name.is_none() {
        return false;
    }
    crate::ax::app_context::resolve_app_context(host_pid, &expected)
        .ok()
        .is_some_and(|resolved| {
            pip_delegation_matches_resolved(
                target,
                host_pid,
                target_pid,
                target_window_id,
                panel_kind,
                &resolved,
            )
        })
}

fn is_application_menu_target(target: &pip_preview::PipTarget) -> bool {
    target.pid > 0 && target.logical_pid == Some(target.pid) && target.delegation.is_none()
}

fn pip_application_menu_image(
    target: &pip_preview::PipTarget,
) -> Option<crate::ax::application_menu::ApplicationMenuImage> {
    if !is_application_menu_target(target) {
        return None;
    }
    let private_image = DELEGATION_PROOF_STATE
        .lock()
        .unwrap()
        .latest_publications
        .get(&target.app_key_pid())
        .and_then(|reservation| private_menu_for_target(reservation, target));
    if let Some(image) = private_image {
        return crate::ax::application_menu::revalidate_menu_image(&image).then_some(image);
    }
    crate::ax::application_menu::observed_live_menu_image(
        i32::try_from(target.pid).ok()?,
        u32::try_from(target.window_id).ok()?,
    )
}

fn private_menu_for_target(
    reservation: &PipPublicationReservation,
    target: &pip_preview::PipTarget,
) -> Option<crate::ax::application_menu::ApplicationMenuImage> {
    let menu = reservation.menu_image.as_ref()?;
    let current_source =
        reservation.target_pid == target.pid && reservation.window_id == target.window_id;
    let pending_same_host = reservation
        .pending_observation
        .as_ref()
        .is_some_and(|pending| {
            pending.pid == target.pid && pending.window_id == u64::from(menu.document_window_id)
        });
    (reservation.session_id == target.session_id
        && i64::from(menu.pid) == target.pid
        && u64::from(menu.menu_window_id) == target.window_id
        && (current_source || pending_same_host))
        .then(|| menu.clone())
}

fn pip_target_session_is_live(target: &pip_preview::PipTarget) -> bool {
    target
        .session_id
        .as_deref()
        .is_none_or(|session_id| !cua_driver_core::session::is_session_ended(session_id))
}

fn pip_delegation_matches_resolved(
    target: &pip_preview::PipTarget,
    host_pid: i32,
    target_pid: i32,
    target_window_id: u32,
    panel_kind: crate::ax::app_context::OpenSavePanelKind,
    resolved: &crate::ax::app_context::ResolvedAppContext,
) -> bool {
    target.app_key_pid() == i64::from(host_pid)
        && target.pid == i64::from(target_pid)
        && target.window_id == u64::from(target_window_id)
        && resolved.target
            == crate::ax::app_context::AppContextTarget {
                pid: target_pid,
                window_id: target_window_id,
            }
        && resolved.delegation.as_ref().is_some_and(|current| {
            current.host_pid == host_pid
                && current.target == resolved.target
                && current.panel_kind == panel_kind
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipTargetSeedPolicy {
    ReuseObservationAndLiveStream,
    AwaitObservationSeed,
    CaptureObservationPreview,
}

fn pip_observation_seed_policy(
    frame: Option<&PipFrame>,
    target: &pip_preview::PipTarget,
) -> PipTargetSeedPolicy {
    if !is_application_menu_target(target)
        && frame.is_some_and(|frame| same_observation_target(&frame.target, target))
    {
        PipTargetSeedPolicy::ReuseObservationAndLiveStream
    } else {
        PipTargetSeedPolicy::CaptureObservationPreview
    }
}

fn preview_window_unchanged(
    before: &crate::windows::WindowInfo,
    after: &crate::windows::WindowInfo,
) -> bool {
    before.pid == after.pid
        && before.window_id == after.window_id
        && before.layer == after.layer
        && [
            before.bounds.x,
            before.bounds.y,
            before.bounds.width,
            before.bounds.height,
        ]
        .map(f64::to_bits)
            == [
                after.bounds.x,
                after.bounds.y,
                after.bounds.width,
                after.bounds.height,
            ]
            .map(f64::to_bits)
}

fn refresh_observed_preview(
    observed: &pip_preview::PipTarget,
    generation: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        pip_target_session_is_live(observed)
            && pip_target_is_selected(observed)
            && pip_publication_is_current(observed, generation)
            && physical_target_is_live(observed.pid, observed.window_id)
            && pip_delegation_is_live(observed),
        "observed preview target is no longer live"
    );
    let pid = i32::try_from(observed.pid)?;
    let host_window = u32::try_from(observed.window_id)?;
    let menu = if observed.logical_pid.is_none() && observed.delegation.is_none() {
        crate::ax::application_menu::active_application_menu(pid, host_window)
    } else {
        None
    };
    let mut source = observed.clone();
    if let Some(menu) = menu.as_ref() {
        source.logical_pid = Some(observed.pid);
        source.window_id = u64::from(menu.menu_window_id);
    }
    anyhow::ensure!(
        select_observation_preview_source(observed, &source, menu, generation),
        "preview superseded"
    );
    anyhow::ensure!(
        register_delegation_frame_proof(&source, generation),
        "preview delegation changed"
    );
    let (policy, previous_source) = {
        let model = VIEW_MODEL.lock().unwrap();
        let frame = model
            .as_ref()
            .and_then(|model| model.frame_for_app(source.app_key_pid()));
        (
            pip_observation_seed_policy(frame, &source),
            frame
                .filter(|frame| {
                    frame.target.session_id == source.session_id
                        && !same_observation_target(&frame.target, &source)
                })
                .map(|frame| frame.target.clone()),
        )
    };
    if policy == PipTargetSeedPolicy::ReuseObservationAndLiveStream {
        finish_pip_observation(&source, generation);
        clear_completed_observation_retry(source.app_key_pid(), generation);
        return Ok(());
    }
    // Remove the previous card on the UI queue before replacing its source.
    // A failed new capture must not leave the old window labelled as current.
    if let Some(previous_source) = previous_source {
        dispatch_to_main(
            (source.clone(), generation, previous_source),
            clear_observation_preview_cb,
        );
    }
    let window_id = u32::try_from(source.window_id)?;
    let before = crate::windows::window_info_by_id(window_id)
        .ok_or_else(|| anyhow::anyhow!("preview window disappeared"))?;
    anyhow::ensure!(
        i64::from(before.pid) == source.pid
            && pip_publication_is_current(&source, generation)
            && pip_delegation_is_live(&source),
        "preview source changed"
    );
    let raw = crate::capture::screenshot_window_bytes(window_id)?;
    let after = crate::windows::window_info_by_id(window_id)
        .ok_or_else(|| anyhow::anyhow!("preview window disappeared"))?;
    let (width, height) = crate::capture::png_dimensions(&raw)?;
    crate::tools::px_frame::validate_capture_frame(window_id, &before.bounds, width, height)
        .map_err(|error| anyhow::anyhow!("preview frame mismatch: {error:?}"))?;
    anyhow::ensure!(
        preview_window_unchanged(&before, &after)
            && physical_target_is_live(observed.pid, observed.window_id)
            && pip_target_session_is_live(&source)
            && pip_publication_is_current(&source, generation)
            && pip_delegation_is_live(&source),
        "preview changed during capture"
    );
    let png_bytes = crate::capture::resize_png_if_needed(&raw, LIVE_CAPTURE_MAX_SIDE as u32)?;
    source.app_name = i32::try_from(source.app_key_pid())
        .ok()
        .and_then(crate::apps::get_app_name_for_pid)
        .unwrap_or(before.app_name);
    source.window_title = (!before.title.trim().is_empty()).then_some(before.title);
    dispatch_to_main(
        VerifiedPipFrame {
            frame: PipFrame {
                target: source,
                png_bytes,
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            },
            publication_generation: generation,
        },
        push_frame_cb,
    );
    Ok(())
}

unsafe extern "C" fn clear_observation_preview_cb(ctx: *mut c_void) {
    let (target, generation, previous_source) =
        *Box::from_raw(ctx as *mut (pip_preview::PipTarget, u64, pip_preview::PipTarget));
    let Some(change) = remove_current_observation_card(&previous_source, || {
        pip_target_session_is_live(&target) && pip_publication_is_current(&target, generation)
    }) else {
        return;
    };
    apply_model_change(&change);
    render_snapshot(&current_snapshot());
}

unsafe extern "C" fn fail_observation_preview_cb(ctx: *mut c_void) {
    let (target, generation, error) =
        *Box::from_raw(ctx as *mut (pip_preview::PipTarget, u64, String));
    let model = VIEW_MODEL.lock().unwrap();
    if !model
        .as_ref()
        .is_some_and(|model| model.accepts_target(&target))
        || !pip_target_session_is_live(&target)
        || !DELEGATION_PROOF_STATE
            .lock()
            .unwrap()
            .latest_publications
            .get(&target.app_key_pid())
            .is_some_and(|reservation| reservation.generation == generation)
    {
        return;
    }
    // An observation can succeed before the capture service/window geometry is
    // ready. Keep that exact observation as retry authority, independently of
    // whether any card exists yet. Do not silently forget a cold AX-only target.
    let mut retries = OBSERVATION_RETRIES.lock().unwrap();
    let changed_error = retries.get(&target.app_key_pid()).is_none_or(|previous| {
        !same_observation_target(&previous.target, &target) || previous.error != error
    });
    if changed_error {
        tracing::warn!(target: "pip", app_pid = target.app_key_pid(), pid = target.pid,
            window_id = target.window_id, %error, "preview unavailable; retrying the observed target");
    }
    let attempts = retries
        .get(&target.app_key_pid())
        .filter(|previous| same_observation_target(&previous.target, &target))
        .map_or(1, |previous| previous.attempts.saturating_add(1));
    retries.insert(
        target.app_key_pid(),
        ObservationRetry {
            target: target.clone(),
            generation,
            retry_at_ms: monotonic_ms().saturating_add(preview_retry_delay_ms(attempts)),
            error,
            attempts,
        },
    );
    // Allow a fresh explicit observation to supersede this retry immediately.
    finish_pip_observation(&target, generation);
    drop(retries);
    drop(model);
    render_snapshot(&current_snapshot());
}

fn remove_current_observation_card(
    target: &pip_preview::PipTarget,
    is_current: impl FnOnce() -> bool,
) -> Option<PipModelChange> {
    // Lock order is model -> publication state (inside is_current). Keep the
    // generation check atomic with removal relative to worker reuse decisions.
    let mut model = VIEW_MODEL.lock().unwrap();
    if !is_current() {
        return None;
    }
    Some(
        model
            .as_mut()
            .map(|model| model.remove_target_frame(target))
            .unwrap_or_default(),
    )
}

fn pip_target_seed_policy(
    frame: Option<&PipFrame>,
    target: &pip_preview::PipTarget,
) -> PipTargetSeedPolicy {
    if frame.is_some_and(|frame| exact_target_matches(frame, target)) {
        PipTargetSeedPolicy::ReuseObservationAndLiveStream
    } else {
        PipTargetSeedPolicy::AwaitObservationSeed
    }
}

unsafe extern "C" fn set_input_passthrough_cb(ctx: *mut c_void) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let passthrough: bool = *Box::from_raw(ctx as *mut bool);
    if let Some(handles) = HANDLES.lock().unwrap().as_ref() {
        let window = handles.window as *mut AnyObject;
        let _: () = msg_send![window, setIgnoresMouseEvents: passthrough];
    }
}

unsafe extern "C" fn push_frame_cb(ctx: *mut c_void) {
    let verified: VerifiedPipFrame = *Box::from_raw(ctx as *mut VerifiedPipFrame);
    let frame = verified.frame;
    let completed_target = frame.target.clone();
    let pid = frame.target.app_key_pid();
    let outcome = {
        let mut model = VIEW_MODEL.lock().unwrap();
        // A newer tree-only observation may reuse the currently displayed
        // frame without enqueueing another push. Check under the same lock
        // as that reuse decision so an older push cannot replace it later.
        if !pip_target_session_is_live(&frame.target)
            || !model
                .as_ref()
                .is_some_and(|model| model.accepts_target(&frame.target))
            || !pip_publication_is_current(&frame.target, verified.publication_generation)
            || !delegation_frame_proof_is_live(&frame.target)
            || HIDDEN_APPS.lock().unwrap().contains(&pid)
        {
            return;
        }
        let model = model.get_or_insert_with(|| PipViewModel::new(MAX_VISIBLE_PIP_CARDS));
        model.upsert(frame)
    };

    if !outcome.accepted {
        return;
    }
    let mut change = outcome.change;
    change.changed_pids.retain(|changed| *changed != pid);
    apply_model_change(&change);
    clear_completed_observation_retry(pid, verified.publication_generation);

    if let Some(evicted_pid) = outcome.evicted_pid {
        remove_delegation_frame_proof(evicted_pid);
        stop_live_capture_for(evicted_pid);
    }
    if outcome.window_changed {
        stop_live_capture_for(pid);
    }
    let snapshot = current_snapshot();
    render_snapshot(&snapshot);
    finish_pip_observation(&completed_target, verified.publication_generation);
}

unsafe extern "C" fn end_session_cb(ctx: *mut c_void) {
    let session_id: String = *Box::from_raw(ctx as *mut String);
    let (snapshot, change) = {
        let mut model = VIEW_MODEL.lock().unwrap();
        let Some(model) = model.as_mut() else {
            return;
        };
        let change = model.remove_session(&session_id);
        let snapshot = model
            .ordered_frames()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        (snapshot, change)
    };
    for &pid in &change.removed_pids {
        remove_ended_session_card_proof(pid, &session_id);
    }
    apply_model_change(&change);
    render_snapshot(&snapshot);
}

fn current_snapshot() -> Vec<PipFrame> {
    VIEW_MODEL
        .lock()
        .unwrap()
        .as_ref()
        .map(|model| model.ordered_frames().into_iter().cloned().collect())
        .unwrap_or_default()
}

/// Metadata-only diagnostics: no screenshot, AX read, target selection, or
/// window activation. In particular a live overlay window alone is not proof
/// that the requested app has a visible card or a running stream.
pub fn diagnostic_state() -> serde_json::Value {
    let (initialized, input_monitors_ready) = HANDLES
        .lock()
        .unwrap()
        .as_ref()
        .map(|handles| {
            (
                true,
                temporary_activation_monitors_ready(
                    handles.global_mouse_monitor,
                    handles.local_mouse_monitor,
                ),
            )
        })
        .unwrap_or((false, false));
    let snapshot = current_snapshot();
    let visible = CARD_HANDLES
        .lock()
        .unwrap()
        .keys()
        .copied()
        .collect::<HashSet<_>>();
    let (suppressed, held) = {
        let state = FOREGROUND_VISIBILITY_STATE.lock().unwrap();
        let now = monotonic_ms();
        let held = snapshot
            .iter()
            .filter(|frame| {
                state
                    .temporary_activations
                    .keeps_visible(activation_target(&frame.target), now)
            })
            .map(|frame| frame.target.app_key_pid())
            .collect::<HashSet<_>>();
        (state.suppressed_pids.clone(), held)
    };
    let streams = LIVE_STREAMS
        .lock()
        .unwrap()
        .iter()
        .map(|(&pid, entry)| {
            let status = if entry.cancelled.load(Ordering::Acquire) {
                "cancelled"
            } else if entry.retry_at.is_some() {
                "retry_wait"
            } else if entry.stream.is_some() {
                "streaming"
            } else {
                "starting"
            };
            (pid, status)
        })
        .collect::<HashMap<_, _>>();
    let cards = snapshot.iter().map(|frame| {
        let pid = frame.target.app_key_pid();
        serde_json::json!({
            "app_pid": pid, "source_pid": frame.target.pid, "window_id": frame.target.window_id,
            "visible": visible.contains(&pid), "foreground_suppressed": suppressed.contains(&pid),
            "capture": streams.get(&pid).copied().unwrap_or("stopped"),
            "temporary_activation_held": held.contains(&pid),
        })
    }).collect::<Vec<_>>();
    let retries = OBSERVATION_RETRIES
        .lock()
        .unwrap()
        .values()
        .map(|retry| {
            serde_json::json!({
                "app_pid": retry.target.app_key_pid(), "source_pid": retry.target.pid,
                "window_id": retry.target.window_id, "attempts": retry.attempts,
                "retry_pending": retry.retry_at_ms != u64::MAX, "last_error": retry.error,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "initialized": initialized, "cards": cards, "preview_retries": retries,
        "last_card_activation": LAST_CARD_ACTIVATION.lock().unwrap().clone(),
        "input_monitors_ready": input_monitors_ready,
        "presentation_counters": {
            "card_view_rebuilds": CARD_VIEW_REBUILDS.load(Ordering::Relaxed),
            "card_view_reuses": CARD_VIEW_REUSES.load(Ordering::Relaxed),
            "temporary_activation_starts": TEMPORARY_ACTIVATION_STARTS.load(Ordering::Relaxed),
            "external_input_hold_cancellations": EXTERNAL_INPUT_HOLD_CANCELLATIONS.load(Ordering::Relaxed),
        },
    })
}

fn frame_is_suppressed(frame: &PipFrame, suppressed_pids: &HashSet<i64>) -> bool {
    suppressed_pids.contains(&frame.target.app_key_pid())
}

fn frame_needs_live_capture(frame: &PipFrame, suppressed_pids: &HashSet<i64>) -> bool {
    // Menu pixels are re-published by each observation. Do not start an
    // unproven live menu stream or overwrite them with the document stream.
    !frame_is_suppressed(frame, suppressed_pids) && !is_application_menu_target(&frame.target)
}

fn frame_has_preview_authority(frame: &PipFrame) -> bool {
    pip_target_session_is_live(&frame.target)
        && pip_target_is_selected(&frame.target)
        && delegation_frame_proof_is_live(&frame.target)
}

fn snapshot_candidate_pid(snapshot: &[PipFrame], pid: Option<i32>) -> Option<i64> {
    let pid = i64::from(pid?);
    snapshot
        .iter()
        .any(|frame| frame.target.app_key_pid() == pid)
        .then_some(pid)
}

fn visually_frontmost_app_key_in(
    snapshot: &[PipFrame],
    windows: &[crate::windows::WindowInfo],
    local_pid: i32,
    mut is_auxiliary: impl FnMut(i32) -> bool,
) -> Option<i64> {
    let mut candidates = windows
        .iter()
        .filter(|window| {
            window.pid > 0
                && window.pid != local_pid
                && window.layer == 0
                && window.is_on_screen
                && window.on_current_space != Some(false)
                && window.bounds.width > 1.0
                && window.bounds.height > 1.0
                && !window.app_name.trim().is_empty()
        })
        .collect::<Vec<_>>();
    // `WindowInfo::z_index` normalizes WindowServer's front-to-back order so
    // larger values are closer to the front. Keep this aligned with
    // `windows::resolve_main_window_id_in` and its ordering contract.
    candidates.sort_by_key(|window| std::cmp::Reverse(window.z_index));

    let mut checked_pids = HashSet::new();
    for window in candidates {
        // A delegated Open/Save panel is an auxiliary process, but when its
        // exact physical window is visually frontmost the corresponding
        // logical host card must disappear. Match the complete tuple because
        // the service can host panels for multiple applications.
        if let Some(frame) = snapshot.iter().find(|frame| {
            frame.target.pid == i64::from(window.pid)
                && frame.target.window_id == u64::from(window.window_id)
        }) {
            return Some(frame.target.app_key_pid());
        }
        if !checked_pids.insert(window.pid) || is_auxiliary(window.pid) {
            continue;
        }
        // The first non-auxiliary visible window is authoritative even when
        // it has no PiP card; do not look through another foreground app and
        // accidentally suppress a lower card.
        return snapshot_candidate_pid(snapshot, Some(window.pid));
    }
    None
}

fn foreground_candidate_pids(
    snapshot: &[PipFrame],
    workspace_frontmost_pid: Option<i32>,
    visual_frontmost_app_key: Option<i64>,
) -> HashSet<i64> {
    // A background-delivered modal can be visibly above every other app while
    // NSWorkspace continues to report the user's terminal as active. Suppress
    // both matching owners rather than letting the visual result replace the
    // active application. WindowServer exposes one global ordering across
    // displays, so the visual member remains a best-effort multi-display hint;
    // the authoritative NSWorkspace member is always retained alongside it.
    let mut suppressed = workspace_frontmost_pid
        .and_then(|pid| snapshot_candidate_pid(snapshot, Some(pid)))
        .into_iter()
        .collect::<HashSet<_>>();
    if let Some(app_key) = visual_frontmost_app_key.filter(|app_key| {
        snapshot
            .iter()
            .any(|frame| frame.target.app_key_pid() == *app_key)
    }) {
        suppressed.insert(app_key);
    }
    suppressed
}

/// Only an OnScreenOnly WindowServer snapshot supplies visual front order.
/// Filtering an all-window snapshot by `is_on_screen` is not equivalent: its
/// order can put a background window ahead of the actual foreground app.
fn foreground_candidates_from_visible_windows(
    snapshot: &[PipFrame],
    visible_windows: &crate::windows::WindowEnumeration,
    workspace_frontmost_pid: Option<i32>,
    local_pid: Option<i32>,
    is_auxiliary: impl FnMut(i32) -> bool,
) -> HashSet<i64> {
    let visual_frontmost_app_key = visible_windows
        .succeeded
        .then(|| {
            local_pid.and_then(|local_pid| {
                visually_frontmost_app_key_in(
                    snapshot,
                    &visible_windows.windows,
                    local_pid,
                    is_auxiliary,
                )
            })
        })
        .flatten();
    foreground_candidate_pids(snapshot, workspace_frontmost_pid, visual_frontmost_app_key)
}

fn foreground_visibility_watcher_needed(snapshot: &[PipFrame]) -> bool {
    !snapshot.is_empty()
}

fn update_foreground_visibility_state(
    state: &mut ForegroundVisibilityState,
    observed_pids: HashSet<i64>,
) -> bool {
    let changed = state.suppressed_pids != observed_pids;
    state.suppressed_pids = observed_pids;
    // Even an unchanged synchronous render supersedes older in-flight samples.
    state.invalidate_pending();
    changed
}

fn suppression_after_window_enumeration(
    previous: &HashSet<i64>,
    mut observed: HashSet<i64>,
    enumeration_succeeded: bool,
) -> HashSet<i64> {
    if !enumeration_succeeded {
        // Failed enumeration is not evidence that the last visual foreground
        // disappeared. Still add an independently observed active app.
        observed.extend(previous.iter().copied());
    }
    observed
}

/// Return the current suppression, whether to render, and whether to resample.
/// Only the main-queue callback commits an asynchronous observation.
fn apply_foreground_visibility_refresh(
    state: &mut ForegroundVisibilityState,
    refresh: &ForegroundVisibilityRefresh,
) -> (HashSet<i64>, bool, bool) {
    if refresh.observed_epoch != state.epoch {
        // Candidate pruning already changed the shared model on the worker.
        // Preserve that render obligation, but never apply stale suppression.
        return (
            state.suppressed_pids.clone(),
            refresh.candidates_changed,
            true,
        );
    }
    let changed = update_foreground_visibility_state(state, refresh.suppressed_pids.clone());
    (
        state.suppressed_pids.clone(),
        candidate_refresh_requires_render(changed, refresh.candidates_changed),
        false,
    )
}

fn live_targets_from_window_enumeration(
    enumeration: &crate::windows::WindowEnumeration,
) -> Option<HashSet<(i64, u64)>> {
    enumeration.succeeded.then(|| {
        enumeration
            .windows
            .iter()
            .map(|window| (i64::from(window.pid), u64::from(window.window_id)))
            .collect()
    })
}

fn refresh_live_candidates(live_targets: &HashSet<(i64, u64)>) -> (Vec<PipFrame>, bool) {
    let candidates = VIEW_MODEL
        .lock()
        .unwrap()
        .as_ref()
        .map(|model| {
            model
                .retained_targets()
                .into_iter()
                .map(|target| {
                    let representative = model
                        .frame_for_app(target.app_key_pid())
                        .is_some_and(|frame| same_observation_target(&frame.target, target));
                    (target.clone(), representative)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // Native host→panel validation can involve AX and code-signature work.
    // Perform it without holding the view-model mutex so frame publication and
    // AppKit rendering are never serialized behind that proof.
    let checked = candidates
        .into_iter()
        .map(|(target, representative)| {
            let key = (
                target.app_key_pid(),
                target.pid,
                target.window_id,
                target.session_id.clone(),
            );
            let restoring = representative
                && DELEGATION_PROOF_STATE
                    .lock()
                    .unwrap()
                    .latest_publications
                    .get(&target.app_key_pid())
                    .is_some_and(|reservation| {
                        reservation.awaiting_frame
                            && reservation.session_id == target.session_id
                            && reservation.target_pid == target.pid
                            && reservation.window_id == target.window_id
                    });
            let live = live_targets.contains(&(target.pid, target.window_id))
                && pip_delegation_is_live(&target)
                // The display proof belongs to one representative per app.
                // Another session's retained frame must not borrow it or be
                // deleted merely because that session is not represented.
                // Restoration revalidates and establishes its own proof off
                // the UI thread before publishing; keep it during that work.
                && (!representative || restoring || delegation_frame_proof_is_live(&target));
            (key, live)
        })
        .collect::<HashMap<_, _>>();
    let (snapshot, change) = {
        let mut model = VIEW_MODEL.lock().unwrap();
        let Some(model) = model.as_mut() else {
            return (Vec::new(), false);
        };
        let change = model.retain_live_targets(|target| {
            checked
                .get(&(
                    target.app_key_pid(),
                    target.pid,
                    target.window_id,
                    target.session_id.clone(),
                ))
                .copied()
                // A newer frame appeared after the validation snapshot. Keep
                // it for the next verifier tick rather than applying stale
                // evidence from the superseded target.
                .unwrap_or(true)
        });
        let snapshot = model
            .ordered_frames()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        (snapshot, change)
    };
    let changed = !change.is_empty();
    if changed {
        // UI rebuilding and representative re-publication belong on AppKit's
        // queue, not the verifier worker.
        dispatch_to_main(change, apply_model_change_cb);
    }
    (snapshot, changed)
}

fn foreground_visibility_check_is_due(now_ms: u64, previous_ms: u64) -> bool {
    previous_ms == 0
        || now_ms.saturating_sub(previous_ms)
            >= FOREGROUND_VISIBILITY_CHECK_INTERVAL.as_millis() as u64
}

fn candidate_refresh_requires_render(suppression_changed: bool, candidates_changed: bool) -> bool {
    suppression_changed || candidates_changed
}

fn compute_foreground_visibility_refresh() -> Option<ForegroundVisibilityRefresh> {
    let now_ms = monotonic_ms();
    let previous_ms = LAST_FOREGROUND_CHECK_MS.load(Ordering::Acquire);
    if !foreground_visibility_check_is_due(now_ms, previous_ms)
        || LAST_FOREGROUND_CHECK_MS
            .compare_exchange(previous_ms, now_ms, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return None;
    }

    // Capture before the potentially slow native enumeration. A synchronous
    // render or activation notification invalidates this sample while it runs.
    let (observed_epoch, previous_suppressed) = {
        let state = FOREGROUND_VISIBILITY_STATE.lock().unwrap();
        (state.epoch, state.suppressed_pids.clone())
    };

    // The full snapshot keeps minimized/off-Space targets alive, but its
    // ordering is not visual front order. Never prune from the visible list.
    // A failed liveness enumeration retains all cards until the next tick.
    let enumeration = crate::windows::all_windows_including_accessory_layers_with_snapshot();
    let Some(live_targets) = live_targets_from_window_enumeration(&enumeration) else {
        return None;
    };
    schedule_live_capture_resizes(&enumeration.windows);
    let (snapshot, candidates_changed) = refresh_live_candidates(&live_targets);
    let visible_windows =
        crate::windows::visible_windows_including_accessory_layers_with_snapshot();
    let foreground_pid = crate::apps::frontmost_pid();
    let observed = foreground_candidates_from_visible_windows(
        &snapshot,
        &visible_windows,
        foreground_pid,
        i32::try_from(std::process::id()).ok(),
        crate::apps::is_auxiliary_application,
    );
    let mut suppressed = suppression_after_window_enumeration(
        &previous_suppressed,
        observed,
        visible_windows.succeeded,
    );
    {
        let mut state = FOREGROUND_VISIBILITY_STATE.lock().unwrap();
        // A begin/end or user takeover during enumeration invalidates the
        // sample. It must not cancel or apply a newer action's lease.
        if state.epoch == observed_epoch {
            preserve_temporarily_activated_cards(
                &mut state,
                &snapshot,
                foreground_pid,
                &mut suppressed,
                monotonic_ms(),
            );
        }
    }
    Some(ForegroundVisibilityRefresh {
        observed_epoch,
        suppressed_pids: suppressed,
        candidates_changed,
    })
}

unsafe extern "C" fn refresh_foreground_visibility_cb(ctx: *mut c_void) {
    let refresh = *Box::from_raw(ctx as *mut ForegroundVisibilityRefresh);
    if HANDLES.lock().unwrap().is_none() {
        FOREGROUND_REFRESH_PENDING.store(false, Ordering::Release);
        return;
    }
    let (suppressed, render_required, resample) = apply_foreground_visibility_refresh(
        &mut FOREGROUND_VISIBILITY_STATE.lock().unwrap(),
        &refresh,
    );
    if render_required {
        render_snapshot_with_suppressed_windows(&current_snapshot(), &suppressed);
    } else {
        // A failed stream bootstrap needs a retry even when focus has not
        // changed. Healthy streams are reused, without rebuilding any views.
        reconcile_live_capture(&current_snapshot(), &suppressed);
    }
    FOREGROUND_REFRESH_PENDING.store(false, Ordering::Release);
    if resample {
        LAST_FOREGROUND_CHECK_MS.store(0, Ordering::Release);
        schedule_foreground_visibility_refresh();
    }
}

fn schedule_foreground_visibility_refresh() {
    if FOREGROUND_REFRESH_PENDING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        if std::thread::Builder::new()
            .name("cua-pip-verify".to_owned())
            .spawn(|| {
                if let Some(refresh) = compute_foreground_visibility_refresh() {
                    dispatch_to_main(refresh, refresh_foreground_visibility_cb);
                } else {
                    FOREGROUND_REFRESH_PENDING.store(false, Ordering::Release);
                }
            })
            .is_err()
        {
            FOREGROUND_REFRESH_PENDING.store(false, Ordering::Release);
        }
    }
}

fn ensure_foreground_visibility_watcher() {
    let mut watcher = FOREGROUND_VISIBILITY_WATCHER.lock().unwrap();
    if watcher
        .as_ref()
        .is_some_and(|current| !current.cancelled.load(Ordering::Acquire))
    {
        return;
    }
    if let Some(previous) = watcher.take() {
        previous.cancelled.store(true, Ordering::Release);
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    *watcher = Some(ForegroundVisibilityWatcher {
        cancelled: Arc::clone(&cancelled),
    });
    drop(watcher);

    if let Err(error) = std::thread::Builder::new()
        .name("cua-pip-foreground".to_owned())
        .spawn(move || {
            while !cancelled.load(Ordering::Acquire) {
                std::thread::sleep(FOREGROUND_VISIBILITY_CHECK_INTERVAL);
                if !cancelled.load(Ordering::Acquire) {
                    retry_observed_previews();
                    schedule_foreground_visibility_refresh();
                }
            }
        })
    {
        FOREGROUND_VISIBILITY_WATCHER.lock().unwrap().take();
        tracing::warn!(target: "pip", %error, "failed to spawn PiP foreground watcher");
    }
}

fn stop_foreground_visibility_watcher() {
    if let Some(watcher) = FOREGROUND_VISIBILITY_WATCHER.lock().unwrap().take() {
        watcher.cancelled.store(true, Ordering::Release);
    }
}

fn reconcile_live_capture(snapshot: &[PipFrame], suppressed_pids: &HashSet<i64>) {
    if foreground_visibility_watcher_needed(snapshot)
        || !OBSERVATION_RETRIES.lock().unwrap().is_empty()
    {
        // Same-app modal changes do not emit an application-activation
        // notification, so visibility must be reconciled while cards exist,
        // not only after one has already been suppressed.
        ensure_foreground_visibility_watcher();
    } else {
        stop_foreground_visibility_watcher();
    }
    for pid in suppressed_pids {
        stop_live_capture_for(*pid);
    }

    for frame in snapshot {
        if is_application_menu_target(&frame.target) {
            stop_live_capture_for(frame.target.app_key_pid());
        }
        if frame_has_preview_authority(frame) && frame_needs_live_capture(frame, suppressed_pids) {
            ensure_live_capture(
                frame.target.app_key_pid(),
                frame.target.pid,
                frame.target.window_id,
            );
        }
    }
}

fn capture_dimensions(width: f64, height: f64) -> (u32, u32) {
    let width = width.max(1.0);
    let height = height.max(1.0);
    let scale = (LIVE_CAPTURE_MAX_SIDE / width.max(height)).min(1.0);
    (
        (width * scale).round().max(1.0) as u32,
        (height * scale).round().max(1.0) as u32,
    )
}

fn live_capture_output_dimensions(width: f64, height: f64) -> Option<(u32, u32)> {
    (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
        .then(|| capture_dimensions(width, height))
}

fn live_capture_configuration(dimensions: (u32, u32)) -> SCStreamConfiguration {
    let frame_interval = CMTime::new(1, LIVE_CAPTURE_FPS);
    SCStreamConfiguration::new()
        .with_width(dimensions.0)
        .with_height(dimensions.1)
        .with_scales_to_fit(true)
        .with_preserves_aspect_ratio(true)
        .with_queue_depth(2)
        .with_minimum_frame_interval(&frame_interval)
        // Keep the overlay cursor separate from the captured desktop cursor.
        .with_shows_cursor(false)
}

fn live_capture_generation_matches(
    entry: &LiveStreamEntry,
    target_pid: i64,
    window_id: u64,
    cancelled: &Arc<AtomicBool>,
) -> bool {
    entry.target_pid == target_pid
        && entry.window_id == window_id
        && Arc::ptr_eq(&entry.cancelled, cancelled)
        && !cancelled.load(Ordering::Acquire)
}

fn finish_live_capture_resize(
    app_pid: i64,
    target_pid: i64,
    window_id: u64,
    cancelled: &Arc<AtomicBool>,
    dimensions: (u32, u32),
    succeeded: bool,
) {
    let mut streams = LIVE_STREAMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(entry) = streams
        .get_mut(&app_pid)
        .filter(|entry| live_capture_generation_matches(entry, target_pid, window_id, cancelled))
    {
        entry.resize.finish(dimensions, succeeded);
    }
}

fn schedule_live_capture_resizes(windows: &[crate::windows::WindowInfo]) {
    let requests = {
        let mut streams = LIVE_STREAMS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        streams
            .iter_mut()
            .filter_map(|(&app_pid, entry)| {
                if entry.cancelled.load(Ordering::Acquire) {
                    return None;
                }
                let window = windows.iter().find(|window| {
                    i64::from(window.pid) == entry.target_pid
                        && u64::from(window.window_id) == entry.window_id
                })?;
                let dimensions =
                    live_capture_output_dimensions(window.bounds.width, window.bounds.height)?;
                let stream = entry.stream.as_ref()?;
                if !entry.resize.begin(dimensions) {
                    return None;
                }
                Some((
                    app_pid,
                    entry.target_pid,
                    entry.window_id,
                    stream.clone(),
                    Arc::clone(&entry.cancelled),
                    dimensions,
                ))
            })
            .collect::<Vec<_>>()
    };

    for (app_pid, target_pid, window_id, stream, cancelled, dimensions) in requests {
        let cancelled_on_spawn_failure = Arc::clone(&cancelled);
        if let Err(error) = std::thread::Builder::new()
            .name(format!("cua-pip-resize-{app_pid}"))
            .spawn(move || {
                let current = LIVE_STREAMS
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&app_pid)
                    .is_some_and(|entry| {
                        live_capture_generation_matches(entry, target_pid, window_id, &cancelled)
                            && entry.resize.pending == Some(dimensions)
                    });
                if !current {
                    return;
                }
                // Check exact live ownership again after leaving the enumeration
                // thread. Never hold the global stream mutex while SCK waits.
                let live_dimensions = u32::try_from(window_id)
                    .ok()
                    .and_then(crate::windows::window_info_by_id)
                    .filter(|window| i64::from(window.pid) == target_pid)
                    .and_then(|window| {
                        live_capture_output_dimensions(window.bounds.width, window.bounds.height)
                    });
                if live_dimensions != Some(dimensions) || cancelled.load(Ordering::Acquire) {
                    finish_live_capture_resize(
                        app_pid, target_pid, window_id, &cancelled, dimensions, false,
                    );
                    return;
                }
                let result = stream.update_configuration(&live_capture_configuration(dimensions));
                finish_live_capture_resize(
                    app_pid,
                    target_pid,
                    window_id,
                    &cancelled,
                    dimensions,
                    result.is_ok(),
                );
                if let Err(error) = result {
                    tracing::debug!(target: "pip", app_pid, target_pid, window_id, %error,
                        "PiP stream resize was not applied; retaining prior configuration");
                }
            })
        {
            finish_live_capture_resize(
                app_pid,
                target_pid,
                window_id,
                &cancelled_on_spawn_failure,
                dimensions,
                false,
            );
            tracing::warn!(target: "pip", app_pid, target_pid, window_id, %error,
                "failed to spawn PiP resize worker");
        }
    }
}

fn monotonic_ms() -> u64 {
    FOREGROUND_CLOCK_ORIGIN.elapsed().as_millis().max(1) as u64
}

fn preview_retry_delay_ms(attempts: u32) -> u64 {
    PREVIEW_RETRY_INTERVAL_MS * (1u64 << attempts.saturating_sub(1).min(3))
}

fn live_capture_is_reusable(
    entry: &LiveStreamEntry,
    target_pid: i64,
    window_id: u64,
    now: Instant,
) -> bool {
    entry.target_pid == target_pid
        && entry.window_id == window_id
        && !entry.cancelled.load(Ordering::Acquire)
        && entry.retry_at.is_none_or(|retry_at| now < retry_at)
}

fn retry_observed_previews() {
    let candidates = OBSERVATION_RETRIES
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for retry in candidates {
        let target = &retry.target;
        let live = pip_target_session_is_live(target)
            && physical_target_is_live(target.pid, target.window_id);
        let model = VIEW_MODEL.lock().unwrap();
        let mut retries = OBSERVATION_RETRIES.lock().unwrap();
        let Some(current) = retries
            .get_mut(&target.app_key_pid())
            .filter(|current| current.generation == retry.generation)
        else {
            continue;
        };
        let generation_current = DELEGATION_PROOF_STATE
            .lock()
            .unwrap()
            .latest_publications
            .get(&target.app_key_pid())
            .is_some_and(|reservation| reservation.generation == retry.generation);
        if !live
            || !model
                .as_ref()
                .is_some_and(|model| model.accepts_target(target))
            || !generation_current
        {
            retries.remove(&target.app_key_pid());
            continue;
        }
        if monotonic_ms() < current.retry_at_ms {
            continue;
        }
        let Some(generation) = reserve_pip_observation(target) else {
            continue;
        };
        current.generation = generation;
        current.retry_at_ms = u64::MAX; // Coalesce while this attempt is running.
        drop(retries);
        drop(model);
        spawn_observation_capture(target.clone(), generation);
    }
}

fn ensure_live_capture(app_pid: i64, target_pid: i64, window_id: u64) {
    {
        let streams = LIVE_STREAMS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if streams.get(&app_pid).is_some_and(|entry| {
            live_capture_is_reusable(entry, target_pid, window_id, Instant::now())
        }) {
            return;
        }
    }

    stop_live_capture_for(app_pid);
    let cancelled = Arc::new(AtomicBool::new(false));
    let frame_pending = Arc::new(AtomicBool::new(false));
    LIVE_STREAMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            app_pid,
            LiveStreamEntry {
                target_pid,
                window_id,
                stream: None,
                resize: LiveCaptureResizeState::default(),
                cancelled: Arc::clone(&cancelled),
                frame_pending: Arc::clone(&frame_pending),
                retry_at: None,
            },
        );

    if let Err(error) = std::thread::Builder::new()
        .name(format!("cua-pip-{app_pid}"))
        .spawn(move || {
            match build_live_capture(
                app_pid,
                target_pid,
                window_id,
                Arc::clone(&cancelled),
                Arc::clone(&frame_pending),
            ) {
                Ok((stream, dimensions)) => {
                    let mut streams = LIVE_STREAMS
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let current = streams.get_mut(&app_pid).filter(|entry| {
                        entry.target_pid == target_pid
                            && entry.window_id == window_id
                            && Arc::ptr_eq(&entry.cancelled, &cancelled)
                            && !cancelled.load(Ordering::Acquire)
                    });
                    if let Some(entry) = current {
                        entry.stream = Some(stream);
                        entry.resize.configured = Some(dimensions);
                        tracing::info!(
                            target: "pip",
                            app_pid,
                            target_pid,
                            window_id,
                            fps = LIVE_CAPTURE_FPS,
                            "live app PiP capture started"
                        );
                    } else {
                        drop(streams);
                        let _ = stream.stop_capture();
                    }
                }
                Err(error) => {
                    let mut streams = LIVE_STREAMS
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let Some(entry) = streams.get_mut(&app_pid).filter(|entry| {
                        live_capture_generation_matches(entry, target_pid, window_id, &cancelled)
                    }) else {
                        return;
                    }; // Intentional foreground/target cancellation.
                    entry.retry_at =
                        Some(Instant::now() + Duration::from_millis(PREVIEW_RETRY_INTERVAL_MS));
                    drop(streams);
                    tracing::warn!(
                        target: "pip",
                        app_pid,
                        target_pid,
                        window_id,
                        %error,
                        "SCStream unavailable; retaining the verified frame and scheduling a retry"
                    );
                }
            }
        })
    {
        if let Some(entry) = LIVE_STREAMS.lock().unwrap().get_mut(&app_pid) {
            entry.retry_at =
                Some(Instant::now() + Duration::from_millis(PREVIEW_RETRY_INTERVAL_MS));
        }
        tracing::warn!(target: "pip", app_pid, target_pid, window_id, %error, "failed to spawn PiP capture worker");
    }
}

fn build_live_capture(
    app_pid: i64,
    target_pid: i64,
    window_id: u64,
    cancelled: Arc<AtomicBool>,
    frame_pending: Arc<AtomicBool>,
) -> anyhow::Result<(SCStream, (u32, u32))> {
    let native_window_id = u32::try_from(window_id)
        .map_err(|_| anyhow::anyhow!("window id {window_id} does not fit a CGWindowID"))?;
    let native_target_pid = i32::try_from(target_pid)
        .map_err(|_| anyhow::anyhow!("target pid is outside the native process-id range"))?;
    let deadline = Instant::now() + LIVE_CAPTURE_WINDOW_LOOKUP_TIMEOUT;
    let target_window = loop {
        if cancelled.load(Ordering::Acquire) {
            anyhow::bail!("capture cancelled");
        }
        let content = SCShareableContent::create()
            .with_exclude_desktop_windows(true)
            .with_on_screen_windows_only(false)
            .get()
            .map_err(|error| anyhow::anyhow!("SCShareableContent lookup failed: {error}"))?;
        if let Some(window) = content.windows().into_iter().find(|window| {
            screen_capture_window_matches_target(
                window.window_id(),
                window
                    .owning_application()
                    .map(|application| application.process_id()),
                native_window_id,
                native_target_pid,
            )
        }) {
            break window;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("target window {window_id} was not available to ScreenCaptureKit");
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let source_frame = target_window.frame();
    let dimensions =
        live_capture_output_dimensions(source_frame.size.width, source_frame.size.height)
            .ok_or_else(|| anyhow::anyhow!("PiP capture window has invalid dimensions"))?;
    let filter = SCContentFilter::create()
        .with_window(&target_window)
        .build();
    let config = live_capture_configuration(dimensions);

    let mut stream = SCStream::new(&filter, &config);
    stream
        .add_output_handler(
            move |sample: screencapturekit::cm::CMSampleBuffer,
                  output_type: SCStreamOutputType| {
                if output_type != SCStreamOutputType::Screen
                    || cancelled.load(Ordering::Acquire)
                    || sample
                        .frame_status()
                        .is_some_and(|status| !status.has_content())
                    || frame_pending.swap(true, Ordering::AcqRel)
                {
                    return;
                }
                match sample.cg_image() {
                    Ok(image) => {
                        dispatch_to_main(
                            LiveFrame {
                                app_pid,
                                target_pid,
                                window_id,
                                image: LiveFrameImage::CgImage(image),
                                frame_pending: Arc::clone(&frame_pending),
                                cancelled: Arc::clone(&cancelled),
                            },
                            push_live_frame_cb,
                        )
                    }
                    Err(error) => {
                        frame_pending.store(false, Ordering::Release);
                        tracing::debug!(target: "pip", app_pid, target_pid, window_id, error, "live PiP frame had no image");
                    }
                }
            },
            SCStreamOutputType::Screen,
        )
        .ok_or_else(|| anyhow::anyhow!("ScreenCaptureKit rejected the PiP output handler"))?;
    stream
        .start_capture()
        .map_err(|error| anyhow::anyhow!("SCStream::start_capture failed: {error}"))?;
    Ok((stream, dimensions))
}

fn screen_capture_window_matches_target(
    window_id: u32,
    owner_pid: Option<i32>,
    expected_window_id: u32,
    expected_pid: i32,
) -> bool {
    window_id == expected_window_id && owner_pid == Some(expected_pid)
}

fn physical_target_is_live(target_pid: i64, window_id: u64) -> bool {
    let (Ok(target_pid), Ok(window_id)) = (i32::try_from(target_pid), u32::try_from(window_id))
    else {
        return false;
    };
    crate::windows::window_info_by_id(window_id)
        .is_some_and(|window| window.pid == target_pid && window.window_id == window_id)
}

fn stop_live_capture_for(pid: i64) {
    let entry = LIVE_STREAMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&pid);
    if let Some(entry) = entry {
        entry.cancelled.store(true, Ordering::Release);
        entry.frame_pending.store(false, Ordering::Release);
        if let Some(stream) = entry.stream {
            // SCStream::stop_capture is a blocking wait. Foreground visibility
            // reconciliation runs on AppKit's main queue, so stopping inline
            // can freeze both PiP and the shared cursor UI pump. Cancellation
            // already prevents late frames from being rendered; finish the
            // native teardown away from the main thread.
            if let Err(error) = std::thread::Builder::new()
                .name(format!("cua-pip-stop-{pid}"))
                .spawn(move || {
                    if let Err(error) = stream.stop_capture() {
                        tracing::debug!(target: "pip", pid, %error, "failed to stop app PiP capture cleanly");
                    }
                })
            {
                tracing::warn!(target: "pip", pid, %error, "failed to schedule app PiP capture teardown");
            }
        }
    }
}

fn stop_all_live_capture() {
    let pids = LIVE_STREAMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .keys()
        .copied()
        .collect::<Vec<_>>();
    for pid in pids {
        stop_live_capture_for(pid);
    }
}

unsafe extern "C" fn push_live_frame_cb(ctx: *mut c_void) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSSize;

    let frame: LiveFrame = *Box::from_raw(ctx as *mut LiveFrame);
    frame.frame_pending.store(false, Ordering::Release);
    if frame.cancelled.load(Ordering::Acquire) {
        return;
    }
    let model_target = VIEW_MODEL
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|model| model.frame_for_app(frame.app_pid))
        .map(|model_frame| model_frame.target.clone());
    let Some(model_target) = model_target else {
        return;
    };
    if HIDDEN_APPS.lock().unwrap().contains(&frame.app_pid)
        || !pip_target_session_is_live(&model_target)
        || !delegation_frame_proof_is_live(&model_target)
        || model_target.pid != frame.target_pid
        || model_target.window_id != frame.window_id
    {
        return;
    }
    let handles = CARD_HANDLES.lock().unwrap().get(&frame.app_pid).copied();
    let Some(handles) = handles else {
        return;
    };
    let image_view = handles.image_view as *mut AnyObject;

    let image: *mut AnyObject = match frame.image {
        LiveFrameImage::CgImage(image) => {
            let cg_image = image.as_ptr() as *mut NativeCGImage;
            let allocated: *mut AnyObject = msg_send![objc2::class!(NSImage), alloc];
            msg_send![allocated, initWithCGImage: cg_image size: NSSize::new(0.0, 0.0)]
        }
    };
    if !image.is_null() {
        let size: NSSize = msg_send![image, size];
        resize_card_for_image(frame.app_pid, handles, size);
        let _: () = msg_send![image_view, setImage: image];
        let _: () = msg_send![image, release];
        if let Some(current) = CARD_HANDLES.lock().unwrap().get_mut(&frame.app_pid) {
            current.has_live_frame = true;
        }
    }
}

fn pip_card_view_class() -> &'static objc2::runtime::AnyClass {
    use objc2::class;
    use objc2::declare::ClassBuilder;

    static CLASS: OnceLock<&'static objc2::runtime::AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("CuaDriverPipCardView", class!(NSView))
            .expect("CuaDriverPipCardView already registered");
        unsafe {
            builder.add_method(
                objc2::sel!(acceptsFirstMouse:),
                accepts_first_mouse as extern "C" fn(_, _, _) -> objc2::runtime::Bool,
            );
            builder.add_method(
                objc2::sel!(mouseEntered:),
                card_mouse_entered as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(mouseExited:),
                card_mouse_exited as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(mouseMoved:),
                card_mouse_entered as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(cursorUpdate:),
                card_mouse_entered as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(mouseDown:),
                card_mouse_down as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(mouseDragged:),
                card_mouse_dragged as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(mouseUp:),
                card_mouse_up as extern "C" fn(_, _, _),
            );
        }
        builder.register()
    })
}

fn pip_canvas_view_class() -> &'static objc2::runtime::AnyClass {
    use objc2::class;
    use objc2::declare::ClassBuilder;

    static CLASS: OnceLock<&'static objc2::runtime::AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("CuaDriverPipCanvasView", class!(NSView))
            .expect("CuaDriverPipCanvasView already registered");
        unsafe {
            builder.add_method(
                objc2::sel!(acceptsFirstMouse:),
                accepts_first_mouse as extern "C" fn(_, _, _) -> objc2::runtime::Bool,
            );
            builder.add_method(
                objc2::sel!(mouseEntered:),
                canvas_mouse_moved as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(mouseExited:),
                canvas_mouse_moved as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(mouseMoved:),
                canvas_mouse_moved as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(cursorUpdate:),
                canvas_mouse_moved as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(resetCursorRects),
                canvas_reset_cursor_rects as extern "C" fn(_, _),
            );
        }
        builder.register()
    })
}

extern "C" fn accepts_first_mouse(
    _view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    _event: *mut objc2::runtime::AnyObject,
) -> objc2::runtime::Bool {
    objc2::runtime::Bool::YES
}

fn set_hovered_app(pid: Option<i64>) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    {
        let mut hovered = HOVERED_APP.lock().unwrap();
        if *hovered == pid {
            return;
        }
        *hovered = pid;
    }

    let front_pid = VIEW_MODEL.lock().unwrap().as_ref().and_then(|model| {
        model
            .ordered_frames()
            .last()
            .map(|frame| frame.target.app_key_pid())
    });
    let handles = CARD_HANDLES.lock().unwrap().clone();
    for (card_pid, card_handles) in handles {
        let controls = card_handles.controls as *mut AnyObject;
        let card = card_handles.card as *mut AnyObject;
        if !controls.is_null() {
            unsafe {
                let hidden = Some(card_pid) != pid || Some(card_pid) != front_pid;
                let _: () = msg_send![controls, setHidden: hidden];
            }
        }
        if !card.is_null() {
            let lift = Some(card_pid) == pid && Some(card_pid) != front_pid;
            let rect = card_handles.resting_rect;
            let frame = NSRect::new(
                NSPoint::new(
                    rect.x + if lift { STACK_HOVER_LIFT_X } else { 0.0 },
                    rect.y + if lift { STACK_HOVER_LIFT_Y } else { 0.0 },
                ),
                NSSize::new(rect.width, rect.height),
            );
            unsafe {
                let animator: *mut AnyObject = msg_send![card, animator];
                let _: () = msg_send![animator, setFrame: frame];
            }
        }
    }
}

unsafe fn hovered_pid_at_event(
    view: *mut objc2::runtime::AnyObject,
    event: *mut objc2::runtime::AnyObject,
) -> Option<i64> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSPoint;

    let window: *mut AnyObject = msg_send![view, window];
    if window.is_null() {
        return None;
    }
    let content: *mut AnyObject = msg_send![window, contentView];
    if content.is_null() {
        return None;
    }
    let point: NSPoint = msg_send![event, locationInWindow];
    let point: NSPoint =
        msg_send![content, convertPoint: point fromView: std::ptr::null_mut::<AnyObject>()];
    hovered_pid_at_content_point(content, point)
}

unsafe fn hovered_pid_at_content_point(
    content: *mut objc2::runtime::AnyObject,
    point: objc2_foundation::NSPoint,
) -> Option<i64> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let hit: *mut AnyObject = msg_send![content, hitTest: point];
    let mut candidate = hit;
    while !candidate.is_null() {
        if let Some(pid) = CARD_VIEW_PIDS
            .lock()
            .unwrap()
            .get(&(candidate as usize))
            .copied()
        {
            return Some(pid);
        }
        candidate = msg_send![candidate, superview];
    }
    None
}

fn rendered_click_target(
    cards: &[(pip_preview::PipTarget, CardRect)],
    pid: Option<i64>,
) -> (Option<ClickedTarget>, Option<i64>) {
    let target = pid.and_then(|pid| {
        cards
            .iter()
            .find(|(target, _)| target.app_key_pid() == pid)
            .map(|(target, rect)| ClickedTarget {
                target: target.clone(),
                layout_rect: *rect,
            })
    });
    (target, cards.last().map(|(target, _)| target.app_key_pid()))
}

fn clicked_presentation_is_current(
    clicked: &ClickedTarget,
    cards: &[(pip_preview::PipTarget, CardRect)],
) -> bool {
    cards.iter().any(|(target, rect)| {
        same_observation_target(target, &clicked.target) && *rect == clicked.layout_rect
    })
}

fn clicked_target_is_retained(target: &pip_preview::PipTarget) -> bool {
    VIEW_MODEL.lock().unwrap().as_ref().is_some_and(|model| {
        model.accepts_target(target)
            && model
                .frame_for_app(target.app_key_pid())
                .is_some_and(|frame| {
                    same_observation_target(&frame.target, target)
                        && frame_has_preview_authority_without_model(frame)
                })
    }) && !HIDDEN_APPS.lock().unwrap().contains(&target.app_key_pid())
}

fn frame_has_preview_authority_without_model(frame: &PipFrame) -> bool {
    pip_target_session_is_live(&frame.target) && delegation_frame_proof_is_live(&frame.target)
}

fn activate_target_window(clicked: ClickedTarget) {
    if !clicked_presentation_is_current(&clicked, &RENDERED_CARDS.lock().unwrap())
        || !clicked_target_is_retained(&clicked.target)
    {
        return;
    }
    let generation = CARD_ACTIVATION_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let app_pid = clicked.target.app_key_pid();
    let window_id = clicked.target.window_id;
    // AX messages and activation settling must not stall the shared AppKit/
    // cursor pump. Bind the worker to the actual presentation clicked, never
    // look up another window by app name, and cancel it on a later click.
    if let Err(error) = std::thread::Builder::new()
        .name(format!("cua-pip-activate-{app_pid}"))
        .spawn(move || {
            let target = clicked.target;
            let validate = || {
                CARD_ACTIVATION_GENERATION.load(Ordering::Acquire) == generation
                    && clicked_target_is_retained(&target)
                    && physical_target_is_live(target.pid, target.window_id)
                    && pip_delegation_is_live(&target)
            };
            let action_window_id = if is_application_menu_target(&target) {
                pip_application_menu_image(&target).map(|menu| u64::from(menu.document_window_id))
            } else {
                Some(target.window_id)
            };
            let result = action_window_id
                .ok_or_else(|| anyhow::anyhow!("menu source expired"))
                .and_then(|window_id| {
                    window_activation::activate(app_pid, target.pid, window_id, validate)
                });
            let (status, error) = match result {
                Ok(()) => ("confirmed", None),
                Err(error) => {
                    tracing::warn!(target: "pip", app_pid, window_id, %error,
                        "exact PiP card activation was not confirmed");
                    ("unconfirmed", Some(error.to_string()))
                }
            };
            if CARD_ACTIVATION_GENERATION.load(Ordering::Acquire) == generation {
                *LAST_CARD_ACTIVATION.lock().unwrap() = Some(serde_json::json!({
                    "app_pid": app_pid, "source_pid": target.pid, "window_id": window_id,
                    "status": status, "error": error,
                }));
            }
            schedule_foreground_visibility_refresh();
        })
    {
        tracing::warn!(target: "pip", app_pid, window_id, %error, "could not start PiP activation worker");
    }
}

fn pointer_gesture_is_click(
    start_mouse: objc2_foundation::NSPoint,
    end_mouse: objc2_foundation::NSPoint,
    start_window_origin: objc2_foundation::NSPoint,
    end_window_origin: objc2_foundation::NSPoint,
) -> bool {
    let pointer_distance = (end_mouse.x - start_mouse.x).hypot(end_mouse.y - start_mouse.y);
    let window_distance = (end_window_origin.x - start_window_origin.x)
        .hypot(end_window_origin.y - start_window_origin.y);
    pointer_distance < CLICK_DRAG_THRESHOLD && window_distance < CLICK_DRAG_THRESHOLD
}

unsafe extern "C" fn promote_clicked_app_cb(ctx: *mut c_void) {
    let target = *Box::from_raw(ctx as *mut ClickedTarget);
    if !clicked_presentation_is_current(&target, &RENDERED_CARDS.lock().unwrap())
        || !clicked_target_is_retained(&target.target)
    {
        return;
    }
    let snapshot = {
        let mut model = VIEW_MODEL.lock().unwrap();
        let Some(model) = model.as_mut() else {
            return;
        };
        model.promote_app(target.target.app_key_pid()).then(|| {
            model
                .ordered_frames()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        })
    };
    HOVERED_APP.lock().unwrap().take();
    if let Some(snapshot) = snapshot {
        render_snapshot(&snapshot);
    }
}

extern "C" fn card_mouse_entered(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    event: *mut objc2::runtime::AnyObject,
) {
    if view.is_null() || event.is_null() {
        return;
    }
    let pid = unsafe { hovered_pid_at_event(view, event) };
    set_hovered_app(pid);
    unsafe { refresh_cursor_at_event(view, event) };
}

extern "C" fn card_mouse_exited(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    event: *mut objc2::runtime::AnyObject,
) {
    if view.is_null() || event.is_null() {
        return;
    }
    let pid = unsafe { hovered_pid_at_event(view, event) };
    set_hovered_app(pid);
    unsafe { refresh_cursor_at_event(view, event) };
}

extern "C" fn canvas_mouse_moved(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    event: *mut objc2::runtime::AnyObject,
) {
    if view.is_null() || event.is_null() {
        return;
    }
    let pid = unsafe { hovered_pid_at_event(view, event) };
    set_hovered_app(pid);
    unsafe { refresh_cursor_at_event(view, event) };
}

unsafe fn refresh_cursor_at_event(
    view: *mut objc2::runtime::AnyObject,
    event: *mut objc2::runtime::AnyObject,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let window: *mut AnyObject = msg_send![view, window];
    if window.is_null() {
        return;
    }
    let content: *mut AnyObject = msg_send![window, contentView];
    if content.is_null() {
        return;
    }
    let window_point: objc2_foundation::NSPoint = msg_send![event, locationInWindow];
    let content_point: objc2_foundation::NSPoint = msg_send![
        content,
        convertPoint: window_point
        fromView: std::ptr::null_mut::<AnyObject>()
    ];
    refresh_cursor_at_content_point(window, content_point);
}

unsafe fn refresh_cursor_at_content_point(
    window: *mut objc2::runtime::AnyObject,
    location: objc2_foundation::NSPoint,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let content: *mut AnyObject = msg_send![window, contentView];
    if content.is_null() {
        hide_custom_cursor();
        return;
    }
    let hit: *mut AnyObject = msg_send![content, hitTest: location];
    let mut candidate = hit;
    while !candidate.is_null() {
        if let Some(direction) = RESIZE_VIEW_DIRECTIONS
            .lock()
            .unwrap()
            .get(&(candidate as usize))
            .copied()
        {
            show_custom_cursor(window, location, resize_cursor_for_direction(direction));
            return;
        }
        if let Some(pid) = CARD_VIEW_PIDS
            .lock()
            .unwrap()
            .get(&(candidate as usize))
            .copied()
        {
            let front_pid = VIEW_MODEL.lock().unwrap().as_ref().and_then(|model| {
                model
                    .ordered_frames()
                    .last()
                    .map(|frame| frame.target.app_key_pid())
            });
            let cursor: *mut AnyObject = if Some(pid) == front_pid {
                msg_send![objc2::class!(NSCursor), pointingHandCursor]
            } else {
                std::ptr::null_mut()
            };
            if cursor.is_null() {
                hide_custom_cursor();
            } else {
                show_custom_cursor(window, location, cursor);
            }
            return;
        }
        candidate = msg_send![candidate, superview];
    }
    hide_custom_cursor();
}

extern "C" fn canvas_reset_cursor_rects(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    if view.is_null() {
        return;
    }
    unsafe {
        let _: () = msg_send![view, discardCursorRects];
        let bounds: objc2_foundation::NSRect = msg_send![view, bounds];
        let layout = visible_card_layout();
        let hand: *mut AnyObject = msg_send![objc2::class!(NSCursor), pointingHandCursor];

        if let Some(front) = layout.last() {
            let inset = RESIZE_HIT_INSET;
            let front_rect = NSRect::new(
                NSPoint::new(front.x + inset, front.y + inset),
                NSSize::new(
                    (front.width - inset * 2.0).max(0.0),
                    (front.height - inset * 2.0).max(0.0),
                ),
            );
            let _: () = msg_send![view, addCursorRect: front_rect cursor: hand];
        }

        for (rect, direction) in resize_cursor_rects(bounds, &layout) {
            let cursor = resize_cursor_for_direction(direction);
            if !cursor.is_null() {
                let _: () = msg_send![view, addCursorRect: rect cursor: cursor];
            }
        }
    }
}

unsafe fn show_custom_cursor(
    _owner_window: *mut objc2::runtime::AnyObject,
    _location: objc2_foundation::NSPoint,
    cursor: *mut objc2::runtime::AnyObject,
) {
    use objc2::msg_send;

    if cursor.is_null() {
        return;
    }
    // Never hide or obscure the foreground application's cursor from this
    // non-activating panel. AppKit cursor updates are safe even if the active
    // application later replaces the requested image.
    let _: () = msg_send![cursor, set];
}

fn schedule_cursor_refresh() {
    if !CURSOR_REFRESH_PENDING.swap(true, Ordering::SeqCst) {
        dispatch_to_main((), cursor_refresh_cb);
    }
}

unsafe fn current_pip_cursor_location(
) -> Option<(*mut objc2::runtime::AnyObject, objc2_foundation::NSPoint)> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let window = HANDLES
        .lock()
        .unwrap()
        .as_ref()
        .map(|handles| handles.window)
        .unwrap_or(0) as *mut AnyObject;
    if window.is_null() {
        return None;
    }
    let visible: objc2::runtime::Bool = msg_send![window, isVisible];
    if !visible.as_bool() {
        return None;
    }
    let content: *mut AnyObject = msg_send![window, contentView];
    if content.is_null() {
        return None;
    }
    let window_point: objc2_foundation::NSPoint =
        msg_send![window, mouseLocationOutsideOfEventStream];
    let content_point: objc2_foundation::NSPoint = msg_send![
        content,
        convertPoint: window_point
        fromView: std::ptr::null_mut::<AnyObject>()
    ];
    let bounds: objc2_foundation::NSRect = msg_send![content, bounds];
    let inside = content_point.x >= bounds.origin.x
        && content_point.x < bounds.origin.x + bounds.size.width
        && content_point.y >= bounds.origin.y
        && content_point.y < bounds.origin.y + bounds.size.height;
    inside.then_some((window, content_point))
}

unsafe extern "C" fn cursor_refresh_cb(ctx: *mut c_void) {
    drop(Box::from_raw(ctx as *mut ()));
    CURSOR_REFRESH_PENDING.store(false, Ordering::SeqCst);
    let Some((window, location)) = current_pip_cursor_location() else {
        hide_custom_cursor();
        return;
    };
    refresh_cursor_at_content_point(window, location);
}

unsafe fn install_mouse_monitors() -> (usize, usize) {
    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    // Observe only event kind/source, never text. A physical/user click or key
    // must release a temporary presentation hold even if it targets the same
    // already-assisted app (which emits no new activation notification).
    let event_mask = (1u64 << 5) | (1 << 1) | (1 << 3) | (1 << 25) | (1 << 10);
    let block = RcBlock::new(move |event: *mut AnyObject| {
        observe_pip_input(event);
    });
    let global_monitor: *mut AnyObject = msg_send![
        objc2::class!(NSEvent),
        addGlobalMonitorForEventsMatchingMask: event_mask
        handler: &*block
    ];
    let local_block = RcBlock::new(move |event: *mut AnyObject| -> *mut AnyObject {
        observe_pip_input(event);
        event
    });
    let local_monitor: *mut AnyObject = msg_send![
        objc2::class!(NSEvent),
        addLocalMonitorForEventsMatchingMask: event_mask
        handler: &*local_block
    ];
    (global_monitor as usize, local_monitor as usize)
}

fn input_cancels_temporary_activation(
    event_type: u64,
    source_pid: Option<i64>,
    driver_pid: i64,
) -> bool {
    matches!(event_type, 1 | 3 | 25 | 10) && source_pid != Some(driver_pid)
}

unsafe fn observe_pip_input(event: *mut objc2::runtime::AnyObject) {
    use objc2::msg_send;
    extern "C" {
        fn CGEventGetIntegerValueField(event: *const c_void, field: u32) -> i64;
    }
    if event.is_null() {
        return;
    }
    let event_type: u64 = msg_send![event, type];
    if event_type == 5 {
        schedule_cursor_refresh();
        return;
    }
    let cg_event: *const c_void = msg_send![event, CGEvent];
    // kCGEventSourceUnixProcessID = 41. Unknown origin is not attributed to us.
    let source_pid = (!cg_event.is_null()).then(|| CGEventGetIntegerValueField(cg_event, 41));
    if input_cancels_temporary_activation(event_type, source_pid, i64::from(std::process::id())) {
        let changed = {
            let mut state = FOREGROUND_VISIBILITY_STATE.lock().unwrap();
            let changed = state.temporary_activations.cancel_all();
            if changed {
                EXTERNAL_INPUT_HOLD_CANCELLATIONS.fetch_add(1, Ordering::Relaxed);
                state.invalidate_pending();
            }
            changed
        };
        if changed {
            LAST_FOREGROUND_CHECK_MS.store(0, Ordering::Release);
            schedule_foreground_visibility_refresh();
        }
    }
}

fn hide_custom_cursor() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    unsafe {
        let arrow: *mut AnyObject = msg_send![objc2::class!(NSCursor), arrowCursor];
        if !arrow.is_null() {
            let _: () = msg_send![arrow, set];
        }
    }
}

extern "C" fn card_mouse_down(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    event: *mut objc2::runtime::AnyObject,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    if view.is_null() || event.is_null() {
        return;
    }
    unsafe {
        let pid = hovered_pid_at_event(view, event);
        // The model may be ahead of the main-queue render, or its last card
        // may be hidden because its source is foreground. Hit-test only the
        // published presentation, including its actual visible front card.
        let (target, front_pid) = rendered_click_target(&RENDERED_CARDS.lock().unwrap(), pid);
        let window: *mut AnyObject = msg_send![view, window];
        if !window.is_null() {
            let start_mouse: objc2_foundation::NSPoint =
                msg_send![objc2::class!(NSEvent), mouseLocation];
            let start_frame: objc2_foundation::NSRect = msg_send![window, frame];
            *CARD_GESTURE.lock().unwrap() = Some(CardGesture {
                target,
                front_pid,
                start_mouse,
                start_window_origin: start_frame.origin,
                dragged: false,
            });
        }
    }
}

extern "C" fn card_mouse_dragged(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    _event: *mut objc2::runtime::AnyObject,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSPoint;

    if view.is_null() {
        return;
    }
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        if window.is_null() {
            return;
        }
        let mouse: NSPoint = msg_send![objc2::class!(NSEvent), mouseLocation];
        let mut gesture = CARD_GESTURE.lock().unwrap();
        let Some(gesture) = gesture.as_mut() else {
            return;
        };
        let delta_x = mouse.x - gesture.start_mouse.x;
        let delta_y = mouse.y - gesture.start_mouse.y;
        if !gesture.dragged && delta_x.hypot(delta_y) < CLICK_DRAG_THRESHOLD {
            return;
        }
        if !gesture.dragged {
            gesture.dragged = true;
            hide_custom_cursor();
        }
        let origin = NSPoint::new(
            gesture.start_window_origin.x + delta_x,
            gesture.start_window_origin.y + delta_y,
        );
        let _: () = msg_send![window, setFrameOrigin: origin];
    }
}

extern "C" fn card_mouse_up(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    _event: *mut objc2::runtime::AnyObject,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    if view.is_null() {
        CARD_GESTURE.lock().unwrap().take();
        return;
    }
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        let Some(gesture) = CARD_GESTURE.lock().unwrap().take() else {
            return;
        };
        if window.is_null() {
            return;
        }
        let end_mouse: objc2_foundation::NSPoint = msg_send![objc2::class!(NSEvent), mouseLocation];
        let end_frame: objc2_foundation::NSRect = msg_send![window, frame];
        let clicked = !gesture.dragged
            && pointer_gesture_is_click(
                gesture.start_mouse,
                end_mouse,
                gesture.start_window_origin,
                end_frame.origin,
            );
        if clicked {
            if let Some(target) = gesture.target {
                if Some(target.target.app_key_pid()) == gesture.front_pid {
                    activate_target_window(target);
                } else {
                    dispatch_to_main(target, promote_clicked_app_cb);
                }
            }
        }
        if let Some((window, location)) = current_pip_cursor_location() {
            refresh_cursor_at_content_point(window, location);
        }
    }
}

fn pip_delegate_class() -> &'static objc2::runtime::AnyClass {
    use objc2::class;
    use objc2::declare::ClassBuilder;

    static CLASS: OnceLock<&'static objc2::runtime::AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("CuaDriverPipDelegate", class!(NSObject))
            .expect("CuaDriverPipDelegate already registered");
        unsafe {
            builder.add_method(
                objc2::sel!(windowDidResize:),
                window_did_resize as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(minimizePip:),
                minimize_pip as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(workspaceDidActivate:),
                workspace_did_activate as extern "C" fn(_, _, _),
            );
        }
        builder.register()
    })
}

extern "C" fn workspace_did_activate(
    _delegate: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    _notification: *mut objc2::runtime::AnyObject,
) {
    // App activation is the authoritative transition for application-level
    // suppression. Schedule immediately, but keep WindowServer/AX/signature
    // validation off AppKit's main thread.
    FOREGROUND_VISIBILITY_STATE
        .lock()
        .unwrap()
        .invalidate_pending();
    LAST_FOREGROUND_CHECK_MS.store(0, Ordering::Release);
    schedule_foreground_visibility_refresh();
}

unsafe fn pip_delegate_instance() -> *mut objc2::runtime::AnyObject {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let allocated: *mut AnyObject = msg_send![pip_delegate_class(), alloc];
    msg_send![allocated, init]
}

extern "C" fn window_did_resize(
    _delegate: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    _notification: *mut objc2::runtime::AnyObject,
) {
    let snapshot = current_snapshot();
    unsafe { render_snapshot(&snapshot) };
}

extern "C" fn minimize_pip(
    _delegate: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    sender: *mut objc2::runtime::AnyObject,
) {
    if sender.is_null() {
        return;
    }
    HOVERED_APP.lock().unwrap().take();
    hide_custom_cursor();
    unsafe {
        let window: *mut objc2::runtime::AnyObject = objc2::msg_send![sender, window];
        if !window.is_null() {
            let _: () = objc2::msg_send![window, orderOut: std::ptr::null_mut::<
                objc2::runtime::AnyObject,
            >()];
        }
    }
}

fn resize_hit_view_class() -> &'static objc2::runtime::AnyClass {
    use objc2::class;
    use objc2::declare::ClassBuilder;

    static CLASS: OnceLock<&'static objc2::runtime::AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("CuaDriverPipResizeView", class!(NSView))
            .expect("CuaDriverPipResizeView already registered");
        unsafe {
            builder.add_method(
                objc2::sel!(acceptsFirstMouse:),
                accepts_first_mouse as extern "C" fn(_, _, _) -> objc2::runtime::Bool,
            );
            builder.add_method(
                objc2::sel!(mouseDown:),
                resize_mouse_down as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(mouseDownCanMoveWindow),
                mouse_down_cannot_move_window as extern "C" fn(_, _) -> objc2::runtime::Bool,
            );
            builder.add_method(
                objc2::sel!(mouseEntered:),
                refresh_resize_cursor as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(mouseMoved:),
                refresh_resize_cursor as extern "C" fn(_, _, _),
            );
            builder.add_method(
                objc2::sel!(cursorUpdate:),
                refresh_resize_cursor as extern "C" fn(_, _, _),
            );
        }
        builder.register()
    })
}

extern "C" fn refresh_resize_cursor(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    event: *mut objc2::runtime::AnyObject,
) {
    if view.is_null() || event.is_null() {
        return;
    }
    unsafe {
        let window: *mut objc2::runtime::AnyObject = objc2::msg_send![view, window];
        if window.is_null() {
            return;
        }
        let direction = RESIZE_VIEW_DIRECTIONS
            .lock()
            .unwrap()
            .get(&(view as usize))
            .copied()
            .unwrap_or(0);
        let location: objc2_foundation::NSPoint = objc2::msg_send![event, locationInWindow];
        show_custom_cursor(window, location, resize_cursor_for_direction(direction));
    }
}

extern "C" fn mouse_down_cannot_move_window(
    _view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
) -> objc2::runtime::Bool {
    objc2::runtime::Bool::NO
}

unsafe fn resize_cursor_for_direction(direction: isize) -> *mut objc2::runtime::AnyObject {
    use objc2::msg_send;

    let cursor_class = objc2::class!(NSCursor);
    let is_horizontal = direction & (RESIZE_LEFT | RESIZE_RIGHT) != 0;
    let is_vertical = direction & (RESIZE_TOP | RESIZE_BOTTOM) != 0;
    if is_horizontal && is_vertical {
        return if direction == (RESIZE_LEFT | RESIZE_TOP)
            || direction == (RESIZE_RIGHT | RESIZE_BOTTOM)
        {
            msg_send![cursor_class, _windowResizeNorthWestSouthEastCursor]
        } else {
            msg_send![cursor_class, _windowResizeNorthEastSouthWestCursor]
        };
    }
    let modern_selector = objc2::sel!(frameResizeCursorFromPosition:inDirections:);
    let supports_frame_cursor: objc2::runtime::Bool =
        msg_send![cursor_class, respondsToSelector: modern_selector];
    if supports_frame_cursor.as_bool() {
        let mut position = 0u64;
        if direction & RESIZE_TOP != 0 {
            position |= 1;
        }
        if direction & RESIZE_LEFT != 0 {
            position |= 2;
        }
        if direction & RESIZE_BOTTOM != 0 {
            position |= 4;
        }
        if direction & RESIZE_RIGHT != 0 {
            position |= 8;
        }
        return msg_send![
            cursor_class,
            frameResizeCursorFromPosition: position
            inDirections: 3u64
        ];
    }
    if direction & (RESIZE_LEFT | RESIZE_RIGHT) != 0
        && direction & (RESIZE_TOP | RESIZE_BOTTOM) == 0
    {
        msg_send![cursor_class, resizeLeftRightCursor]
    } else if direction & (RESIZE_TOP | RESIZE_BOTTOM) != 0
        && direction & (RESIZE_LEFT | RESIZE_RIGHT) == 0
    {
        msg_send![cursor_class, resizeUpDownCursor]
    } else {
        msg_send![cursor_class, crosshairCursor]
    }
}

fn resized_window_frame(
    start: objc2_foundation::NSRect,
    delta_x: f64,
    delta_y: f64,
    direction: isize,
    minimum: objc2_foundation::NSSize,
) -> objc2_foundation::NSRect {
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let mut x = start.origin.x;
    let mut y = start.origin.y;
    let mut width = start.size.width;
    let mut height = start.size.height;
    if direction & RESIZE_LEFT != 0 {
        let applied = delta_x.min(width - minimum.width);
        x += applied;
        width -= applied;
    }
    if direction & RESIZE_RIGHT != 0 {
        width = (width + delta_x).max(minimum.width);
    }
    if direction & RESIZE_BOTTOM != 0 {
        let applied = delta_y.min(height - minimum.height);
        y += applied;
        height -= applied;
    }
    if direction & RESIZE_TOP != 0 {
        height = (height + delta_y).max(minimum.height);
    }
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

extern "C" fn resize_mouse_down(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    event: *mut objc2::runtime::AnyObject,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    if view.is_null() || event.is_null() {
        return;
    }
    unsafe {
        let window: *mut AnyObject = msg_send![view, window];
        if window.is_null() {
            return;
        }
        let direction = RESIZE_VIEW_DIRECTIONS
            .lock()
            .unwrap()
            .get(&(view as usize))
            .copied()
            .unwrap_or(0);
        let start_frame: objc2_foundation::NSRect = msg_send![window, frame];
        let minimum: objc2_foundation::NSSize = msg_send![window, minSize];
        let start_mouse: objc2_foundation::NSPoint =
            msg_send![objc2::class!(NSEvent), mouseLocation];
        let event_mask: u64 = (1 << 2) | (1 << 6);
        let distant_future: *mut AnyObject = msg_send![objc2::class!(NSDate), distantFuture];
        let default_mode = ns_string("kCFRunLoopDefaultMode");
        loop {
            let next: *mut AnyObject = msg_send![
                window,
                nextEventMatchingMask: event_mask
                untilDate: distant_future
                inMode: default_mode
                dequeue: true
            ];
            if next.is_null() {
                break;
            }
            let event_type: usize = msg_send![next, type];
            if event_type == 2 {
                break;
            }
            let mouse: objc2_foundation::NSPoint = msg_send![objc2::class!(NSEvent), mouseLocation];
            let frame = resized_window_frame(
                start_frame,
                mouse.x - start_mouse.x,
                mouse.y - start_mouse.y,
                direction,
                minimum,
            );
            let _: () = msg_send![window, setFrame: frame display: true];
            let location: objc2_foundation::NSPoint = msg_send![next, locationInWindow];
            show_custom_cursor(window, location, resize_cursor_for_direction(direction));
        }
    }
}

unsafe fn add_resize_hit_view(
    parent: *mut objc2::runtime::AnyObject,
    frame: objc2_foundation::NSRect,
    direction: isize,
    autoresizing_mask: u64,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let allocated: *mut AnyObject = msg_send![resize_hit_view_class(), alloc];
    let view: *mut AnyObject = msg_send![allocated, initWithFrame: frame];
    RESIZE_VIEW_DIRECTIONS
        .lock()
        .unwrap()
        .insert(view as usize, direction);
    let _: () = msg_send![view, setAutoresizingMask: autoresizing_mask];
    let _: () = msg_send![view, setWantsLayer: true];
    let layer: *mut AnyObject = msg_send![view, layer];
    set_layer_background(layer, color(0.0, 0.0, 0.0, 0.001));
    let _: () = msg_send![
        parent,
        addSubview: view
        positioned: 1i64
        relativeTo: std::ptr::null_mut::<AnyObject>()
    ];
    let tracking_options: u64 = 0x1 | 0x2 | 0x4 | 0x80 | 0x200 | 0x400;
    let tracking_allocated: *mut AnyObject = msg_send![objc2::class!(NSTrackingArea), alloc];
    let tracking: *mut AnyObject = msg_send![
        tracking_allocated,
        initWithRect: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
        options: tracking_options
        owner: view
        userInfo: std::ptr::null_mut::<AnyObject>()
    ];
    let _: () = msg_send![view, addTrackingArea: tracking];
    let _: () = msg_send![tracking, release];
}

unsafe fn install_resize_hit_views(
    canvas: *mut objc2::runtime::AnyObject,
    bounds: objc2_foundation::NSRect,
    layout: &[CardRect],
) {
    for (rect, direction) in resize_cursor_rects(bounds, layout) {
        add_resize_hit_view(canvas, rect, direction, 0);
    }
}

fn resize_cursor_rects(
    bounds: objc2_foundation::NSRect,
    layout: &[CardRect],
) -> Vec<(objc2_foundation::NSRect, isize)> {
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let Some(first) = layout.first() else {
        return Vec::new();
    };
    // Cards now retain independent source aspect ratios. Any card, not just
    // the front/back pair, can define an outer edge of the visible stack.
    let (left, bottom, right, top) = layout.iter().fold(
        (
            first.x,
            first.y,
            first.x + first.width,
            first.y + first.height,
        ),
        |(left, bottom, right, top), rect| {
            (
                left.min(rect.x),
                bottom.min(rect.y),
                right.max(rect.x + rect.width),
                top.max(rect.y + rect.height),
            )
        },
    );
    let edge = RESIZE_HIT_INSET;
    let half = edge / 2.0;
    let clamp_x = |x: f64| x.clamp(0.0, (bounds.size.width - edge).max(0.0));
    let clamp_y = |y: f64| y.clamp(0.0, (bounds.size.height - edge).max(0.0));
    vec![
        (
            NSRect::new(
                NSPoint::new(clamp_x(left - half), clamp_y(bottom - half)),
                NSSize::new(edge, edge),
            ),
            RESIZE_LEFT | RESIZE_BOTTOM,
        ),
        (
            NSRect::new(
                NSPoint::new(clamp_x(right - half), clamp_y(bottom - half)),
                NSSize::new(edge, edge),
            ),
            RESIZE_RIGHT | RESIZE_BOTTOM,
        ),
        (
            NSRect::new(
                NSPoint::new(clamp_x(left - half), clamp_y(top - half)),
                NSSize::new(edge, edge),
            ),
            RESIZE_LEFT | RESIZE_TOP,
        ),
        (
            NSRect::new(
                NSPoint::new(clamp_x(right - half), clamp_y(top - half)),
                NSSize::new(edge, edge),
            ),
            RESIZE_RIGHT | RESIZE_TOP,
        ),
    ]
}

unsafe fn ns_string(value: &str) -> *mut objc2::runtime::AnyObject {
    use objc2::{class, msg_send};

    let sanitized = value.replace('\0', " ");
    let Ok(cstr) = std::ffi::CString::new(sanitized) else {
        return std::ptr::null_mut();
    };
    msg_send![class!(NSString), stringWithUTF8String: cstr.as_ptr() as *const u8]
}

unsafe fn color(red: f64, green: f64, blue: f64, alpha: f64) -> *mut objc2::runtime::AnyObject {
    use objc2::{class, msg_send};

    msg_send![
        class!(NSColor),
        colorWithCalibratedRed: red
        green: green
        blue: blue
        alpha: alpha
    ]
}

unsafe fn vibrant_dark_appearance() -> *mut objc2::runtime::AnyObject {
    use objc2::{class, msg_send};

    msg_send![class!(NSAppearance), appearanceNamed: ns_string("NSAppearanceNameVibrantDark")]
}

unsafe fn set_layer_background(
    layer: *mut objc2::runtime::AnyObject,
    background: *mut objc2::runtime::AnyObject,
) {
    use objc2::msg_send;
    let cg: *mut CGColor = msg_send![background, CGColor];
    let _: () = msg_send![layer, setBackgroundColor: cg];
}

unsafe fn image_from_png(bytes: &[u8]) -> *mut objc2::runtime::AnyObject {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    let data: *mut AnyObject = msg_send![
        class!(NSData),
        dataWithBytes: bytes.as_ptr() as *const c_void
        length: bytes.len()
    ];
    if data.is_null() {
        return std::ptr::null_mut();
    }
    let allocated: *mut AnyObject = msg_send![class!(NSImage), alloc];
    msg_send![allocated, initWithData: data]
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CardRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn card_layout(width: f64, height: f64, count: usize) -> Vec<CardRect> {
    if count == 0 {
        return Vec::new();
    }
    let visible_count = count.min(MAX_VISIBLE_PIP_CARDS);
    let depth = visible_count.saturating_sub(1) as f64;
    let usable_width = (width - 2.0 * STACK_INSET - STACK_HOVER_LIFT_X).max(1.0);
    let usable_height = (height - 2.0 * STACK_INSET - STACK_HOVER_LIFT_Y).max(1.0);
    let offset_x = if depth == 0.0 {
        0.0
    } else {
        STACK_CARD_OFFSET_X.min((usable_width * 0.24) / depth)
    };
    let offset_y = if depth == 0.0 {
        0.0
    } else {
        STACK_CARD_OFFSET_Y.min(((usable_height - STACK_MIN_CARD_HEIGHT).max(0.0)) / depth)
    };
    let card_width = (usable_width - offset_x * depth).max(1.0);
    let card_height = (usable_height - offset_y * depth).max(1.0);

    // Older cards sit closely behind and peek out above/right of the newest
    // card. Clicking a visible sliver promotes that app into the front slot.
    // AppKit paints later subviews on top, so the final published app becomes
    // the front card while every earlier app keeps a visible title strip.
    (0..visible_count)
        .map(|index| {
            let depth_from_front = (visible_count - 1 - index) as f64;
            CardRect {
                x: STACK_INSET + depth_from_front * offset_x,
                y: STACK_INSET + depth_from_front * offset_y,
                width: card_width,
                height: card_height,
            }
        })
        .collect()
}

fn initial_pip_size(screen_width: f64, screen_height: f64, geometry: PipGeometry) -> (f64, f64) {
    let default_geometry = PipGeometry::default();
    let use_responsive_default = geometry.width == default_geometry.width
        && geometry.height == default_geometry.height
        && geometry.x.is_none()
        && geometry.y.is_none();
    if use_responsive_default {
        return (
            (screen_width * DEFAULT_SCREEN_FRACTION)
                .round()
                .clamp(MINIMUM_PIP_WIDTH, MAXIMUM_DEFAULT_PIP_WIDTH),
            (screen_height * DEFAULT_SCREEN_FRACTION)
                .round()
                .clamp(MINIMUM_PIP_HEIGHT, MAXIMUM_DEFAULT_PIP_HEIGHT),
        );
    }
    (
        (geometry.width as f64).max(MINIMUM_PIP_WIDTH),
        (geometry.height as f64).max(MINIMUM_PIP_HEIGHT),
    )
}

fn fitted_card_rect(slot: CardRect, image_width: f64, image_height: f64) -> CardRect {
    let (width, height) =
        pip_preview::fit_preview_size((slot.width, slot.height), (image_width, image_height));
    CardRect {
        x: slot.x + (slot.width - width) / 2.0,
        y: slot.y + (slot.height - height) / 2.0,
        width,
        height,
    }
}

fn visible_card_layout() -> Vec<CardRect> {
    // Cursor/geometry refresh needs ordering, not copies of all captured PNGs.
    let pids = VIEW_MODEL
        .lock()
        .unwrap()
        .as_ref()
        .map(|model| {
            model
                .ordered_frames()
                .iter()
                .map(|frame| frame.target.app_key_pid())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let handles = CARD_HANDLES.lock().unwrap();
    pids.iter()
        .filter_map(|pid| handles.get(pid).map(|card| card.resting_rect))
        .collect()
}

unsafe fn resize_card_for_image(
    pid: i64,
    handles: NativeCardHandles,
    size: objc2_foundation::NSSize,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let rect = fitted_card_rect(handles.layout_rect, size.width, size.height);
    if rect == handles.resting_rect {
        return;
    }
    if let Some(current) = CARD_HANDLES.lock().unwrap().get_mut(&pid) {
        current.resting_rect = rect;
    }
    let front_pid = current_snapshot()
        .last()
        .map(|frame| frame.target.app_key_pid());
    let hovered = *HOVERED_APP.lock().unwrap() == Some(pid) && front_pid != Some(pid);
    let card = handles.card as *mut AnyObject;
    let _: () = msg_send![card, setFrame: NSRect::new(
        NSPoint::new(rect.x + if hovered { STACK_HOVER_LIFT_X } else { 0.0 },
                     rect.y + if hovered { STACK_HOVER_LIFT_Y } else { 0.0 }),
        NSSize::new(rect.width, rect.height),
    )];
    // Autoresizing keeps image, clipping, drag surface and chrome attached to
    // the same card. Resize hit areas must follow its new visible edges too.
    let native = HANDLES
        .lock()
        .unwrap()
        .as_ref()
        .map(|native| (native.window, native.canvas));
    if let Some((window, canvas)) = native {
        let canvas = canvas as *mut AnyObject;
        let bounds: NSRect = msg_send![canvas, bounds];
        let hit_rects = resize_cursor_rects(bounds, &visible_card_layout());
        let resize_views = RESIZE_VIEW_DIRECTIONS.lock().unwrap().clone();
        for (view, direction) in resize_views {
            if let Some((rect, _)) = hit_rects.iter().find(|(_, side)| *side == direction) {
                let _: () = msg_send![view as *mut AnyObject, setFrame: *rect];
            }
        }
        let window = window as *mut AnyObject;
        let _: () = msg_send![window, invalidateCursorRectsForView: canvas];
    }
    if let Some((window, point)) = current_pip_cursor_location() {
        let content: *mut AnyObject = msg_send![window, contentView];
        set_hovered_app(hovered_pid_at_content_point(content, point));
        refresh_cursor_at_content_point(window, point);
    } else {
        set_hovered_app(None);
    }
}

unsafe fn install_tracking_area(card: *mut objc2::runtime::AnyObject) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    // Mouse enter/exit, move, cursor-update, always-active, visible-rect, and
    // during-drag tracking. This matches Codex's native PIPStackContentView.
    let options: u64 = 0x1 | 0x2 | 0x4 | 0x80 | 0x200 | 0x400;
    let allocated: *mut AnyObject = msg_send![objc2::class!(NSTrackingArea), alloc];
    let area: *mut AnyObject = msg_send![
        allocated,
        initWithRect: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
        options: options
        owner: card
        userInfo: std::ptr::null_mut::<AnyObject>()
    ];
    let _: () = msg_send![card, addTrackingArea: area];
    let _: () = msg_send![area, release];
}

unsafe fn render_card(
    canvas: *mut objc2::runtime::AnyObject,
    delegate: *mut objc2::runtime::AnyObject,
    frame: &PipFrame,
    rect: CardRect,
) -> NativeCardHandles {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let layout_rect = rect;
    let image = image_from_png(&frame.png_bytes);
    let rect = if image.is_null() {
        layout_rect
    } else {
        let size: NSSize = msg_send![image, size];
        fitted_card_rect(layout_rect, size.width, size.height)
    };
    let card_frame = NSRect::new(
        NSPoint::new(rect.x, rect.y),
        NSSize::new(rect.width, rect.height),
    );
    let allocated: *mut AnyObject = msg_send![pip_card_view_class(), alloc];
    let card: *mut AnyObject = msg_send![allocated, initWithFrame: card_frame];
    CARD_VIEW_PIDS
        .lock()
        .unwrap()
        .insert(card as usize, frame.target.app_key_pid());
    let _: () = msg_send![card, setWantsLayer: true];
    let card_layer: *mut AnyObject = msg_send![card, layer];
    let shadow = color(0.0, 0.0, 0.0, 0.72);
    let shadow_cg: *mut CGColor = msg_send![shadow, CGColor];
    let _: () = msg_send![card_layer, setShadowColor: shadow_cg];
    let _: () = msg_send![card_layer, setShadowOpacity: 0.34_f32];
    let _: () = msg_send![card_layer, setShadowRadius: 10.0_f64];
    let _: () = msg_send![card_layer, setShadowOffset: NSSize::new(0.0, -3.0)];

    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), card_frame.size);
    let clip: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSView), alloc];
        msg_send![allocated, initWithFrame: bounds]
    };
    let _: () = msg_send![clip, setAutoresizingMask: 18u64];
    let _: () = msg_send![clip, setWantsLayer: true];
    let clip_layer: *mut AnyObject = msg_send![clip, layer];
    let _: () = msg_send![clip_layer, setCornerRadius: CARD_RADIUS];
    let _: () = msg_send![clip_layer, setMasksToBounds: true];
    set_layer_background(clip_layer, color(0.0, 0.0, 0.0, 0.0));
    let _: () = msg_send![card, addSubview: clip];

    let glass: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSVisualEffectView), alloc];
        msg_send![allocated, initWithFrame: bounds]
    };
    let _: () = msg_send![glass, setAutoresizingMask: 18u64];
    let _: () = msg_send![glass, setMaterial: 13i64];
    let _: () = msg_send![glass, setBlendingMode: 1i64];
    let _: () = msg_send![glass, setState: 1i64];
    let _: () = msg_send![glass, setAppearance: vibrant_dark_appearance()];
    let _: () = msg_send![clip, addSubview: glass];

    let image_view: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSImageView), alloc];
        msg_send![allocated, initWithFrame: bounds]
    };
    let _: () = msg_send![image_view, setAutoresizingMask: 18u64];
    let _: () = msg_send![image_view, setImageScaling: 3u64];
    if !image.is_null() {
        let _: () = msg_send![image_view, setImage: image];
        let _: () = msg_send![image, release];
    }
    let _: () = msg_send![clip, addSubview: image_view];

    // A transparent surface above the preview makes the whole card draggable.
    // The hover chrome is added after it and therefore remains clickable.
    let drag_allocated: *mut AnyObject = msg_send![pip_card_view_class(), alloc];
    let drag_surface: *mut AnyObject = msg_send![drag_allocated, initWithFrame: bounds];
    CARD_VIEW_PIDS
        .lock()
        .unwrap()
        .insert(drag_surface as usize, frame.target.app_key_pid());
    let _: () = msg_send![drag_surface, setAutoresizingMask: 18u64];
    let _: () = msg_send![clip, addSubview: drag_surface];
    install_tracking_area(drag_surface);

    let controls_frame = NSRect::new(
        NSPoint::new(
            rect.width - CONTROL_SIZE - 7.0,
            rect.height - CONTROL_SIZE - 7.0,
        ),
        NSSize::new(CONTROL_SIZE, CONTROL_SIZE),
    );
    let controls: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSVisualEffectView), alloc];
        msg_send![allocated, initWithFrame: controls_frame]
    };
    let _: () = msg_send![controls, setMaterial: 13i64];
    let _: () = msg_send![controls, setAutoresizingMask: 9u64];
    let _: () = msg_send![controls, setBlendingMode: 0i64];
    let _: () = msg_send![controls, setState: 1i64];
    let _: () = msg_send![controls, setAppearance: vibrant_dark_appearance()];
    let _: () = msg_send![controls, setWantsLayer: true];
    let controls_layer: *mut AnyObject = msg_send![controls, layer];
    let _: () = msg_send![controls_layer, setCornerRadius: CONTROL_SIZE / 2.0];
    let _: () = msg_send![controls_layer, setMasksToBounds: true];

    let button: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSButton), alloc];
        msg_send![allocated, initWithFrame: NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(CONTROL_SIZE, CONTROL_SIZE),
        )]
    };
    let _: () = msg_send![button, setBordered: false];
    let symbol: *mut AnyObject = msg_send![
        objc2::class!(NSImage),
        imageWithSystemSymbolName: ns_string("minus")
        accessibilityDescription: ns_string("Minimize PiP")
    ];
    if !symbol.is_null() {
        let _: () = msg_send![button, setImage: symbol];
    }
    let _: () = msg_send![button, setContentTintColor: color(1.0, 1.0, 1.0, 0.96)];
    let _: () = msg_send![button, setToolTip: ns_string("Minimize PiP")];
    let _: () = msg_send![button, setTarget: delegate];
    let _: () = msg_send![button, setAction: objc2::sel!(minimizePip:)];
    let _: () = msg_send![controls, addSubview: button];
    let _: () = msg_send![controls, setHidden: true];
    let _: () = msg_send![clip, addSubview: controls];

    let _: () = msg_send![canvas, addSubview: card];
    NativeCardHandles {
        card: card as usize,
        image_view: image_view as usize,
        controls: controls as usize,
        resting_rect: rect,
        layout_rect,
        has_live_frame: false,
    }
}

fn can_reuse_card_views(
    previous: &[(pip_preview::PipTarget, CardRect)],
    next: &[(pip_preview::PipTarget, CardRect)],
) -> bool {
    previous.len() == next.len()
        && previous
            .iter()
            .zip(next)
            .all(|((old_target, old_rect), (target, rect))| {
                same_observation_target(old_target, target) && old_rect == rect
            })
}

fn should_update_seed_image(has_live_frame: bool, live_stream_is_current: bool) -> bool {
    // An observation PNG can be older than the stream frame already on screen.
    // Reapplying it on each get-state would visibly roll the image backwards.
    !has_live_frame || !live_stream_is_current
}

unsafe fn update_card_seed_in_place(frame: &PipFrame, handles: NativeCardHandles) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSSize;

    let live_stream_is_current = LIVE_STREAMS
        .lock()
        .unwrap()
        .get(&frame.target.app_key_pid())
        .is_some_and(|entry| {
            entry.target_pid == frame.target.pid
                && entry.window_id == frame.target.window_id
                && !entry.cancelled.load(Ordering::Acquire)
                && entry.stream.is_some()
        });
    if !should_update_seed_image(handles.has_live_frame, live_stream_is_current) {
        return;
    }
    let image = image_from_png(&frame.png_bytes);
    if !image.is_null() {
        let size: NSSize = msg_send![image, size];
        resize_card_for_image(frame.target.app_key_pid(), handles, size);
        let _: () = msg_send![handles.image_view as *mut AnyObject, setImage: image];
        let _: () = msg_send![image, release];
    }
}

unsafe fn render_snapshot(snapshot: &[PipFrame]) {
    let visible_windows =
        crate::windows::visible_windows_including_accessory_layers_with_snapshot();
    let foreground_pid = crate::apps::frontmost_pid();
    let observed = foreground_candidates_from_visible_windows(
        snapshot,
        &visible_windows,
        foreground_pid,
        i32::try_from(std::process::id()).ok(),
        crate::apps::is_auxiliary_application,
    );
    let suppressed = {
        let mut state = FOREGROUND_VISIBILITY_STATE.lock().unwrap();
        let mut suppressed = suppression_after_window_enumeration(
            &state.suppressed_pids,
            observed,
            visible_windows.succeeded,
        );
        preserve_temporarily_activated_cards(
            &mut state,
            snapshot,
            foreground_pid,
            &mut suppressed,
            monotonic_ms(),
        );
        update_foreground_visibility_state(&mut state, suppressed.clone());
        suppressed
    };
    render_snapshot_with_suppressed_windows(snapshot, &suppressed);
}

unsafe fn render_snapshot_with_suppressed_windows(
    snapshot: &[PipFrame],
    suppressed_pids: &HashSet<i64>,
) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let visible_snapshot = snapshot
        .iter()
        .filter(|frame| {
            frame_has_preview_authority(frame) && !frame_is_suppressed(frame, suppressed_pids)
        })
        .collect::<Vec<_>>();

    let (window, canvas, delegate) = {
        let guard = HANDLES.lock().unwrap();
        let Some(handles) = guard.as_ref() else {
            return;
        };
        (
            handles.window as *mut AnyObject,
            handles.canvas as *mut AnyObject,
            handles.delegate as *mut AnyObject,
        )
    };

    let bounds: objc2_foundation::NSRect = msg_send![canvas, bounds];
    let layout = card_layout(
        bounds.size.width,
        bounds.size.height,
        visible_snapshot.len(),
    );
    let next_cards = visible_snapshot
        .iter()
        .zip(&layout)
        .map(|(frame, rect)| (frame.target.clone(), *rect))
        .collect::<Vec<_>>();
    let reuse = can_reuse_card_views(&RENDERED_CARDS.lock().unwrap(), &next_cards) && {
        let handles = CARD_HANDLES.lock().unwrap();
        handles.len() == next_cards.len()
            && next_cards
                .iter()
                .all(|(target, _)| handles.contains_key(&target.app_key_pid()))
    };
    if reuse {
        CARD_VIEW_REUSES.fetch_add(1, Ordering::Relaxed);
        for frame in &visible_snapshot {
            let handles = CARD_HANDLES
                .lock()
                .unwrap()
                .get(&frame.target.app_key_pid())
                .copied();
            if let Some(handles) = handles {
                update_card_seed_in_place(frame, handles);
            }
        }
    } else {
        CARD_VIEW_REBUILDS.fetch_add(1, Ordering::Relaxed);
        CARD_HANDLES.lock().unwrap().clear();
        CARD_VIEW_PIDS.lock().unwrap().clear();
        RESIZE_VIEW_DIRECTIONS.lock().unwrap().clear();
        let empty: *mut AnyObject = msg_send![objc2::class!(NSArray), array];
        let _: () = msg_send![canvas, setSubviews: empty];
        for (frame, rect) in visible_snapshot.iter().zip(layout.iter().copied()) {
            let handles = render_card(canvas, delegate, frame, rect);
            CARD_HANDLES
                .lock()
                .unwrap()
                .insert(frame.target.app_key_pid(), handles);
        }
        install_resize_hit_views(canvas, bounds, &visible_card_layout());
    }
    *RENDERED_CARDS.lock().unwrap() = next_cards;
    let _: () = msg_send![window, invalidateCursorRectsForView: canvas];
    if let Some((window, location)) = current_pip_cursor_location() {
        refresh_cursor_at_content_point(window, location);
    }

    let hovered_pid = HOVERED_APP.lock().unwrap().take();
    set_hovered_app(hovered_pid);

    let is_visible: objc2::runtime::Bool = msg_send![window, isVisible];
    if visible_snapshot.is_empty() {
        hide_custom_cursor();
        if is_visible.as_bool() {
            let _: () = msg_send![window, orderOut: std::ptr::null_mut::<AnyObject>()];
        }
    } else if !is_visible.as_bool() {
        let _: () = msg_send![window, orderFrontRegardless];
    }
    reconcile_live_capture(snapshot, suppressed_pids);
}

pub struct MacosPipBackendFactory;

impl PipBackendFactory for MacosPipBackendFactory {
    fn start(&self, cfg: &PipConfig) -> anyhow::Result<Box<dyn PipBackend>> {
        dispatch_to_main(cfg.clone(), init_cb);
        Ok(Box::new(MacosPipBackend))
    }
}

unsafe extern "C" fn init_cb(ctx: *mut c_void) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let cfg: PipConfig = *Box::from_raw(ctx as *mut PipConfig);
    if HANDLES.lock().unwrap().is_some() {
        return;
    }

    let screen: *mut AnyObject = msg_send![objc2::class!(NSScreen), mainScreen];
    if screen.is_null() {
        return;
    }
    let screen_frame: NSRect = msg_send![screen, frame];
    let minimum = NSSize::new(MINIMUM_PIP_WIDTH, MINIMUM_PIP_HEIGHT);
    let (width, height) = initial_pip_size(
        screen_frame.size.width,
        screen_frame.size.height,
        cfg.geometry,
    );
    let inset = 24.0;
    let (top_left_x, top_left_y) = match (cfg.geometry.x, cfg.geometry.y) {
        (Some(x), Some(y)) => (x as f64, y as f64),
        _ => (screen_frame.size.width - width - inset, inset),
    };
    let bottom_y = screen_frame.size.height - top_left_y - height;
    let rect = NSRect::new(
        NSPoint::new(top_left_x, bottom_y),
        NSSize::new(width, height),
    );

    // Borderless, non-activating panel. Movement and resizing are handled by
    // custom hit views, so no standard traffic-light controls cover previews.
    let style_mask: u64 = 1 << 7;
    let window: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSPanel), alloc];
        msg_send![
            allocated,
            initWithContentRect: rect
            styleMask: style_mask
            backing: 2u64
            defer: false
        ]
    };
    if window.is_null() {
        return;
    }

    let clear: *mut AnyObject = msg_send![objc2::class!(NSColor), clearColor];
    let _: () = msg_send![window, setBackgroundColor: clear];
    let _: () = msg_send![window, setOpaque: false];
    let _: () = msg_send![window, setHasShadow: false];
    let _: () = msg_send![window, setIgnoresMouseEvents: false];
    let _: () = msg_send![window, setAcceptsMouseMovedEvents: true];
    // NSPanel is deliberately non-activating. Opt into cursor-rect handling
    // while another application remains active, otherwise AppKit delivers the
    // hover callbacks but leaves the visible system cursor unchanged.
    let _: () = msg_send![window, setAllowsCursorRectsWhenInactive: true];
    let _: () = msg_send![window, _setAllowEdgeResizingCursorsInInactiveApp: true];
    let _: () = msg_send![window, _setWantsMouseMoveEventsInBackground: true];
    let _: () = msg_send![window, setBecomesKeyOnlyIfNeeded: true];
    // Cards explicitly call performWindowDragWithEvent. Keeping the window's
    // implicit background dragging off lets the edge/corner resize hit views
    // receive mouseDown first instead of moving the whole panel.
    let _: () = msg_send![window, setMovableByWindowBackground: false];
    let _: () = msg_send![window, setFloatingPanel: false];
    // PiP must remain visible above whichever *background* app the user is
    // currently controlling. Foreground-owner suppression keeps the active or
    // visually-frontmost app's own card out of the stack, so floating the
    // container does not duplicate that app on top of itself.
    let _: () = msg_send![window, setLevel: PIP_WINDOW_LEVEL];
    let _: () = msg_send![window, setCollectionBehavior: 0x108u64];
    let _: () = msg_send![window, setReleasedWhenClosed: false];
    let _: () = msg_send![window, setHidesOnDeactivate: false];
    let _: () = msg_send![window, setMinSize: minimum];

    let content_view: *mut AnyObject = msg_send![window, contentView];
    let _: () = msg_send![content_view, setWantsLayer: true];
    let content_layer: *mut AnyObject = msg_send![content_view, layer];
    set_layer_background(content_layer, color(0.0, 0.0, 0.0, 0.0));

    let bounds: NSRect = msg_send![content_view, bounds];
    let canvas: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![pip_canvas_view_class(), alloc];
        msg_send![allocated, initWithFrame: bounds]
    };
    let _: () = msg_send![canvas, setAutoresizingMask: 18u64];
    let _: () = msg_send![content_view, addSubview: canvas];
    install_tracking_area(canvas);

    let delegate = pip_delegate_instance();
    let _: () = msg_send![window, setDelegate: delegate];
    let workspace: *mut AnyObject = msg_send![objc2::class!(NSWorkspace), sharedWorkspace];
    let notification_center: *mut AnyObject = msg_send![workspace, notificationCenter];
    let _: () = msg_send![
        notification_center,
        addObserver: delegate
        selector: objc2::sel!(workspaceDidActivate:)
        name: objc2_app_kit::NSWorkspaceDidActivateApplicationNotification
        object: std::ptr::null_mut::<AnyObject>()
    ];
    let _: () = msg_send![
        notification_center,
        addObserver: delegate
        selector: objc2::sel!(workspaceDidActivate:)
        name: ns_string("NSWorkspaceDidTerminateApplicationNotification")
        object: std::ptr::null_mut::<AnyObject>()
    ];
    let (global_mouse_monitor, local_mouse_monitor) = install_mouse_monitors();
    *HANDLES.lock().unwrap() = Some(NativeHandles {
        window: window as usize,
        canvas: canvas as usize,
        delegate: delegate as usize,
        global_mouse_monitor,
        local_mouse_monitor,
    });
    VIEW_MODEL
        .lock()
        .unwrap()
        .get_or_insert_with(|| PipViewModel::new(MAX_VISIBLE_PIP_CARDS));

    tracing::info!(
        target: "pip",
        width,
        height,
        max_cards = MAX_VISIBLE_PIP_CARDS,
        "native per-app PiP stack initialised"
    );
}

unsafe extern "C" fn shutdown_cb(_ctx: *mut c_void) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    hide_custom_cursor();
    stop_foreground_visibility_watcher();
    CARD_HANDLES.lock().unwrap().clear();
    CARD_VIEW_PIDS.lock().unwrap().clear();
    RESIZE_VIEW_DIRECTIONS.lock().unwrap().clear();
    VIEW_MODEL.lock().unwrap().take();
    RENDERED_CARDS.lock().unwrap().clear();
    OBSERVATION_RETRIES.lock().unwrap().clear();
    HIDDEN_APPS.lock().unwrap().clear();
    HOVERED_APP.lock().unwrap().take();
    CARD_GESTURE.lock().unwrap().take();
    LAST_FOREGROUND_CHECK_MS.store(0, Ordering::Release);
    FOREGROUND_REFRESH_PENDING.store(false, Ordering::Release);
    *FOREGROUND_VISIBILITY_STATE.lock().unwrap() = ForegroundVisibilityState::default();
    if let Some(handles) = HANDLES.lock().unwrap().take() {
        let window = handles.window as *mut AnyObject;
        let workspace: *mut AnyObject = msg_send![objc2::class!(NSWorkspace), sharedWorkspace];
        let notification_center: *mut AnyObject = msg_send![workspace, notificationCenter];
        let delegate = handles.delegate as *mut AnyObject;
        let _: () = msg_send![notification_center, removeObserver: delegate];
        let global_mouse_monitor = handles.global_mouse_monitor as *mut AnyObject;
        if !global_mouse_monitor.is_null() {
            let _: () = msg_send![
                objc2::class!(NSEvent),
                removeMonitor: global_mouse_monitor
            ];
        }
        let local_mouse_monitor = handles.local_mouse_monitor as *mut AnyObject;
        if !local_mouse_monitor.is_null() {
            let _: () = msg_send![
                objc2::class!(NSEvent),
                removeMonitor: local_mouse_monitor
            ];
        }
        let _: () = msg_send![window, setDelegate: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![window, orderOut: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![window, close];
        let _ = handles.delegate;
    }
}

/// Prepare the process-wide AppKit host when another observer (currently the
/// agent cursor) owns the actual `NSApplication.run()` call.
pub fn prepare_for_shared_appkit_main_loop() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let _mtm = objc2_foundation::MainThreadMarker::new()
        .expect("prepare_for_shared_appkit_main_loop must run on the main thread");
    unsafe {
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        let _: bool = msg_send![app, setActivationPolicy: 1i64];
        let cursor_property = ns_string("SetsCursorInBackground");
        let enabled: *mut AnyObject = msg_send![
            objc2::class!(NSNumber),
            numberWithBool: objc2::runtime::Bool::YES
        ];
        let background_cursor_updates = crate::input::skylight::enable_background_cursor_updates(
            cursor_property as *const c_void,
            enabled as *const c_void,
        );
        if !background_cursor_updates {
            tracing::warn!(
                target: "pip",
                "WindowServer rejected background cursor ownership; PiP hover cursors may be unavailable"
            );
        }
    }
}

/// Park the main thread in `NSApplication.run()` so AppKit can service the
/// asynchronously created stack and live-frame callbacks.
pub fn run_appkit_main_loop() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    prepare_for_shared_appkit_main_loop();
    unsafe {
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, finishLaunching];
        let _: () = msg_send![app, run];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(window_id: u64, pid: i64) -> PipFrame {
        PipFrame {
            target: pip_preview::PipTarget {
                logical_pid: None,
                delegation: None,
                pid,
                window_id,
                session_id: Some("session-a".to_owned()),
                app_name: format!("App {pid}"),
                window_title: None,
            },
            png_bytes: Vec::new(),
            timestamp_ms: 0,
        }
    }

    fn delegated_frame(window_id: u64, host_pid: i64, helper_pid: i64) -> PipFrame {
        let mut frame = frame(window_id, helper_pid);
        frame.target.logical_pid = Some(host_pid);
        frame.target.delegation = Some(pip_preview::PipDelegation {
            kind: "trusted_macos_open_save_panel".to_owned(),
            host_pid,
            panel_kind: "open".to_owned(),
            expected_bundle_id: Some("com.example.host".to_owned()),
            expected_app_name: Some(format!("Host {host_pid}")),
        });
        frame
    }

    #[test]
    fn temporary_activation_preserves_only_the_existing_exact_card() {
        let current = frame(7, 42);
        let mut state = ForegroundVisibilityState::default();
        let token = state
            .temporary_activations
            .begin(activation_target(&current.target), Some(90), true, 0)
            .unwrap();
        let lease = Some((state.activation_lifetime, token));
        let mut suppressed = HashSet::from([42, 99]);
        preserve_temporarily_activated_cards(
            &mut state,
            &[current.clone()],
            Some(42),
            &mut suppressed,
            1,
        );
        assert_eq!(suppressed, HashSet::from([99]));
        let mut changed_window_suppression = HashSet::from([42]);
        preserve_temporarily_activated_cards(
            &mut state,
            &[frame(8, 42)],
            Some(42),
            &mut changed_window_suppression,
            2,
        );
        assert_eq!(changed_window_suppression, HashSet::from([42]));
        assert!(finish_temporary_activation(&mut state, lease));
        let mut suppressed = HashSet::from([42]);
        preserve_temporarily_activated_cards(&mut state, &[current], Some(42), &mut suppressed, 3);
        assert_eq!(suppressed, HashSet::from([42]));
    }

    #[test]
    fn temporary_panel_activation_preserves_host_card_but_not_another_panel() {
        let current = delegated_frame(7, 42, 88);
        let mut state = ForegroundVisibilityState::default();
        state
            .temporary_activations
            .begin(activation_target(&current.target), Some(90), true, 0)
            .unwrap();
        for foreground in [42, 88, 90] {
            let mut suppressed = HashSet::from([42]);
            preserve_temporarily_activated_cards(
                &mut state,
                &[current.clone()],
                Some(foreground),
                &mut suppressed,
                1,
            );
            assert!(suppressed.is_empty());
        }
        let mut suppressed = HashSet::from([42]);
        preserve_temporarily_activated_cards(
            &mut state,
            &[delegated_frame(8, 42, 88)],
            Some(88),
            &mut suppressed,
            2,
        );
        assert_eq!(suppressed, HashSet::from([42]));
    }

    #[test]
    fn temporary_activation_does_not_reveal_the_previous_foreground_card() {
        let current = frame(7, 42);
        let original_front = frame(8, 90);
        let mut state = ForegroundVisibilityState::default();
        let token = state
            .temporary_activations
            .begin(activation_target(&current.target), Some(90), true, 0)
            .unwrap();
        let mut suppressed = HashSet::from([42]);
        preserve_temporarily_activated_cards(
            &mut state,
            &[current.clone(), original_front.clone()],
            Some(42),
            &mut suppressed,
            1,
        );
        assert_eq!(suppressed, HashSet::from([90]));
        state.temporary_activations.end(token);
        let mut suppressed = HashSet::from([90]);
        preserve_temporarily_activated_cards(
            &mut state,
            &[current, original_front],
            Some(90),
            &mut suppressed,
            2,
        );
        assert_eq!(suppressed, HashSet::from([90]));
    }

    #[test]
    fn temporary_activation_requires_both_input_monitors() {
        assert!(temporary_activation_monitors_ready(1, 2));
        assert!(!temporary_activation_monitors_ready(0, 2));
        assert!(!temporary_activation_monitors_ready(1, 0));
        assert!(!temporary_activation_monitors_ready(0, 0));
    }

    #[test]
    fn user_takeover_revokes_temporary_visibility_without_later_resurrection() {
        let current = frame(7, 42);
        let mut state = ForegroundVisibilityState::default();
        state
            .temporary_activations
            .begin(activation_target(&current.target), Some(90), true, 0)
            .unwrap();
        let mut suppressed = HashSet::from([42]);
        preserve_temporarily_activated_cards(
            &mut state,
            &[current.clone()],
            Some(99),
            &mut suppressed,
            1,
        );
        assert_eq!(suppressed, HashSet::from([42]));
        preserve_temporarily_activated_cards(&mut state, &[current], Some(42), &mut suppressed, 2);
        assert_eq!(suppressed, HashSet::from([42]));
    }

    #[test]
    fn external_clicks_and_keys_cancel_holds_but_our_events_and_mouse_motion_do_not() {
        for event in [1, 3, 25, 10] {
            assert!(input_cancels_temporary_activation(event, Some(0), 123));
            assert!(input_cancels_temporary_activation(event, Some(90), 123));
            assert!(input_cancels_temporary_activation(event, None, 123));
            assert!(!input_cancels_temporary_activation(event, Some(123), 123));
        }
        assert!(!input_cancels_temporary_activation(5, Some(0), 123));
    }

    #[test]
    fn ending_a_hold_invalidates_inflight_foreground_samples() {
        let mut state = ForegroundVisibilityState::default();
        let token = state
            .temporary_activations
            .begin(activation_target(&frame(7, 42).target), Some(90), true, 0)
            .unwrap();
        let lease = Some((state.activation_lifetime, token));
        let refresh = ForegroundVisibilityRefresh {
            observed_epoch: state.epoch,
            suppressed_pids: HashSet::new(),
            candidates_changed: false,
        };
        assert!(finish_temporary_activation(&mut state, lease));
        let (_, _, resample) = apply_foreground_visibility_refresh(&mut state, &refresh);
        assert!(resample);
        assert!(!finish_temporary_activation(&mut state, lease));
    }

    #[test]
    fn old_guard_cannot_end_a_reinitialized_backends_new_hold() {
        let target = activation_target(&frame(7, 42).target);
        let mut old = ForegroundVisibilityState::default();
        let token = old
            .temporary_activations
            .begin(target, Some(90), true, 0)
            .unwrap();
        let old_lease = Some((old.activation_lifetime, token));
        let mut new = ForegroundVisibilityState::default();
        new.temporary_activations
            .begin(target, Some(90), true, 0)
            .unwrap();
        assert!(!finish_temporary_activation(&mut new, old_lease));
        assert!(new.temporary_activations.keeps_visible(target, 1));
    }

    #[test]
    fn same_window_frame_updates_reuse_views_but_identity_or_layout_changes_do_not() {
        let rect = card_layout(480.0, 300.0, 1)[0];
        let target = frame(7, 42).target;
        let before = vec![(target.clone(), rect)];
        let mut renamed = target.clone();
        renamed.window_title = Some("Saved document".into());
        assert!(can_reuse_card_views(&before, &[(renamed, rect)]));
        for changed in [
            frame(8, 42).target,
            frame(7, 43).target,
            delegated_frame(7, 90, 42).target,
        ] {
            assert!(!can_reuse_card_views(&before, &[(changed, rect)]));
        }
        let mut new_session = target.clone();
        new_session.session_id = Some("other-session".into());
        assert!(!can_reuse_card_views(&before, &[(new_session, rect)]));
        assert!(!can_reuse_card_views(&before, &[]));
        assert!(!can_reuse_card_views(
            &before,
            &[(target, card_layout(600.0, 300.0, 1)[0])]
        ));
    }

    #[test]
    fn observation_seed_does_not_roll_back_a_live_preview() {
        assert!(!should_update_seed_image(true, true));
        assert!(should_update_seed_image(false, true));
        assert!(should_update_seed_image(true, false));
        assert!(should_update_seed_image(false, false));
    }

    #[test]
    fn application_menu_preview_is_seeded_not_overwritten_by_live_document_capture() {
        let ordinary = frame(7, 42);
        let mut menu = frame(9, 42);
        menu.target.logical_pid = Some(42);
        assert!(frame_needs_live_capture(&ordinary, &HashSet::new()));
        assert!(!frame_needs_live_capture(&menu, &HashSet::new()));
        assert!(!frame_needs_live_capture(&ordinary, &HashSet::from([42])));
        assert!(!frame_needs_live_capture(&menu, &HashSet::from([42])));
        // Existing out-of-process helper panels retain their live stream.
        assert!(frame_needs_live_capture(
            &delegated_frame(99, 42, 900),
            &HashSet::new()
        ));
    }

    fn resolved_delegated_context(
        host_pid: i32,
        helper_pid: i32,
        window_id: u32,
        panel_kind: crate::ax::app_context::OpenSavePanelKind,
    ) -> crate::ax::app_context::ResolvedAppContext {
        let target = crate::ax::app_context::AppContextTarget {
            pid: helper_pid,
            window_id,
        };
        crate::ax::app_context::ResolvedAppContext {
            identity: crate::ax::app_context::RunningAppIdentity {
                bundle_id: Some("com.example.host".to_owned()),
                app_name: Some(format!("Host {host_pid}")),
            },
            snapshot: crate::ax::app_context::AppContextSnapshot {
                focused: crate::ax::app_context::AxWindowEvidence::Resolved(window_id),
                main: crate::ax::app_context::AxWindowEvidence::NotQueried,
                windows: crate::ax::app_context::AxWindowsEvidence::NotQueried,
            },
            selection: crate::ax::app_context::AppContextSelection {
                window_id,
                reason: crate::ax::app_context::AppContextSelectionReason::FocusedWindow,
            },
            target,
            delegation: Some(crate::ax::app_context::AppContextDelegation {
                host_pid,
                target,
                panel_kind,
            }),
        }
    }

    fn visible_window(
        window_id: u32,
        pid: i32,
        app_name: &str,
        z_index: usize,
    ) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid,
            app_name: app_name.to_owned(),
            title: format!("{app_name} window"),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            layer: 0,
            z_index,
            is_on_screen: true,
            current_space_id: Some(1),
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
        }
    }

    #[test]
    fn capture_dimensions_preserve_aspect_ratio_and_bound_size() {
        assert_eq!(capture_dimensions(640.0, 400.0), (640, 400));
        assert_eq!(capture_dimensions(4000.0, 2000.0), (960, 480));
    }

    #[test]
    fn failed_live_capture_retries_without_restarting_a_healthy_or_pending_stream() {
        let now = Instant::now();
        let mut entry = LiveStreamEntry {
            target_pid: 42,
            window_id: 7,
            stream: None,
            resize: LiveCaptureResizeState::default(),
            cancelled: Arc::new(AtomicBool::new(false)),
            frame_pending: Arc::new(AtomicBool::new(false)),
            retry_at: None,
        };
        assert!(
            live_capture_is_reusable(&entry, 42, 7, now),
            "coalesce in-flight start"
        );
        entry.retry_at = Some(now + Duration::from_secs(1));
        assert!(
            live_capture_is_reusable(&entry, 42, 7, now),
            "respect failure backoff"
        );
        assert!(!live_capture_is_reusable(
            &entry,
            42,
            7,
            now + Duration::from_secs(1)
        ));
        entry.retry_at = None;
        assert!(live_capture_is_reusable(
            &entry,
            42,
            7,
            now + Duration::from_secs(2)
        ));
        assert!(
            !live_capture_is_reusable(&entry, 42, 8, now),
            "a different window is not reused"
        );
        entry.cancelled.store(true, Ordering::Release);
        assert!(!live_capture_is_reusable(&entry, 42, 7, now));
    }

    #[test]
    fn preview_failure_backoff_is_bounded_and_does_not_busy_poll() {
        assert_eq!(preview_retry_delay_ms(1), 1_000);
        assert_eq!(preview_retry_delay_ms(2), 2_000);
        assert_eq!(preview_retry_delay_ms(3), 4_000);
        assert_eq!(preview_retry_delay_ms(4), 8_000);
        assert_eq!(preview_retry_delay_ms(u32::MAX), 8_000);
    }

    #[test]
    fn cross_app_handoff_retains_seeds_but_new_observation_rejects_old_generation() {
        let mut model = PipViewModel::new(5);
        let mut finder = frame(1_049_011, 1_049_010).target;
        finder.session_id = Some("pip-switch-regression".into());
        let mut editor = frame(1_049_021, 1_049_020).target;
        editor.session_id = finder.session_id.clone();
        model.select_target(&finder);
        let old_generation = reserve_pip_publication(&finder);
        model.select_target(&editor);
        assert!(
            model.accepts_target(&finder),
            "the task still retains its first app"
        );
        assert!(pip_publication_is_current(&finder, old_generation));
        model.select_target(&finder);
        let current_generation = reserve_pip_publication(&finder);
        assert!(model.accepts_target(&finder));
        assert!(!pip_publication_is_current(&finder, old_generation));
        assert!(pip_publication_is_current(&finder, current_generation));
        cancel_pip_publication(&finder, current_generation);
    }

    #[test]
    fn stale_card_cleanup_preserves_pending_full_image_publication() {
        let target = frame(1_049_031, 1_049_030).target;
        let generation = reserve_pip_publication(&target);
        remove_expired_card_proof(target.app_key_pid());
        assert!(pip_publication_is_current(&target, generation));
        finish_pip_observation(&target, generation);
        remove_expired_card_proof(target.app_key_pid());
        assert!(!pip_publication_is_current(&target, generation));
    }

    #[test]
    fn card_click_uses_the_rendered_window_not_a_newer_unrendered_model() {
        let old = frame(701, 70);
        let newer = frame(702, 70);
        let rect = CardRect {
            x: 8.0,
            y: 8.0,
            width: 300.0,
            height: 200.0,
        };
        let mut model = PipViewModel::new(5);
        model.upsert(newer);
        let cards = vec![(old.target.clone(), rect)];
        let (clicked, front) = rendered_click_target(&cards, Some(70));
        let clicked = clicked.unwrap();
        assert_eq!(clicked.target.window_id, 701);
        assert_eq!(front, Some(70));
        assert_ne!(
            clicked.target.window_id,
            model.frame_for_app(70).unwrap().target.window_id
        );
    }

    #[test]
    fn hidden_latest_card_does_not_change_visible_front_card_click() {
        let shown = frame(801, 80);
        let hidden = frame(901, 90);
        let mut model = PipViewModel::new(5);
        model.upsert(shown.clone());
        model.select_target(&hidden.target);
        model.upsert(hidden);
        assert_eq!(model.ordered_frames().last().unwrap().target.pid, 90);
        let cards = vec![(
            shown.target,
            CardRect {
                x: 8.0,
                y: 8.0,
                width: 300.0,
                height: 200.0,
            },
        )];
        let (clicked, front) = rendered_click_target(&cards, Some(80));
        assert_eq!(clicked.unwrap().target.app_key_pid(), front.unwrap());
        assert!(rendered_click_target(&cards, Some(90)).0.is_none());
    }

    #[test]
    fn changed_window_session_or_layout_cancels_card_click() {
        let mut target = frame(701, 70).target;
        target.session_id = Some("pip-click-a".into());
        let rect = CardRect {
            x: 8.0,
            y: 8.0,
            width: 300.0,
            height: 200.0,
        };
        let clicked = ClickedTarget {
            target: target.clone(),
            layout_rect: rect,
        };
        assert!(clicked_presentation_is_current(
            &clicked,
            &[(target.clone(), rect)]
        ));
        assert!(!clicked_presentation_is_current(&clicked, &[]));
        let mut sibling = target.clone();
        sibling.window_id += 1;
        assert!(!clicked_presentation_is_current(
            &clicked,
            &[(sibling, rect)]
        ));
        let mut another_session = target.clone();
        another_session.session_id = Some("pip-click-b".into());
        assert!(!clicked_presentation_is_current(
            &clicked,
            &[(another_session, rect)]
        ));
        let moved = CardRect { y: 26.0, ..rect };
        assert!(!clicked_presentation_is_current(
            &clicked,
            &[(target, moved)]
        ));
    }

    #[test]
    fn live_capture_resize_coalesces_pending_updates_and_tracks_success() {
        let mut resize = LiveCaptureResizeState {
            configured: Some((230, 408)),
            pending: None,
        };
        assert!(!resize.begin((230, 408)));
        assert!(resize.begin((640, 408)));
        assert!(!resize.begin((640, 408)));
        assert!(!resize.begin((800, 408)), "only one worker per stream");
        resize.finish((640, 408), true);
        assert_eq!(resize.configured, Some((640, 408)));
        assert!(!resize.begin((640, 408)));
        assert!(
            resize.begin((800, 408)),
            "next sample can apply the latest size"
        );
    }

    #[test]
    fn live_capture_resize_failure_preserves_config_and_allows_retry() {
        let mut resize = LiveCaptureResizeState {
            configured: Some((230, 408)),
            pending: None,
        };
        assert!(resize.begin((640, 408)));
        resize.finish((640, 408), false);
        assert_eq!(resize.configured, Some((230, 408)));
        assert_eq!(resize.pending, None);
        assert!(resize.begin((640, 408)));
    }

    #[test]
    fn live_capture_resize_stale_completion_cannot_clear_new_pending_size() {
        let mut resize = LiveCaptureResizeState {
            configured: Some((230, 408)),
            pending: Some((800, 408)),
        };
        resize.finish((640, 408), true);
        assert_eq!(resize.configured, Some((230, 408)));
        assert_eq!(resize.pending, Some((800, 408)));
    }

    #[test]
    fn live_capture_resize_generation_rejects_cancelled_replaced_or_other_target() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let entry = LiveStreamEntry {
            target_pid: 42,
            window_id: 7,
            stream: None,
            resize: LiveCaptureResizeState::default(),
            cancelled: Arc::clone(&cancelled),
            frame_pending: Arc::new(AtomicBool::new(false)),
            retry_at: None,
        };
        assert!(live_capture_generation_matches(&entry, 42, 7, &cancelled));
        assert!(!live_capture_generation_matches(&entry, 43, 7, &cancelled));
        assert!(!live_capture_generation_matches(&entry, 42, 8, &cancelled));
        assert!(!live_capture_generation_matches(
            &entry,
            42,
            7,
            &Arc::new(AtomicBool::new(false))
        ));
        cancelled.store(true, Ordering::Release);
        assert!(!live_capture_generation_matches(&entry, 42, 7, &cancelled));
    }

    #[test]
    fn live_capture_resize_rejects_invalid_geometry_and_preserves_new_ratio() {
        for (width, height) in [
            (0.0, 400.0),
            (400.0, -1.0),
            (f64::NAN, 400.0),
            (400.0, f64::INFINITY),
        ] {
            assert_eq!(live_capture_output_dimensions(width, height), None);
        }
        assert_eq!(
            live_capture_output_dimensions(230.0, 408.0),
            Some((230, 408))
        );
        assert_eq!(
            live_capture_output_dimensions(1600.0, 800.0),
            Some((960, 480))
        );
    }

    #[test]
    fn screen_capture_window_requires_exact_id_and_owner() {
        assert!(screen_capture_window_matches_target(77, Some(900), 77, 900));
        assert!(!screen_capture_window_matches_target(
            77,
            Some(901),
            77,
            900
        ));
        assert!(!screen_capture_window_matches_target(
            78,
            Some(900),
            77,
            900
        ));
        assert!(!screen_capture_window_matches_target(77, None, 77, 900));
    }

    #[test]
    fn ended_session_cannot_publish_or_reactivate_a_pip_target() {
        let session_id = format!("pip-ended-session-{}", std::process::id());
        let mut target = frame(77, 42).target;
        target.session_id = Some(session_id.clone());
        assert!(pip_target_session_is_live(&target));
        cua_driver_core::session::end_session(&session_id);
        assert!(!pip_target_session_is_live(&target));
        assert!(!pip_delegation_is_live(&target));
    }

    #[test]
    fn delegated_live_frame_proof_is_synchronously_revoked_on_target_rebind() {
        let target = delegated_frame(98_771, 98_742, 98_900).target;
        let epoch = delegation_target_epoch(&target);
        let publication_generation = reserve_pip_publication(&target);
        DELEGATION_PROOF_STATE.lock().unwrap().frame_proofs.insert(
            target.app_key_pid(),
            DelegationFrameProof {
                target_pid: target.pid,
                window_id: target.window_id,
                epoch,
                publication_generation,
                session_id: target.session_id.clone(),
            },
        );
        assert!(delegation_frame_proof_is_live(&target));
        invalidate_app_context_target(crate::ax::app_context::AppContextTarget {
            pid: i32::try_from(target.pid).unwrap(),
            window_id: u32::try_from(target.window_id).unwrap(),
        });
        assert!(!delegation_frame_proof_is_live(&target));
        remove_delegation_frame_proof(target.app_key_pid());
    }

    #[test]
    fn invalidation_after_reservation_prevents_stale_proof_registration() {
        let target = delegated_frame(98_773, 98_744, 98_901).target;
        let publication_generation = reserve_pip_publication(&target);
        assert!(pip_publication_is_current(&target, publication_generation));

        invalidate_app_context_target(crate::ax::app_context::AppContextTarget {
            pid: i32::try_from(target.pid).unwrap(),
            window_id: u32::try_from(target.window_id).unwrap(),
        });

        assert!(!pip_publication_is_current(&target, publication_generation));
        assert!(!commit_delegation_frame_proof(
            &target,
            publication_generation
        ));
        assert!(!delegation_frame_proof_is_live(&target));
        cancel_pip_publication(&target, publication_generation);
    }

    #[test]
    fn old_publication_cannot_reappear_after_session_id_reuse() {
        let session_id = format!("pip-reused-session-{}", std::process::id());
        let mut target = frame(98_772, 98_743).target;
        target.session_id = Some(session_id.clone());
        let old_generation = reserve_pip_publication(&target);
        assert!(pip_publication_is_current(&target, old_generation));
        remove_session_delegation_frame_proofs(&session_id);
        assert!(!pip_publication_is_current(&target, old_generation));

        let new_generation = reserve_pip_publication(&target);
        assert_ne!(old_generation, new_generation);
        assert!(!pip_publication_is_current(&target, old_generation));
        assert!(pip_publication_is_current(&target, new_generation));
        remove_session_delegation_frame_proofs(&session_id);
    }

    #[test]
    fn tree_only_new_session_survives_queued_old_session_card_cleanup() {
        let mut old = frame(1_048_401, 1_048_400).target;
        old.session_id = Some("pip-old-card-session".into());
        let old_generation = reserve_pip_observation(&old).unwrap();
        remove_session_delegation_frame_proofs("pip-old-card-session");
        assert!(!pip_publication_is_current(&old, old_generation));

        let mut next = old.clone();
        next.session_id = Some("pip-new-card-session".into());
        let next_generation = reserve_pip_observation(&next).unwrap();
        // The main-thread callback for the old card can run after this new
        // observation has reserved its asynchronous bootstrap capture.
        remove_ended_session_card_proof(old.app_key_pid(), "pip-old-card-session");
        assert!(pip_publication_is_current(&next, next_generation));
        remove_ended_session_card_proof(next.app_key_pid(), "pip-new-card-session");
        assert!(!pip_publication_is_current(&next, next_generation));
    }

    #[test]
    fn tree_only_cleanup_checks_generation_while_reuse_decisions_are_locked() {
        let mut checked_under_model_lock = false;
        assert!(
            remove_current_observation_card(&frame(1_048_501, 1_048_500).target, || {
                checked_under_model_lock = VIEW_MODEL.try_lock().is_err();
                false
            })
            .is_none()
        );
        assert!(checked_under_model_lock);
    }

    #[test]
    fn missing_or_changed_target_waits_for_an_observation_seed() {
        let target = pip_preview::PipTarget {
            logical_pid: None,
            delegation: None,
            pid: 100,
            window_id: 10,
            session_id: None,
            app_name: String::new(),
            window_title: None,
        };
        assert_eq!(
            pip_target_seed_policy(None, &target),
            PipTargetSeedPolicy::AwaitObservationSeed
        );
        assert_eq!(
            pip_target_seed_policy(Some(&frame(11, 100)), &target),
            PipTargetSeedPolicy::AwaitObservationSeed
        );
        assert_eq!(
            pip_target_seed_policy(Some(&frame(10, 100)), &target),
            PipTargetSeedPolicy::ReuseObservationAndLiveStream
        );
    }

    #[test]
    fn tree_only_observation_bootstraps_cold_and_changed_targets() {
        let target = frame(10, 100).target;
        assert_eq!(
            pip_observation_seed_policy(None, &target),
            PipTargetSeedPolicy::CaptureObservationPreview
        );
        assert_eq!(
            pip_observation_seed_policy(Some(&frame(11, 100)), &target),
            PipTargetSeedPolicy::CaptureObservationPreview
        );
        assert_eq!(
            pip_observation_seed_policy(Some(&frame(10, 100)), &target),
            PipTargetSeedPolicy::ReuseObservationAndLiveStream
        );
    }

    #[test]
    fn tree_only_menu_and_new_session_require_their_own_preview() {
        let mut menu = frame(10, 100);
        menu.target.logical_pid = Some(100);
        assert_eq!(
            pip_observation_seed_policy(Some(&menu), &menu.target),
            PipTargetSeedPolicy::CaptureObservationPreview
        );
        let previous = frame(10, 100);
        let mut next = previous.target.clone();
        next.session_id = Some("new-session".into());
        assert_eq!(
            pip_observation_seed_policy(Some(&previous), &next),
            PipTargetSeedPolicy::CaptureObservationPreview
        );
        assert!(same_observation_target(&menu.target, &menu.target));
        assert!(!same_observation_target(&previous.target, &next));
    }

    #[test]
    fn tree_only_pending_seed_coalesces_and_keeps_its_publication_generation() {
        let target = frame(1_048_201, 1_048_200).target;
        let generation = reserve_pip_observation(&target).unwrap();
        assert!(reserve_pip_observation(&target).is_none());
        let mut menu = target.clone();
        menu.logical_pid = Some(target.pid);
        menu.window_id += 1;
        assert!(select_observation_preview_source(
            &target, &menu, None, generation
        ));
        assert!(pip_publication_is_current(&menu, generation));
        assert!(reserve_pip_observation(&target).is_none());
        remove_expired_card_proof(target.app_key_pid());
        assert!(pip_publication_is_current(&menu, generation));
        let mut newer = target.clone();
        newer.window_id += 2;
        let newer_generation = reserve_pip_observation(&newer).unwrap();
        assert!(!pip_publication_is_current(&menu, generation));
        cancel_pip_publication(&target, generation);
        assert!(pip_publication_is_current(&newer, newer_generation));
        finish_pip_observation(&newer, newer_generation);
        remove_expired_card_proof(newer.app_key_pid());
        assert!(!pip_publication_is_current(&newer, newer_generation));
    }

    #[test]
    fn tree_only_private_menu_seed_never_changes_model_coordinate_authority() {
        let observed = frame(1_048_301, 1_048_300).target;
        let generation = reserve_pip_observation(&observed).unwrap();
        let mut source = observed.clone();
        source.logical_pid = Some(observed.pid);
        source.window_id += 1;
        let bounds = crate::windows::WindowBounds {
            x: 1.0,
            y: 2.0,
            width: 100.0,
            height: 100.0,
        };
        let menu = crate::ax::application_menu::ApplicationMenuImage {
            pid: observed.pid as i32,
            document_window_id: observed.window_id as u32,
            menu_window_id: source.window_id as u32,
            document_bounds: bounds.clone(),
            menu_bounds: bounds,
        };
        assert!(select_observation_preview_source(
            &observed,
            &source,
            Some(menu),
            generation
        ));
        assert!(
            !crate::ax::application_menu::menu_image_coordinates_withheld(
                observed.window_id as u32
            )
        );
        assert!(
            !crate::ax::application_menu::menu_image_coordinates_withheld(source.window_id as u32)
        );
        finish_pip_observation(&source, generation);
        let refreshed_generation = reserve_pip_observation(&observed).unwrap();
        {
            let state = DELEGATION_PROOF_STATE.lock().unwrap();
            let pending = state
                .latest_publications
                .get(&observed.app_key_pid())
                .unwrap();
            assert!(private_menu_for_target(pending, &source).is_some());
            let mut wrong_session = source.clone();
            wrong_session.session_id = Some("other-session".into());
            assert!(private_menu_for_target(pending, &wrong_session).is_none());
            let mut wrong_menu = source.clone();
            wrong_menu.window_id += 1;
            assert!(private_menu_for_target(pending, &wrong_menu).is_none());
        }
        cancel_pip_publication(&observed, refreshed_generation);
    }

    #[test]
    fn tree_only_capture_rejects_changed_owner_or_geometry() {
        let before = visible_window(7, 42, "App", 0);
        assert!(preview_window_unchanged(&before, &before));
        let mut moved = before.clone();
        moved.bounds.x += 1.0;
        assert!(!preview_window_unchanged(&before, &moved));
        let mut foreign = before.clone();
        foreign.pid = 43;
        assert!(!preview_window_unchanged(&before, &foreign));
        let mut sibling = before.clone();
        sibling.window_id = 8;
        assert!(!preview_window_unchanged(&before, &sibling));
    }

    #[test]
    fn exact_target_match_requires_both_pid_and_window() {
        let frame = frame(10, 100);
        assert!(exact_target_matches(&frame, &frame.target));
        assert!(!exact_target_matches(
            &frame,
            &pip_preview::PipTarget {
                logical_pid: None,
                delegation: None,
                pid: 100,
                window_id: 11,
                session_id: None,
                app_name: String::new(),
                window_title: None,
            }
        ));
        assert!(!exact_target_matches(
            &frame,
            &pip_preview::PipTarget {
                logical_pid: None,
                delegation: None,
                pid: 101,
                window_id: 10,
                session_id: None,
                app_name: String::new(),
                window_title: None,
            }
        ));
    }

    #[test]
    fn delegated_pip_proof_is_bound_to_host_target_and_panel_kind() {
        let frame = delegated_frame(77, 42, 900);
        let open = resolved_delegated_context(
            42,
            900,
            77,
            crate::ax::app_context::OpenSavePanelKind::Open,
        );
        assert!(pip_delegation_matches_resolved(
            &frame.target,
            42,
            900,
            77,
            crate::ax::app_context::OpenSavePanelKind::Open,
            &open,
        ));
        let rebound = resolved_delegated_context(
            43,
            900,
            77,
            crate::ax::app_context::OpenSavePanelKind::Open,
        );
        assert!(!pip_delegation_matches_resolved(
            &frame.target,
            42,
            900,
            77,
            crate::ax::app_context::OpenSavePanelKind::Open,
            &rebound,
        ));
        assert!(!pip_delegation_matches_resolved(
            &frame.target,
            42,
            900,
            77,
            crate::ax::app_context::OpenSavePanelKind::Save,
            &open,
        ));
    }

    #[test]
    fn frontmost_app_is_a_candidate_regardless_of_which_window_is_focused() {
        let snapshot = vec![frame(10, 100), frame(20, 200)];
        assert_eq!(
            foreground_candidate_pids(&snapshot, Some(100), None),
            HashSet::from([100])
        );
        assert_eq!(
            foreground_candidate_pids(&snapshot, Some(200), None),
            HashSet::from([200])
        );
        assert!(foreground_candidate_pids(&snapshot, Some(300), None).is_empty());
        assert!(foreground_candidate_pids(&snapshot, None, None).is_empty());
    }

    #[test]
    fn active_and_visual_frontmost_apps_are_both_suppressed() {
        let snapshot = vec![frame(10, 100), frame(20, 200)];
        assert_eq!(
            foreground_candidate_pids(&snapshot, Some(200), Some(100)),
            HashSet::from([100, 200])
        );
    }

    #[test]
    fn foreground_uses_visible_order_when_all_window_order_disagrees() {
        let snapshot = vec![frame(10, 100)];
        // Reproduces the macOS 26 report: the all-window response puts
        // background TextEdit first even after filtering `is_on_screen`.
        let all_windows = crate::windows::WindowEnumeration {
            windows: vec![
                visible_window(10, 100, "TextEdit", 20),
                visible_window(20, 200, "Chrome", 10),
            ],
            current_space_id: None,
            succeeded: true,
        };
        let visible_windows = crate::windows::WindowEnumeration {
            windows: vec![
                visible_window(20, 200, "Chrome", 20),
                visible_window(10, 100, "TextEdit", 10),
            ],
            current_space_id: None,
            succeeded: true,
        };
        assert_eq!(
            visually_frontmost_app_key_in(&snapshot, &all_windows.windows, 999, |_| false),
            Some(100),
            "the old full-list ordering incorrectly suppresses TextEdit"
        );
        let observed = foreground_candidates_from_visible_windows(
            &snapshot,
            &visible_windows,
            Some(200),
            Some(999),
            |_| false,
        );
        let suppressed = suppression_after_window_enumeration(
            &HashSet::from([100]),
            observed,
            visible_windows.succeeded,
        );
        assert!(suppressed.is_empty(), "background TextEdit must reappear");
        assert!(frame_needs_live_capture(&snapshot[0], &suppressed));
        assert!(live_targets_from_window_enumeration(&all_windows)
            .unwrap()
            .contains(&(100, 10)));
    }

    #[test]
    fn visible_foreground_snapshot_does_not_prune_offscreen_targets() {
        let snapshot = vec![frame(10, 100), frame(20, 200)];
        let mut offscreen = visible_window(10, 100, "Minimized", 20);
        offscreen.is_on_screen = false;
        offscreen.on_current_space = Some(false);
        let front = visible_window(20, 200, "Foreground", 10);
        let all_windows = crate::windows::WindowEnumeration {
            windows: vec![offscreen, front.clone()],
            current_space_id: None,
            succeeded: true,
        };
        let visible_windows = crate::windows::WindowEnumeration {
            windows: vec![front],
            current_space_id: None,
            succeeded: true,
        };
        let live_targets = live_targets_from_window_enumeration(&all_windows).unwrap();
        assert_eq!(live_targets, HashSet::from([(100, 10), (200, 20)]));
        let suppressed = foreground_candidates_from_visible_windows(
            &snapshot,
            &visible_windows,
            Some(200),
            Some(999),
            |_| false,
        );
        assert_eq!(suppressed, HashSet::from([200]));
        assert!(!frame_is_suppressed(&snapshot[0], &suppressed));
    }

    #[test]
    fn failed_visible_snapshot_preserves_suppression_but_empty_success_clears_it() {
        let snapshot = vec![frame(10, 100), frame(20, 200)];
        for succeeded in [false, true] {
            let visible_windows = crate::windows::WindowEnumeration {
                windows: Vec::new(),
                current_space_id: None,
                succeeded,
            };
            let observed = foreground_candidates_from_visible_windows(
                &snapshot,
                &visible_windows,
                Some(200),
                Some(999),
                |_| false,
            );
            let suppressed = suppression_after_window_enumeration(
                &HashSet::from([100]),
                observed,
                visible_windows.succeeded,
            );
            assert_eq!(
                suppressed,
                if succeeded {
                    HashSet::from([200])
                } else {
                    HashSet::from([100, 200])
                }
            );
        }
    }

    #[test]
    fn visible_foreground_snapshot_keeps_exact_delegated_panel_ownership() {
        let snapshot = vec![delegated_frame(77, 42, 900), delegated_frame(78, 43, 900)];
        let visible_windows = crate::windows::WindowEnumeration {
            windows: vec![
                visible_window(78, 900, "Open and Save Panel Service", 20),
                visible_window(20, 200, "Chrome", 10),
            ],
            current_space_id: None,
            succeeded: true,
        };
        let suppressed = foreground_candidates_from_visible_windows(
            &snapshot,
            &visible_windows,
            Some(200),
            Some(999),
            |pid| pid == 900,
        );
        assert_eq!(suppressed, HashSet::from([43]));
        assert!(!frame_is_suppressed(&snapshot[0], &suppressed));
        assert!(frame_is_suppressed(&snapshot[1], &suppressed));
    }

    #[test]
    fn delegated_panel_is_suppressed_by_its_logical_host_not_shared_helper() {
        let snapshot = vec![delegated_frame(77, 42, 900)];
        let suppressed = foreground_candidate_pids(&snapshot, Some(42), None);
        assert_eq!(suppressed, HashSet::from([42]));
        assert!(frame_is_suppressed(&snapshot[0], &suppressed));
        assert!(!frame_is_suppressed(&snapshot[0], &HashSet::from([900])));
    }

    #[test]
    fn visual_frontmost_selection_skips_the_pip_process_and_auxiliary_overlays() {
        let windows = vec![
            visible_window(1, 999, "Cua Driver Local", 100),
            visible_window(2, 300, "CursorUIViewService", 90),
            visible_window(3, 100, "Blender", 80),
            visible_window(4, 200, "Terminal", 70),
        ];
        assert_eq!(
            visually_frontmost_app_key_in(&[frame(3, 100)], &windows, 999, |pid| pid == 300),
            Some(100)
        );
    }

    #[test]
    fn visual_frontmost_selection_rejects_off_space_and_zero_sized_windows() {
        let mut off_space = visible_window(1, 100, "Off Space", 100);
        off_space.on_current_space = Some(false);
        let mut empty = visible_window(2, 200, "Empty", 90);
        empty.bounds.width = 0.0;
        let usable = visible_window(3, 300, "Usable", 80);
        assert_eq!(
            visually_frontmost_app_key_in(
                &[frame(3, 300)],
                &[off_space, empty, usable],
                999,
                |_| false
            ),
            Some(300)
        );
    }

    #[test]
    fn exact_frontmost_delegated_panel_suppresses_its_logical_host_card() {
        let snapshot = vec![delegated_frame(77, 42, 900)];
        let windows = vec![
            visible_window(77, 900, "Open and Save Panel Service", 100),
            visible_window(2, 500, "Terminal", 90),
        ];
        let visual = visually_frontmost_app_key_in(&snapshot, &windows, 999, |pid| pid == 900);
        assert_eq!(visual, Some(42));
        assert_eq!(
            foreground_candidate_pids(&snapshot, Some(500), visual),
            HashSet::from([42])
        );
    }

    #[test]
    fn sibling_panel_from_shared_helper_does_not_suppress_the_wrong_host() {
        let snapshot = vec![delegated_frame(77, 42, 900), delegated_frame(78, 43, 900)];
        let windows = vec![visible_window(78, 900, "Open and Save Panel Service", 100)];
        let visual = visually_frontmost_app_key_in(&snapshot, &windows, 999, |pid| pid == 900);
        assert_eq!(visual, Some(43));
    }

    #[test]
    fn foreground_visibility_watcher_runs_for_unsuppressed_cards() {
        assert!(foreground_visibility_watcher_needed(&[frame(10, 100)]));
        assert!(!foreground_visibility_watcher_needed(&[]));
    }

    #[test]
    fn failed_window_enumeration_is_not_authoritative_absence() {
        let failed = crate::windows::WindowEnumeration {
            windows: Vec::new(),
            current_space_id: None,
            succeeded: false,
        };
        let successful_empty = crate::windows::WindowEnumeration {
            windows: Vec::new(),
            current_space_id: Some(1),
            succeeded: true,
        };

        assert!(live_targets_from_window_enumeration(&failed).is_none());
        assert_eq!(
            live_targets_from_window_enumeration(&successful_empty),
            Some(HashSet::new())
        );
    }

    #[test]
    fn suppression_hides_every_window_of_the_frontmost_app() {
        let suppressed = HashSet::from([100]);
        assert!(frame_is_suppressed(&frame(10, 100), &suppressed));
        assert!(frame_is_suppressed(&frame(20, 100), &suppressed));
        assert!(!frame_is_suppressed(&frame(10, 200), &suppressed));
        assert!(!frame_needs_live_capture(&frame(10, 100), &suppressed));
        assert!(frame_needs_live_capture(&frame(10, 200), &suppressed));
    }

    #[test]
    fn foreground_card_hides_and_reappears_on_the_same_transition() {
        let mut state = ForegroundVisibilityState::default();
        assert!(update_foreground_visibility_state(
            &mut state,
            HashSet::from([100])
        ));
        assert_eq!(state.suppressed_pids, HashSet::from([100]));
        assert!(!update_foreground_visibility_state(
            &mut state,
            HashSet::from([100])
        ));
        assert!(update_foreground_visibility_state(
            &mut state,
            HashSet::new()
        ));
        assert!(state.suppressed_pids.is_empty());
    }

    #[test]
    fn delayed_visibility_refresh_cannot_reveal_a_newly_foreground_card() {
        let mut state = ForegroundVisibilityState::default();
        let old_refresh = ForegroundVisibilityRefresh {
            observed_epoch: state.epoch,
            suppressed_pids: HashSet::new(),
            candidates_changed: false,
        };
        // A synchronous observation renders the newly foreground app before
        // the old worker's completion reaches the main queue.
        update_foreground_visibility_state(&mut state, HashSet::from([100]));
        let applied_epoch = state.epoch;
        let (suppressed, render, resample) =
            apply_foreground_visibility_refresh(&mut state, &old_refresh);
        assert_eq!(suppressed, HashSet::from([100]));
        assert!(frame_is_suppressed(&frame(10, 100), &suppressed));
        assert!(
            !render,
            "a stale sample must not rebuild a visible old card"
        );
        assert!(resample);
        assert_eq!(state.epoch, applied_epoch);
        assert_eq!(state.suppressed_pids, HashSet::from([100]));
    }

    #[test]
    fn stale_visibility_refresh_preserves_the_candidate_pruning_render() {
        let mut state = ForegroundVisibilityState::default();
        let old_refresh = ForegroundVisibilityRefresh {
            observed_epoch: state.epoch,
            suppressed_pids: HashSet::new(),
            candidates_changed: true,
        };
        update_foreground_visibility_state(&mut state, HashSet::from([100]));
        let (suppressed, render, resample) =
            apply_foreground_visibility_refresh(&mut state, &old_refresh);
        assert_eq!(suppressed, HashSet::from([100]));
        assert!(
            render,
            "worker pruning must still remove obsolete card views"
        );
        assert!(
            resample,
            "resample instead of treating stale suppression as applied"
        );
    }

    #[test]
    fn visibility_state_is_committed_only_when_a_current_result_is_applied() {
        let mut state = ForegroundVisibilityState::default();
        let refresh = ForegroundVisibilityRefresh {
            observed_epoch: state.epoch,
            suppressed_pids: HashSet::from([100]),
            candidates_changed: false,
        };
        assert!(
            state.suppressed_pids.is_empty(),
            "a pending sample is not applied state"
        );
        let (suppressed, render, resample) =
            apply_foreground_visibility_refresh(&mut state, &refresh);
        assert_eq!(suppressed, HashSet::from([100]));
        assert!(render);
        assert!(!resample);
        assert_ne!(state.epoch, refresh.observed_epoch);
        let (_, duplicate_render, duplicate_resample) =
            apply_foreground_visibility_refresh(&mut state, &refresh);
        assert!(!duplicate_render);
        assert!(
            duplicate_resample,
            "duplicate completions cannot commit twice"
        );
    }

    #[test]
    fn activation_invalidates_a_pending_visibility_sample_without_clearing_state() {
        let mut state = ForegroundVisibilityState::default();
        update_foreground_visibility_state(&mut state, HashSet::from([100]));
        let refresh = ForegroundVisibilityRefresh {
            observed_epoch: state.epoch,
            suppressed_pids: HashSet::new(),
            candidates_changed: false,
        };
        state.invalidate_pending();
        let (suppressed, render, resample) =
            apply_foreground_visibility_refresh(&mut state, &refresh);
        assert_eq!(suppressed, HashSet::from([100]));
        assert!(!render);
        assert!(resample);
    }

    #[test]
    fn visibility_refresh_from_a_previous_backend_lifetime_is_stale() {
        let previous = ForegroundVisibilityState::default();
        let refresh = ForegroundVisibilityRefresh {
            observed_epoch: previous.epoch,
            suppressed_pids: HashSet::from([100]),
            candidates_changed: false,
        };
        let mut restarted = ForegroundVisibilityState::default();
        let (suppressed, render, resample) =
            apply_foreground_visibility_refresh(&mut restarted, &refresh);
        assert!(suppressed.is_empty());
        assert!(!render);
        assert!(resample);
    }

    #[test]
    fn failed_visibility_enumeration_preserves_previous_visual_suppression() {
        let previous = HashSet::from([100]);
        assert_eq!(
            suppression_after_window_enumeration(&previous, HashSet::new(), false),
            previous
        );
        assert_eq!(
            suppression_after_window_enumeration(&previous, HashSet::from([200]), false),
            HashSet::from([100, 200])
        );
        assert_eq!(
            suppression_after_window_enumeration(&previous, HashSet::from([200]), true),
            HashSet::from([200]),
            "only successful enumeration may remove the old visual owner"
        );
    }

    #[test]
    fn foreground_visibility_checks_are_throttled() {
        assert!(foreground_visibility_check_is_due(10_000, 0));
        assert!(!foreground_visibility_check_is_due(10_100, 10_000));
        assert!(foreground_visibility_check_is_due(10_250, 10_000));
    }

    #[test]
    fn stale_card_removal_rerenders_even_when_foreground_is_unchanged() {
        assert!(candidate_refresh_requires_render(false, true));
        assert!(candidate_refresh_requires_render(true, false));
        assert!(!candidate_refresh_requires_render(false, false));
    }

    #[test]
    fn cards_form_a_bounded_vertical_stack() {
        let cards = card_layout(620.0, 420.0, 5);
        assert_eq!(cards.len(), 5);
        for card in &cards {
            assert!(card.width > 0.0 && card.height >= STACK_MIN_CARD_HEIGHT);
            assert!(card.x >= STACK_INSET && card.y >= STACK_INSET);
            assert!(card.x + card.width <= 620.0 - STACK_INSET + f64::EPSILON);
            assert!(card.y + card.height <= 420.0 - STACK_INSET + f64::EPSILON);
        }
        for pair in cards.windows(2) {
            let behind = pair[0];
            let in_front = pair[1];
            assert!(behind.x > in_front.x);
            assert!(behind.y > in_front.y);
            assert!(behind.y < in_front.y + in_front.height);
        }
    }

    #[test]
    fn source_resize_fits_whole_card_without_cropping_or_gray_bands() {
        let slot = CardRect {
            x: 12.0,
            y: 10.0,
            width: 580.0,
            height: 332.0,
        };
        for (width, height) in [(300.0, 500.0), (850.0, 500.0), (300.0, 500.0)] {
            let rect = fitted_card_rect(slot, width, height);
            assert!(rect.width <= slot.width && rect.height <= slot.height);
            assert!(rect.x >= slot.x && rect.y >= slot.y);
            assert!(rect.x + rect.width <= slot.x + slot.width + f64::EPSILON);
            assert!(rect.y + rect.height <= slot.y + slot.height + f64::EPSILON);
            assert!((rect.width / rect.height - width / height).abs() < 0.000_001);
        }
    }

    #[test]
    fn fitted_stack_keeps_each_sources_aspect_ratio_inside_its_slot() {
        for (slot, (width, height)) in card_layout(620.0, 420.0, 3).into_iter().zip([
            (300.0, 500.0),
            (850.0, 500.0),
            (1200.0, 600.0),
        ]) {
            let fitted = fitted_card_rect(slot, width, height);
            assert!((fitted.width / fitted.height - width / height).abs() < 0.000_001);
            assert!(fitted.width <= slot.width && fitted.height <= slot.height);
        }
    }

    #[test]
    fn resize_handles_follow_union_of_mixed_aspect_cards() {
        use objc2_foundation::{NSPoint, NSRect, NSSize};
        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(620.0, 420.0));
        let mut layout = vec![
            CardRect {
                x: 210.0,
                y: 40.0,
                width: 180.0,
                height: 350.0,
            },
            CardRect {
                x: 30.0,
                y: 80.0,
                width: 560.0,
                height: 260.0,
            },
            CardRect {
                x: 90.0,
                y: 20.0,
                width: 440.0,
                height: 220.0,
            },
        ];
        let rects = resize_cursor_rects(bounds, &layout);
        let half = RESIZE_HIT_INSET / 2.0;
        assert_eq!(rects.len(), 4);
        for ((rect, _), (x, y)) in
            rects
                .iter()
                .zip([(30.0, 20.0), (590.0, 20.0), (30.0, 390.0), (590.0, 390.0)])
        {
            assert_eq!((rect.origin.x + half, rect.origin.y + half), (x, y));
        }
        layout.reverse();
        assert_eq!(rects, resize_cursor_rects(bounds, &layout));
        assert!(resize_cursor_rects(bounds, &[]).is_empty());
        assert_eq!(resize_cursor_rects(bounds, &layout[..1]).len(), 4);
    }

    #[test]
    fn default_window_size_tracks_screen_size_and_explicit_geometry_wins() {
        assert_eq!(
            initial_pip_size(1728.0, 1000.0, PipGeometry::default()),
            (346.0, 200.0)
        );
        assert_eq!(
            initial_pip_size(
                1728.0,
                1000.0,
                PipGeometry {
                    width: 500,
                    height: 360,
                    x: Some(24),
                    y: Some(24),
                }
            ),
            (500.0, 360.0)
        );
    }

    #[test]
    fn resize_geometry_keeps_far_edges_anchored() {
        use objc2_foundation::{NSPoint, NSRect, NSSize};
        let start = NSRect::new(NSPoint::new(100.0, 100.0), NSSize::new(400.0, 260.0));
        let resized = resized_window_frame(
            start,
            40.0,
            30.0,
            RESIZE_LEFT | RESIZE_TOP,
            NSSize::new(320.0, 200.0),
        );
        assert_eq!(resized.origin.x, 140.0);
        assert_eq!(resized.size.width, 360.0);
        assert_eq!(resized.origin.y, 100.0);
        assert_eq!(resized.size.height, 290.0);
    }

    #[test]
    fn activating_a_card_requires_a_stationary_pointer_and_window() {
        use objc2_foundation::NSPoint;

        let origin = NSPoint::new(100.0, 100.0);
        assert!(pointer_gesture_is_click(
            NSPoint::new(20.0, 30.0),
            NSPoint::new(21.0, 31.0),
            origin,
            origin,
        ));
        assert!(!pointer_gesture_is_click(
            NSPoint::new(20.0, 30.0),
            NSPoint::new(40.0, 50.0),
            origin,
            NSPoint::new(120.0, 120.0),
        ));
        // AppKit's global pointer sample can occasionally be stale after
        // performWindowDragWithEvent:, so window movement alone must veto
        // activation as well.
        assert!(!pointer_gesture_is_click(
            NSPoint::new(20.0, 30.0),
            NSPoint::new(20.0, 30.0),
            origin,
            NSPoint::new(140.0, 100.0),
        ));
    }
}
