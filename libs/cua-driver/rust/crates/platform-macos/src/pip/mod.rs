//! Native macOS Picture-in-Picture stack for Computer Use.
//!
//! Exact native windows are captured continuously, one app owns one bounded
//! card, and all cards live inside one borderless native stack. The daemon is
//! in-process, so it does not need Codex's cross-process CAContext transport.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pip_preview::{
    PipBackend, PipBackendFactory, PipConfig, PipFrame, PipGeometry, PipViewModel,
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
}

struct LiveStreamEntry {
    target_pid: i64,
    window_id: u64,
    stream: Option<SCStream>,
    cancelled: Arc<AtomicBool>,
    frame_pending: Arc<AtomicBool>,
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
}

#[derive(Default)]
struct ForegroundVisibilityState {
    suppressed_pids: HashSet<i64>,
}

struct ForegroundVisibilityWatcher {
    cancelled: Arc<AtomicBool>,
}

struct ForegroundVisibilityRefresh {
    suppressed_pids: HashSet<i64>,
    render_required: bool,
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

const LIVE_CAPTURE_FPS: i32 = 12;
const LIVE_CAPTURE_MAX_SIDE: f64 = 960.0;
const LIVE_CAPTURE_WINDOW_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const FOREGROUND_VISIBILITY_CHECK_INTERVAL: Duration = Duration::from_millis(250);
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
        let publication_generation = reserve_pip_publication(&frame.target);
        let target_for_failure = frame.target.clone();
        if std::thread::Builder::new()
            .name(format!("cua-pip-seed-{app_pid}"))
            .spawn(move || {
                if !pip_target_session_is_live(&frame.target)
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
                let Some(window) = crate::windows::all_windows().into_iter().find(|window| {
                    window.pid == pid && u64::from(window.window_id) == frame.target.window_id
                }) else {
                    cancel_pip_publication(&frame.target, publication_generation);
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
            cancel_pip_publication(&target_for_failure, publication_generation);
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
        // PiP is strictly downstream of authoritative observations. Without a
        // response image there is nothing safe to seed: wait for the next full
        // or screenshot-only observation rather than competing with its fresh
        // capture (or click calibration) through the single-frame API.
        tracing::debug!(
            target: "pip",
            pid = target.pid,
            window_id = target.window_id,
            "PiP target has no observation seed yet; retaining existing cards"
        );
    }

    fn end_session(&self, session_id: &str) {
        remove_session_delegation_frame_proofs(session_id);
        dispatch_to_main(session_id.to_owned(), end_session_cb);
    }

    fn set_input_passthrough(&self, passthrough: bool) -> anyhow::Result<()> {
        dispatch_to_main_sync(passthrough, set_input_passthrough_cb);
        Ok(())
    }

    fn shutdown(self: Box<Self>) {
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
        },
    );
    generation
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

fn remove_session_delegation_frame_proofs(session_id: &str) {
    let mut state = DELEGATION_PROOF_STATE.lock().unwrap();
    state
        .frame_proofs
        .retain(|_, proof| proof.session_id.as_deref() != Some(session_id));
    state
        .latest_publications
        .retain(|_, reservation| reservation.session_id.as_deref() != Some(session_id));
}

fn pip_delegation_is_live(target: &pip_preview::PipTarget) -> bool {
    if !pip_target_session_is_live(target) {
        return false;
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
    let pid = frame.target.app_key_pid();
    if !pip_target_session_is_live(&frame.target)
        || !pip_publication_is_current(&frame.target, verified.publication_generation)
        || !delegation_frame_proof_is_live(&frame.target)
        || HIDDEN_APPS.lock().unwrap().contains(&pid)
    {
        return;
    }

    let outcome = {
        let mut model = VIEW_MODEL.lock().unwrap();
        let model = model.get_or_insert_with(|| PipViewModel::new(MAX_VISIBLE_PIP_CARDS));
        model.upsert(frame)
    };

    if let Some(evicted_pid) = outcome.evicted_pid {
        remove_delegation_frame_proof(evicted_pid);
        stop_live_capture_for(evicted_pid);
    }
    if outcome.window_changed {
        stop_live_capture_for(pid);
    }
    let snapshot = current_snapshot();
    render_snapshot(&snapshot);
}

unsafe extern "C" fn end_session_cb(ctx: *mut c_void) {
    let session_id: String = *Box::from_raw(ctx as *mut String);
    let (snapshot, removed_pids) = {
        let mut model = VIEW_MODEL.lock().unwrap();
        let Some(model) = model.as_mut() else {
            return;
        };
        let removed_pids = model.remove_session(&session_id);
        let snapshot = model
            .ordered_frames()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        (snapshot, removed_pids)
    };
    for pid in removed_pids {
        remove_delegation_frame_proof(pid);
        stop_live_capture_for(pid);
    }
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

fn frame_is_suppressed(frame: &PipFrame, suppressed_pids: &HashSet<i64>) -> bool {
    suppressed_pids.contains(&frame.target.app_key_pid())
}

fn frame_needs_live_capture(frame: &PipFrame, suppressed_pids: &HashSet<i64>) -> bool {
    !frame_is_suppressed(frame, suppressed_pids)
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

fn foreground_visibility_watcher_needed(snapshot: &[PipFrame]) -> bool {
    !snapshot.is_empty()
}

fn update_foreground_visibility_state(
    state: &mut ForegroundVisibilityState,
    observed_pids: HashSet<i64>,
) -> bool {
    let changed = state.suppressed_pids != observed_pids;
    state.suppressed_pids = observed_pids;
    changed
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
                .ordered_frames()
                .into_iter()
                .map(|frame| frame.target.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // Native host→panel validation can involve AX and code-signature work.
    // Perform it without holding the view-model mutex so frame publication and
    // AppKit rendering are never serialized behind that proof.
    let checked = candidates
        .into_iter()
        .map(|target| {
            let key = (target.app_key_pid(), target.pid, target.window_id);
            let live = live_targets.contains(&(target.pid, target.window_id))
                && pip_delegation_is_live(&target)
                && delegation_frame_proof_is_live(&target);
            (key, live)
        })
        .collect::<HashMap<_, _>>();
    let (snapshot, removed_pids) = {
        let mut model = VIEW_MODEL.lock().unwrap();
        let Some(model) = model.as_mut() else {
            return (Vec::new(), false);
        };
        let removed_pids = model.retain_live_targets(|target| {
            checked
                .get(&(target.app_key_pid(), target.pid, target.window_id))
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
        (snapshot, removed_pids)
    };
    let changed = !removed_pids.is_empty();
    for pid in removed_pids {
        remove_delegation_frame_proof(pid);
        stop_live_capture_for(pid);
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

    // One raw WindowServer snapshot drives both liveness and visual-frontmost
    // selection. A failed enumeration is not authoritative absence: retain all
    // cards and retry on the next tick rather than deleting the session state.
    let enumeration = crate::windows::all_windows_including_accessory_layers_with_snapshot();
    let Some(live_targets) = live_targets_from_window_enumeration(&enumeration) else {
        return None;
    };
    let (snapshot, candidates_changed) = refresh_live_candidates(&live_targets);
    let local_pid = i32::try_from(std::process::id()).ok();
    let visual_frontmost_app_key = local_pid.and_then(|local_pid| {
        visually_frontmost_app_key_in(
            &snapshot,
            &enumeration.windows,
            local_pid,
            crate::apps::is_auxiliary_application,
        )
    });
    let suppressed = foreground_candidate_pids(
        &snapshot,
        crate::apps::frontmost_pid(),
        visual_frontmost_app_key,
    );
    let mut state = FOREGROUND_VISIBILITY_STATE.lock().unwrap();
    let suppression_changed = update_foreground_visibility_state(&mut state, suppressed.clone());
    drop(state);
    Some(ForegroundVisibilityRefresh {
        suppressed_pids: suppressed,
        render_required: candidate_refresh_requires_render(suppression_changed, candidates_changed),
    })
}

unsafe extern "C" fn refresh_foreground_visibility_cb(ctx: *mut c_void) {
    let refresh = *Box::from_raw(ctx as *mut ForegroundVisibilityRefresh);
    if refresh.render_required && HANDLES.lock().unwrap().is_some() {
        render_snapshot_with_suppressed_windows(&current_snapshot(), &refresh.suppressed_pids);
    }
    FOREGROUND_REFRESH_PENDING.store(false, Ordering::Release);
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
    if foreground_visibility_watcher_needed(snapshot) {
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
        if frame_needs_live_capture(frame, suppressed_pids) {
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

fn monotonic_ms() -> u64 {
    FOREGROUND_CLOCK_ORIGIN.elapsed().as_millis().max(1) as u64
}

fn ensure_live_capture(app_pid: i64, target_pid: i64, window_id: u64) {
    {
        let streams = LIVE_STREAMS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if streams.get(&app_pid).is_some_and(|entry| {
            entry.target_pid == target_pid
                && entry.window_id == window_id
                && !entry.cancelled.load(Ordering::Acquire)
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
                cancelled: Arc::clone(&cancelled),
                frame_pending: Arc::clone(&frame_pending),
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
                Ok(stream) => {
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
                    tracing::warn!(
                        target: "pip",
                        app_pid,
                        target_pid,
                        window_id,
                        %error,
                        "SCStream unavailable; exact-window polling is keeping PiP live"
                    );
                }
            }
        })
    {
        tracing::warn!(target: "pip", app_pid, target_pid, window_id, %error, "failed to spawn PiP capture worker");
    }
}

fn build_live_capture(
    app_pid: i64,
    target_pid: i64,
    window_id: u64,
    cancelled: Arc<AtomicBool>,
    frame_pending: Arc<AtomicBool>,
) -> anyhow::Result<SCStream> {
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
    let (output_width, output_height) =
        capture_dimensions(source_frame.size.width, source_frame.size.height);
    let filter = SCContentFilter::create()
        .with_window(&target_window)
        .build();
    let frame_interval = CMTime::new(1, LIVE_CAPTURE_FPS);
    let config = SCStreamConfiguration::new()
        .with_width(output_width)
        .with_height(output_height)
        .with_scales_to_fit(true)
        .with_preserves_aspect_ratio(true)
        .with_queue_depth(2)
        .with_minimum_frame_interval(&frame_interval)
        // The PiP window renders its own interaction cursor. Including the
        // desktop cursor in the captured app frame would make the source
        // arrow appear underneath that hand/resize cursor.
        .with_shows_cursor(false);

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
    Ok(stream)
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
    let image_view = CARD_HANDLES
        .lock()
        .unwrap()
        .get(&frame.app_pid)
        .map(|handles| handles.image_view)
        .unwrap_or(0) as *mut AnyObject;
    if image_view.is_null() {
        return;
    }

    let image: *mut AnyObject = match frame.image {
        LiveFrameImage::CgImage(image) => {
            let cg_image = image.as_ptr() as *mut NativeCGImage;
            let allocated: *mut AnyObject = msg_send![objc2::class!(NSImage), alloc];
            msg_send![allocated, initWithCGImage: cg_image size: NSSize::new(0.0, 0.0)]
        }
    };
    if !image.is_null() {
        let _: () = msg_send![image_view, setImage: image];
        let _: () = msg_send![image, release];
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

fn activate_target_window(target: ClickedTarget) {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};

    if !pip_target_session_is_live(&target.target)
        || !delegation_frame_proof_is_live(&target.target)
        || !physical_target_is_live(target.target.pid, target.target.window_id)
        || !pip_delegation_is_live(&target.target)
    {
        return;
    }
    let (Ok(app_pid), Ok(target_pid), Ok(window_id)) = (
        libc::pid_t::try_from(target.target.app_key_pid()),
        libc::pid_t::try_from(target.target.pid),
        u32::try_from(target.target.window_id),
    ) else {
        return;
    };
    if let Some(app) =
        unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(app_pid) }
    {
        unsafe {
            app.activateWithOptions(
                NSApplicationActivationOptions::NSApplicationActivateAllWindows,
            );
        }
    }
    // Host activation can synchronously close or replace a modal panel. Never
    // carry the pre-activation proof across that side effect.
    if !physical_target_is_live(target.target.pid, target.target.window_id)
        || !pip_delegation_is_live(&target.target)
        || !delegation_frame_proof_is_live(&target.target)
    {
        return;
    }
    if app_pid == target_pid {
        let _ = crate::input::skylight::set_front_process_persistently(target_pid, window_id);
    }
    // Keep the final proof adjacent to the exact-window activation. This also
    // covers same-process windows that may close during app activation.
    if !physical_target_is_live(target.target.pid, target.target.window_id)
        || !pip_delegation_is_live(&target.target)
        || !delegation_frame_proof_is_live(&target.target)
    {
        return;
    }
    let _ = crate::input::skylight::make_exact_window_key(target_pid, window_id);
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
        let count = VIEW_MODEL
            .lock()
            .unwrap()
            .as_ref()
            .map(|model| model.ordered_frames().len())
            .unwrap_or(0);
        let layout = card_layout(bounds.size.width, bounds.size.height, count);
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

    let block = RcBlock::new(move |_event: *mut AnyObject| {
        schedule_cursor_refresh();
    });
    let global_monitor: *mut AnyObject = msg_send![
        objc2::class!(NSEvent),
        addGlobalMonitorForEventsMatchingMask: 0x20u64
        handler: &*block
    ];
    let local_block = RcBlock::new(move |event: *mut AnyObject| -> *mut AnyObject {
        schedule_cursor_refresh();
        event
    });
    let local_monitor: *mut AnyObject = msg_send![
        objc2::class!(NSEvent),
        addLocalMonitorForEventsMatchingMask: 0x20u64
        handler: &*local_block
    ];
    (global_monitor as usize, local_monitor as usize)
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
        let (target, front_pid) = VIEW_MODEL
            .lock()
            .unwrap()
            .as_ref()
            .map(|model| {
                (
                    pid.and_then(|pid| {
                        model.frame_for_app(pid).map(|frame| ClickedTarget {
                            target: frame.target.clone(),
                        })
                    }),
                    model
                        .ordered_frames()
                        .last()
                        .map(|frame| frame.target.app_key_pid()),
                )
            })
            .unwrap_or((None, None));
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

    let (Some(back), Some(front)) = (layout.first(), layout.last()) else {
        return Vec::new();
    };
    let edge = RESIZE_HIT_INSET;
    let half = edge / 2.0;
    let clamp_x = |x: f64| x.clamp(0.0, (bounds.size.width - edge).max(0.0));
    let clamp_y = |y: f64| y.clamp(0.0, (bounds.size.height - edge).max(0.0));
    vec![
        (
            NSRect::new(
                NSPoint::new(clamp_x(front.x - half), clamp_y(front.y - half)),
                NSSize::new(edge, edge),
            ),
            RESIZE_LEFT | RESIZE_BOTTOM,
        ),
        (
            NSRect::new(
                NSPoint::new(
                    clamp_x(front.x + front.width - half),
                    clamp_y(front.y - half),
                ),
                NSSize::new(edge, edge),
            ),
            RESIZE_RIGHT | RESIZE_BOTTOM,
        ),
        (
            NSRect::new(
                NSPoint::new(clamp_x(back.x - half), clamp_y(back.y + back.height - half)),
                NSSize::new(edge, edge),
            ),
            RESIZE_LEFT | RESIZE_TOP,
        ),
        (
            NSRect::new(
                NSPoint::new(
                    clamp_x(back.x + back.width - half),
                    clamp_y(back.y + back.height - half),
                ),
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

fn aspect_fill_rect(
    container_width: f64,
    container_height: f64,
    image_width: f64,
    image_height: f64,
) -> CardRect {
    if image_width <= 0.0 || image_height <= 0.0 {
        return CardRect {
            x: 0.0,
            y: 0.0,
            width: container_width,
            height: container_height,
        };
    }
    let scale = (container_width / image_width).max(container_height / image_height);
    let width = image_width * scale;
    let height = image_height * scale;
    CardRect {
        x: (container_width - width) / 2.0,
        y: (container_height - height) / 2.0,
        width,
        height,
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

    let image = image_from_png(&frame.png_bytes);
    let image_rect = if image.is_null() {
        CardRect {
            x: 0.0,
            y: 0.0,
            width: rect.width,
            height: rect.height,
        }
    } else {
        let image_size: NSSize = msg_send![image, size];
        aspect_fill_rect(rect.width, rect.height, image_size.width, image_size.height)
    };

    let image_view: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSImageView), alloc];
        msg_send![allocated, initWithFrame: NSRect::new(
            NSPoint::new(image_rect.x, image_rect.y),
            NSSize::new(image_rect.width, image_rect.height),
        )]
    };
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
    }
}

unsafe fn render_snapshot(snapshot: &[PipFrame]) {
    let enumeration = crate::windows::all_windows_including_accessory_layers_with_snapshot();
    let visual_frontmost_app_key = enumeration
        .succeeded
        .then(|| {
            i32::try_from(std::process::id())
                .ok()
                .and_then(|local_pid| {
                    visually_frontmost_app_key_in(
                        snapshot,
                        &enumeration.windows,
                        local_pid,
                        crate::apps::is_auxiliary_application,
                    )
                })
        })
        .flatten();
    let suppressed = foreground_candidate_pids(
        snapshot,
        crate::apps::frontmost_pid(),
        visual_frontmost_app_key,
    );
    update_foreground_visibility_state(
        &mut FOREGROUND_VISIBILITY_STATE.lock().unwrap(),
        suppressed.clone(),
    );
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
        .filter(|frame| !frame_is_suppressed(frame, suppressed_pids))
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

    CARD_HANDLES.lock().unwrap().clear();
    CARD_VIEW_PIDS.lock().unwrap().clear();
    RESIZE_VIEW_DIRECTIONS.lock().unwrap().clear();
    let empty: *mut AnyObject = msg_send![objc2::class!(NSArray), array];
    let _: () = msg_send![canvas, setSubviews: empty];
    let bounds: objc2_foundation::NSRect = msg_send![canvas, bounds];
    let layout = card_layout(
        bounds.size.width,
        bounds.size.height,
        visible_snapshot.len(),
    );
    for (frame, rect) in visible_snapshot.iter().zip(layout.iter().copied()) {
        let handles = render_card(canvas, delegate, frame, rect);
        CARD_HANDLES
            .lock()
            .unwrap()
            .insert(frame.target.app_key_pid(), handles);
    }
    install_resize_hit_views(canvas, bounds, &layout);
    let _: () = msg_send![window, invalidateCursorRectsForView: canvas];
    if let Some((window, location)) = current_pip_cursor_location() {
        refresh_cursor_at_content_point(window, location);
    }

    let hovered_pid = HOVERED_APP.lock().unwrap().take();
    set_hovered_app(hovered_pid);

    if visible_snapshot.is_empty() {
        hide_custom_cursor();
        let _: () = msg_send![window, orderOut: std::ptr::null_mut::<AnyObject>()];
    } else {
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
    *VIEW_MODEL.lock().unwrap() = Some(PipViewModel::new(MAX_VISIBLE_PIP_CARDS));

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
    fn aspect_fill_crops_instead_of_letterboxing() {
        let rect = aspect_fill_rect(580.0, 332.0, 860.0, 535.0);
        assert!(rect.width >= 580.0);
        assert!(rect.height >= 332.0);
        assert!(rect.x <= 0.0);
        assert!(rect.y <= 0.0);
        assert!((rect.width / rect.height - 860.0 / 535.0).abs() < 0.000_001);
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
