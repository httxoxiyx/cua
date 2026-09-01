//! Native macOS Picture-in-Picture stack for Computer Use.
//!
//! Exact native windows are captured continuously, one app owns one bounded
//! card, and all cards live inside one borderless native stack. The daemon is
//! in-process, so it does not need Codex's cross-process CAContext transport.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    cursor_image_view: usize,
}

#[derive(Clone, Copy)]
struct NativeCardHandles {
    card: usize,
    image_view: usize,
    controls: usize,
    resting_rect: CardRect,
}

struct LiveStreamEntry {
    window_id: u64,
    stream: Option<SCStream>,
    cancelled: Arc<AtomicBool>,
    frame_pending: Arc<AtomicBool>,
}

struct LiveFrame {
    pid: i64,
    window_id: u64,
    image: LiveFrameImage,
    frame_pending: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
struct ClickedTarget {
    pid: i64,
    window_id: u64,
}

#[derive(Clone, Copy)]
struct CardGesture {
    target: Option<ClickedTarget>,
    front_pid: Option<i64>,
    start_mouse: objc2_foundation::NSPoint,
    start_window_origin: objc2_foundation::NSPoint,
    dragged: bool,
}

enum LiveFrameImage {
    CgImage(screencapturekit::CGImage),
    Png(Vec<u8>),
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
static LIVE_STREAMS: LazyLock<Mutex<HashMap<i64, LiveStreamEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const LIVE_CAPTURE_FPS: i32 = 12;
const LIVE_CAPTURE_MAX_SIDE: f64 = 960.0;
const LIVE_CAPTURE_WINDOW_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const LIVE_CAPTURE_WATCHDOG: Duration = Duration::from_millis(750);
const FALLBACK_CAPTURE_INTERVAL: Duration = Duration::from_millis(500);
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
        if frame.target.pid <= 0
            || frame.target.window_id == 0
            || HIDDEN_APPS.lock().unwrap().contains(&frame.target.pid)
        {
            return;
        }

        if let Ok(pid) = i32::try_from(frame.target.pid) {
            if let Some(window) = crate::windows::all_windows().into_iter().find(|window| {
                window.pid == pid && u64::from(window.window_id) == frame.target.window_id
            }) {
                frame.target.app_name = window.app_name;
                frame.target.window_title =
                    (!window.title.trim().is_empty()).then_some(window.title);
            } else if frame.target.app_name.is_empty() {
                frame.target.app_name = crate::apps::get_app_name_for_pid(pid).unwrap_or_default();
            }
        }
        if frame.target.app_name.trim().is_empty() {
            frame.target.app_name = format!("App {}", frame.target.pid);
        }

        dispatch_to_main(frame, push_frame_cb);
    }

    fn set_input_passthrough(&self, passthrough: bool) -> anyhow::Result<()> {
        dispatch_to_main_sync(passthrough, set_input_passthrough_cb);
        Ok(())
    }

    fn shutdown(self: Box<Self>) {
        stop_all_live_capture();
        dispatch_to_main((), shutdown_cb);
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
    let frame: PipFrame = *Box::from_raw(ctx as *mut PipFrame);
    if HIDDEN_APPS.lock().unwrap().contains(&frame.target.pid) {
        return;
    }

    let pid = frame.target.pid;
    let window_id = frame.target.window_id;
    let (snapshot, outcome) = {
        let mut model = VIEW_MODEL.lock().unwrap();
        let model = model.get_or_insert_with(|| PipViewModel::new(MAX_VISIBLE_PIP_CARDS));
        let outcome = model.upsert(frame);
        let snapshot = model
            .ordered_frames()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        (snapshot, outcome)
    };

    if let Some(evicted_pid) = outcome.evicted_pid {
        stop_live_capture_for(evicted_pid);
    }
    if outcome.window_changed {
        stop_live_capture_for(pid);
    }
    render_snapshot(&snapshot);
    ensure_live_capture(pid, window_id);
}

fn current_snapshot() -> Vec<PipFrame> {
    VIEW_MODEL
        .lock()
        .unwrap()
        .as_ref()
        .map(|model| model.ordered_frames().into_iter().cloned().collect())
        .unwrap_or_default()
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

fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn stream_frame_is_fresh(now_ms: u64, last_stream_frame_ms: u64) -> bool {
    last_stream_frame_ms != 0
        && now_ms.saturating_sub(last_stream_frame_ms) < LIVE_CAPTURE_WATCHDOG.as_millis() as u64
}

fn ensure_live_capture(pid: i64, window_id: u64) {
    {
        let streams = LIVE_STREAMS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if streams.get(&pid).is_some_and(|entry| {
            entry.window_id == window_id && !entry.cancelled.load(Ordering::Acquire)
        }) {
            return;
        }
    }

    stop_live_capture_for(pid);
    let cancelled = Arc::new(AtomicBool::new(false));
    let frame_pending = Arc::new(AtomicBool::new(false));
    let last_stream_frame_ms = Arc::new(AtomicU64::new(0));
    LIVE_STREAMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            pid,
            LiveStreamEntry {
                window_id,
                stream: None,
                cancelled: Arc::clone(&cancelled),
                frame_pending: Arc::clone(&frame_pending),
            },
        );

    start_polling_fallback(
        pid,
        window_id,
        Arc::clone(&cancelled),
        Arc::clone(&frame_pending),
        Arc::clone(&last_stream_frame_ms),
    );

    if let Err(error) = std::thread::Builder::new()
        .name(format!("cua-pip-{pid}"))
        .spawn(move || {
            match build_live_capture(
                pid,
                window_id,
                Arc::clone(&cancelled),
                Arc::clone(&frame_pending),
                Arc::clone(&last_stream_frame_ms),
            ) {
                Ok(stream) => {
                    let mut streams = LIVE_STREAMS
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let current = streams.get_mut(&pid).filter(|entry| {
                        entry.window_id == window_id
                            && Arc::ptr_eq(&entry.cancelled, &cancelled)
                            && !cancelled.load(Ordering::Acquire)
                    });
                    if let Some(entry) = current {
                        entry.stream = Some(stream);
                        tracing::info!(
                            target: "pip",
                            pid,
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
                        pid,
                        window_id,
                        %error,
                        "SCStream unavailable; exact-window polling is keeping PiP live"
                    );
                }
            }
        })
    {
        tracing::warn!(target: "pip", pid, window_id, %error, "failed to spawn PiP capture worker");
    }
}

fn start_polling_fallback(
    pid: i64,
    window_id: u64,
    cancelled: Arc<AtomicBool>,
    frame_pending: Arc<AtomicBool>,
    last_stream_frame_ms: Arc<AtomicU64>,
) {
    let Ok(native_window_id) = u32::try_from(window_id) else {
        return;
    };
    if let Err(error) = std::thread::Builder::new()
        .name(format!("cua-pip-watchdog-{pid}"))
        .spawn(move || {
            while !cancelled.load(Ordering::Acquire) {
                let last_stream_frame = last_stream_frame_ms.load(Ordering::Acquire);
                let stream_is_fresh = stream_frame_is_fresh(wall_clock_ms(), last_stream_frame);
                if !stream_is_fresh && !frame_pending.swap(true, Ordering::AcqRel) {
                    match crate::capture::screenshot_window_bytes(native_window_id) {
                        Ok(png_bytes) => dispatch_to_main(
                            LiveFrame {
                                pid,
                                window_id,
                                image: LiveFrameImage::Png(png_bytes),
                                frame_pending: Arc::clone(&frame_pending),
                            },
                            push_live_frame_cb,
                        ),
                        Err(error) => {
                            frame_pending.store(false, Ordering::Release);
                            tracing::debug!(target: "pip", pid, window_id, %error, "PiP fallback capture failed");
                        }
                    }
                }
                std::thread::sleep(FALLBACK_CAPTURE_INTERVAL);
            }
        })
    {
        tracing::warn!(target: "pip", pid, window_id, %error, "failed to spawn PiP capture watchdog");
    }
}

fn build_live_capture(
    pid: i64,
    window_id: u64,
    cancelled: Arc<AtomicBool>,
    frame_pending: Arc<AtomicBool>,
    last_stream_frame_ms: Arc<AtomicU64>,
) -> anyhow::Result<SCStream> {
    let native_window_id = u32::try_from(window_id)
        .map_err(|_| anyhow::anyhow!("window id {window_id} does not fit a CGWindowID"))?;
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
        if let Some(window) = content
            .windows()
            .into_iter()
            .find(|window| window.window_id() == native_window_id)
        {
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
                        last_stream_frame_ms.store(wall_clock_ms(), Ordering::Release);
                        dispatch_to_main(
                            LiveFrame {
                                pid,
                                window_id,
                                image: LiveFrameImage::CgImage(image),
                                frame_pending: Arc::clone(&frame_pending),
                            },
                            push_live_frame_cb,
                        )
                    }
                    Err(error) => {
                        frame_pending.store(false, Ordering::Release);
                        tracing::debug!(target: "pip", pid, window_id, error, "live PiP frame had no image");
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

fn stop_live_capture_for(pid: i64) {
    let entry = LIVE_STREAMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&pid);
    if let Some(entry) = entry {
        entry.cancelled.store(true, Ordering::Release);
        entry.frame_pending.store(false, Ordering::Release);
        if let Some(stream) = entry.stream {
            if let Err(error) = stream.stop_capture() {
                tracing::debug!(target: "pip", pid, %error, "failed to stop app PiP capture cleanly");
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
    if HIDDEN_APPS.lock().unwrap().contains(&frame.pid)
        || !VIEW_MODEL
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|model| model.frame_for_app(frame.pid))
            .is_some_and(|model_frame| model_frame.target.window_id == frame.window_id)
    {
        return;
    }
    let image_view = CARD_HANDLES
        .lock()
        .unwrap()
        .get(&frame.pid)
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
        LiveFrameImage::Png(png_bytes) => image_from_png(&png_bytes),
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

fn pip_cursor_image_view_class() -> &'static objc2::runtime::AnyClass {
    use objc2::class;
    use objc2::declare::ClassBuilder;

    static CLASS: OnceLock<&'static objc2::runtime::AnyClass> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut builder = ClassBuilder::new("CuaDriverPipCursorImageView", class!(NSImageView))
            .expect("CuaDriverPipCursorImageView already registered");
        unsafe {
            builder.add_method(
                objc2::sel!(hitTest:),
                cursor_image_hit_test as extern "C" fn(_, _, _) -> _,
            );
        }
        builder.register()
    })
}

extern "C" fn cursor_image_hit_test(
    _view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    _point: objc2_foundation::NSPoint,
) -> *mut objc2::runtime::AnyObject {
    std::ptr::null_mut()
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

    let front_pid = VIEW_MODEL
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|model| model.ordered_frames().last().map(|frame| frame.target.pid));
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

    let (Ok(pid), Ok(window_id)) = (
        libc::pid_t::try_from(target.pid),
        u32::try_from(target.window_id),
    ) else {
        return;
    };
    let _ = crate::input::skylight::set_front_process_persistently(pid, window_id);
    let _ = crate::input::skylight::make_exact_window_key(pid, window_id);
    if let Some(app) = unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pid) }
    {
        unsafe {
            app.activateWithOptions(
                NSApplicationActivationOptions::NSApplicationActivateAllWindows,
            );
        }
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
    let snapshot = {
        let mut model = VIEW_MODEL.lock().unwrap();
        let Some(model) = model.as_mut() else {
            return;
        };
        model.promote_app(target.pid).then(|| {
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
    let location: objc2_foundation::NSPoint = msg_send![event, locationInWindow];
    refresh_cursor_at_window_point(window, location);
}

unsafe fn refresh_cursor_at_window_point(
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
            let front_pid = VIEW_MODEL
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|model| model.ordered_frames().last().map(|frame| frame.target.pid));
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
    // non-activating panel. AppKit may honor this safe best-effort cursor set;
    // if it does not, the ordinary arrow remains visible.
    let _: () = msg_send![cursor, set];
}

fn hide_custom_cursor() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let image_view = HANDLES
        .lock()
        .unwrap()
        .as_ref()
        .map(|handles| handles.cursor_image_view)
        .unwrap_or(0) as *mut AnyObject;
    unsafe {
        if !image_view.is_null() {
            let _: () = msg_send![image_view, setHidden: true];
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
                            pid,
                            window_id: frame.target.window_id,
                        })
                    }),
                    model.ordered_frames().last().map(|frame| frame.target.pid),
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
                if Some(target.pid) == gesture.front_pid {
                    activate_target_window(target);
                } else {
                    dispatch_to_main(target, promote_clicked_app_cb);
                }
            }
        }
        let location: objc2_foundation::NSPoint =
            msg_send![window, mouseLocationOutsideOfEventStream];
        refresh_cursor_at_window_point(window, location);
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
        }
        builder.register()
    })
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
    let tracking_options: u64 = 0x1 | 0x2 | 0x80 | 0x200;
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
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let (Some(back), Some(front)) = (layout.first(), layout.last()) else {
        return;
    };
    let edge = RESIZE_HIT_INSET;
    let half = edge / 2.0;
    let clamp_x = |x: f64| x.clamp(0.0, (bounds.size.width - edge).max(0.0));
    let clamp_y = |y: f64| y.clamp(0.0, (bounds.size.height - edge).max(0.0));
    add_resize_hit_view(
        canvas,
        NSRect::new(
            NSPoint::new(clamp_x(front.x - half), clamp_y(front.y - half)),
            NSSize::new(edge, edge),
        ),
        RESIZE_LEFT | RESIZE_BOTTOM,
        0,
    );
    add_resize_hit_view(
        canvas,
        NSRect::new(
            NSPoint::new(
                clamp_x(front.x + front.width - half),
                clamp_y(front.y - half),
            ),
            NSSize::new(edge, edge),
        ),
        RESIZE_RIGHT | RESIZE_BOTTOM,
        0,
    );
    add_resize_hit_view(
        canvas,
        NSRect::new(
            NSPoint::new(clamp_x(back.x - half), clamp_y(back.y + back.height - half)),
            NSSize::new(edge, edge),
        ),
        RESIZE_LEFT | RESIZE_TOP,
        0,
    );
    add_resize_hit_view(
        canvas,
        NSRect::new(
            NSPoint::new(
                clamp_x(back.x + back.width - half),
                clamp_y(back.y + back.height - half),
            ),
            NSSize::new(edge, edge),
        ),
        RESIZE_RIGHT | RESIZE_TOP,
        0,
    );
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

    let options: u64 = 0x1 | 0x2 | 0x80 | 0x200;
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
        .insert(card as usize, frame.target.pid);
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
        .insert(drag_surface as usize, frame.target.pid);
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
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

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
    let layout = card_layout(bounds.size.width, bounds.size.height, snapshot.len());
    for (frame, rect) in snapshot.iter().zip(layout.iter().copied()) {
        let handles = render_card(canvas, delegate, frame, rect);
        CARD_HANDLES
            .lock()
            .unwrap()
            .insert(frame.target.pid, handles);
    }
    install_resize_hit_views(canvas, bounds, &layout);
    let mouse_location: objc2_foundation::NSPoint =
        msg_send![window, mouseLocationOutsideOfEventStream];
    refresh_cursor_at_window_point(window, mouse_location);

    let hovered_pid = HOVERED_APP.lock().unwrap().take();
    set_hovered_app(hovered_pid);

    if snapshot.is_empty() {
        hide_custom_cursor();
        let _: () = msg_send![window, orderOut: std::ptr::null_mut::<AnyObject>()];
    } else {
        let _: () = msg_send![window, orderFrontRegardless];
    }
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
    let style_mask: u64 = (1 << 7) | (1 << 3);
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
    // Match Codex's ordinary window level instead of pinning the preview above
    // every application. The opt-in override is only for local UI demos where
    // the headless daemon has no foreground app capable of owning the panel.
    let window_level = if std::env::var_os("CUA_PIP_DEMO_FLOATING").is_some() {
        3i64
    } else {
        0i64
    };
    let _: () = msg_send![window, setLevel: window_level];
    let _: () = msg_send![window, setCollectionBehavior: 0x108u64];
    let _: () = msg_send![window, setReleasedWhenClosed: false];
    let _: () = msg_send![window, setHidesOnDeactivate: false];
    let _: () = msg_send![window, setMinSize: minimum];

    let cursor_image_view: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![pip_cursor_image_view_class(), alloc];
        msg_send![allocated, initWithFrame: NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(1.0, 1.0),
        )]
    };

    let content_view: *mut AnyObject = msg_send![window, contentView];
    let _: () = msg_send![content_view, setWantsLayer: true];
    let content_layer: *mut AnyObject = msg_send![content_view, layer];
    set_layer_background(content_layer, color(0.0, 0.0, 0.0, 0.0));

    let bounds: NSRect = msg_send![content_view, bounds];
    let canvas: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSView), alloc];
        msg_send![allocated, initWithFrame: bounds]
    };
    let _: () = msg_send![canvas, setAutoresizingMask: 18u64];
    let _: () = msg_send![content_view, addSubview: canvas];
    let _: () = msg_send![cursor_image_view, setHidden: true];
    let _: () = msg_send![
        content_view,
        addSubview: cursor_image_view
        positioned: 1i64
        relativeTo: std::ptr::null_mut::<AnyObject>()
    ];

    let delegate = pip_delegate_instance();
    let _: () = msg_send![window, setDelegate: delegate];
    *HANDLES.lock().unwrap() = Some(NativeHandles {
        window: window as usize,
        canvas: canvas as usize,
        delegate: delegate as usize,
        cursor_image_view: cursor_image_view as usize,
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
    CARD_HANDLES.lock().unwrap().clear();
    CARD_VIEW_PIDS.lock().unwrap().clear();
    RESIZE_VIEW_DIRECTIONS.lock().unwrap().clear();
    VIEW_MODEL.lock().unwrap().take();
    HIDDEN_APPS.lock().unwrap().clear();
    HOVERED_APP.lock().unwrap().take();
    CARD_GESTURE.lock().unwrap().take();
    if let Some(handles) = HANDLES.lock().unwrap().take() {
        let window = handles.window as *mut AnyObject;
        let _: () = msg_send![window, setDelegate: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![window, orderOut: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![window, close];
        let _ = handles.delegate;
    }
}

/// Park the main thread in `NSApplication.run()` so AppKit can service the
/// asynchronously created stack and live-frame callbacks.
pub fn run_appkit_main_loop() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let _mtm = objc2_foundation::MainThreadMarker::new()
        .expect("run_appkit_main_loop must be called from the main thread");
    unsafe {
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        let _: bool = msg_send![app, setActivationPolicy: 1i64];
        let _: () = msg_send![app, finishLaunching];
        let _: () = msg_send![app, run];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_dimensions_preserve_aspect_ratio_and_bound_size() {
        assert_eq!(capture_dimensions(640.0, 400.0), (640, 400));
        assert_eq!(capture_dimensions(4000.0, 2000.0), (960, 480));
    }

    #[test]
    fn polling_watchdog_only_takes_over_after_stream_stalls() {
        assert!(!stream_frame_is_fresh(10_000, 0));
        assert!(stream_frame_is_fresh(10_000, 9_500));
        assert!(!stream_frame_is_fresh(10_000, 9_000));
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
