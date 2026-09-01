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
    PipBackend, PipBackendFactory, PipConfig, PipFrame, PipViewModel, MAX_VISIBLE_PIP_CARDS,
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
}

#[derive(Clone, Copy)]
struct NativeCardHandles {
    image_view: usize,
    controls: usize,
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
static LIVE_STREAMS: LazyLock<Mutex<HashMap<i64, LiveStreamEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const LIVE_CAPTURE_FPS: i32 = 12;
const LIVE_CAPTURE_MAX_SIDE: f64 = 960.0;
const LIVE_CAPTURE_WINDOW_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const LIVE_CAPTURE_WATCHDOG: Duration = Duration::from_millis(750);
const FALLBACK_CAPTURE_INTERVAL: Duration = Duration::from_millis(500);
const CARD_GAP: f64 = 8.0;
const STACK_INSET: f64 = 8.0;
const CARD_RADIUS: f64 = 12.0;
const TITLE_HEIGHT: f64 = 27.0;
const CONTROL_SIZE: f64 = 24.0;
const RESIZE_HIT_INSET: f64 = 7.0;

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
        .with_shows_cursor(true);

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

fn set_card_controls_hidden(pid: i64, hidden: bool) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let controls = CARD_HANDLES
        .lock()
        .unwrap()
        .get(&pid)
        .map(|handles| handles.controls)
        .unwrap_or(0) as *mut AnyObject;
    if !controls.is_null() {
        unsafe {
            let _: () = msg_send![controls, setHidden: hidden];
        }
    }
}

extern "C" fn card_mouse_entered(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    _event: *mut objc2::runtime::AnyObject,
) {
    if view.is_null() {
        return;
    }
    let pid = CARD_VIEW_PIDS
        .lock()
        .unwrap()
        .get(&(view as usize))
        .copied();
    if let Some(pid) = pid {
        set_card_controls_hidden(pid, false);
    }
}

extern "C" fn card_mouse_exited(
    view: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    _event: *mut objc2::runtime::AnyObject,
) {
    if view.is_null() {
        return;
    }
    let pid = CARD_VIEW_PIDS
        .lock()
        .unwrap()
        .get(&(view as usize))
        .copied();
    if let Some(pid) = pid {
        set_card_controls_hidden(pid, true);
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
        let window: *mut AnyObject = msg_send![view, window];
        if !window.is_null() {
            let _: () = msg_send![window, performWindowDragWithEvent: event];
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
                objc2::sel!(hideAppPreview:),
                hide_app_preview as extern "C" fn(_, _, _),
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

extern "C" fn hide_app_preview(
    _delegate: *mut objc2::runtime::AnyObject,
    _selector: objc2::runtime::Sel,
    sender: *mut objc2::runtime::AnyObject,
) {
    if sender.is_null() {
        return;
    }
    let pid: isize = unsafe { objc2::msg_send![sender, tag] };
    let pid = pid as i64;
    HIDDEN_APPS.lock().unwrap().insert(pid);
    stop_live_capture_for(pid);
    let snapshot = {
        let mut model = VIEW_MODEL.lock().unwrap();
        let Some(model) = model.as_mut() else {
            return;
        };
        model.remove_app(pid);
        model
            .ordered_frames()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
    };
    unsafe { render_snapshot(&snapshot) };
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
        }
        builder.register()
    })
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

    let allocated: *mut AnyObject = msg_send![resize_hit_view_class(), alloc];
    let view: *mut AnyObject = msg_send![allocated, initWithFrame: frame];
    RESIZE_VIEW_DIRECTIONS
        .lock()
        .unwrap()
        .insert(view as usize, direction);
    let _: () = msg_send![view, setAutoresizingMask: autoresizing_mask];
    let _: () = msg_send![parent, addSubview: view];
}

unsafe fn install_resize_hit_views(
    content_view: *mut objc2::runtime::AnyObject,
    bounds: objc2_foundation::NSRect,
) {
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let edge = RESIZE_HIT_INSET;
    let width = bounds.size.width;
    let height = bounds.size.height;
    add_resize_hit_view(
        content_view,
        NSRect::new(
            NSPoint::new(edge, height - edge),
            NSSize::new(width - 2.0 * edge, edge),
        ),
        RESIZE_TOP,
        10,
    );
    add_resize_hit_view(
        content_view,
        NSRect::new(
            NSPoint::new(edge, 0.0),
            NSSize::new(width - 2.0 * edge, edge),
        ),
        RESIZE_BOTTOM,
        10,
    );
    add_resize_hit_view(
        content_view,
        NSRect::new(
            NSPoint::new(0.0, edge),
            NSSize::new(edge, height - 2.0 * edge),
        ),
        RESIZE_LEFT,
        20,
    );
    add_resize_hit_view(
        content_view,
        NSRect::new(
            NSPoint::new(width - edge, edge),
            NSSize::new(edge, height - 2.0 * edge),
        ),
        RESIZE_RIGHT,
        17,
    );
    for (origin, direction, mask) in [
        (NSPoint::new(0.0, 0.0), RESIZE_LEFT | RESIZE_BOTTOM, 4),
        (
            NSPoint::new(width - edge, 0.0),
            RESIZE_RIGHT | RESIZE_BOTTOM,
            1,
        ),
        (
            NSPoint::new(0.0, height - edge),
            RESIZE_LEFT | RESIZE_TOP,
            8,
        ),
        (
            NSPoint::new(width - edge, height - edge),
            RESIZE_RIGHT | RESIZE_TOP,
            2,
        ),
    ] {
        add_resize_hit_view(
            content_view,
            NSRect::new(origin, NSSize::new(edge, edge)),
            direction,
            mask,
        );
    }
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
    let columns = match count {
        1 => 1,
        2 | 3 | 4 => 2,
        _ => 3,
    };
    let rows = count.div_ceil(columns);
    let usable_width = (width - 2.0 * STACK_INSET - CARD_GAP * (columns - 1) as f64).max(1.0);
    let usable_height = (height - 2.0 * STACK_INSET - CARD_GAP * (rows - 1) as f64).max(1.0);
    let card_width = usable_width / columns as f64;
    let card_height = usable_height / rows as f64;

    (0..count)
        .map(|index| {
            let column = index % columns;
            let row_from_top = index / columns;
            CardRect {
                x: STACK_INSET + column as f64 * (card_width + CARD_GAP),
                y: height
                    - STACK_INSET
                    - (row_from_top + 1) as f64 * card_height
                    - row_from_top as f64 * CARD_GAP,
                width: card_width,
                height: card_height,
            }
        })
        .collect()
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
    let _: () = msg_send![card_layer, setCornerRadius: CARD_RADIUS];
    let _: () = msg_send![card_layer, setMasksToBounds: true];
    let _: () = msg_send![card_layer, setBorderWidth: 0.75_f64];
    let border = color(1.0, 1.0, 1.0, 0.28);
    let border_cg: *mut CGColor = msg_send![border, CGColor];
    let _: () = msg_send![card_layer, setBorderColor: border_cg];
    set_layer_background(card_layer, color(0.02, 0.025, 0.035, 0.72));

    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), card_frame.size);
    let glass: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSVisualEffectView), alloc];
        msg_send![allocated, initWithFrame: bounds]
    };
    let _: () = msg_send![glass, setAutoresizingMask: 18u64];
    let _: () = msg_send![glass, setMaterial: 13i64];
    let _: () = msg_send![glass, setBlendingMode: 1i64];
    let _: () = msg_send![glass, setState: 1i64];
    let _: () = msg_send![glass, setAppearance: vibrant_dark_appearance()];
    let _: () = msg_send![card, addSubview: glass];

    let image_view: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSImageView), alloc];
        msg_send![allocated, initWithFrame: bounds]
    };
    let _: () = msg_send![image_view, setAutoresizingMask: 18u64];
    let _: () = msg_send![image_view, setImageScaling: 3u64];
    let image = image_from_png(&frame.png_bytes);
    if !image.is_null() {
        let _: () = msg_send![image_view, setImage: image];
        let _: () = msg_send![image, release];
    }
    let _: () = msg_send![card, addSubview: image_view];

    let title_width = (rect.width - CONTROL_SIZE - 24.0).max(40.0);
    let title_glass_frame = NSRect::new(
        NSPoint::new(7.0, rect.height - TITLE_HEIGHT - 7.0),
        NSSize::new(title_width, TITLE_HEIGHT),
    );
    let title_glass: *mut AnyObject = {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSVisualEffectView), alloc];
        msg_send![allocated, initWithFrame: title_glass_frame]
    };
    let _: () = msg_send![title_glass, setMaterial: 13i64];
    let _: () = msg_send![title_glass, setBlendingMode: 0i64];
    let _: () = msg_send![title_glass, setState: 1i64];
    let _: () = msg_send![title_glass, setAppearance: vibrant_dark_appearance()];
    let _: () = msg_send![title_glass, setWantsLayer: true];
    let title_layer: *mut AnyObject = msg_send![title_glass, layer];
    let _: () = msg_send![title_layer, setCornerRadius: TITLE_HEIGHT / 2.0];
    let _: () = msg_send![title_layer, setMasksToBounds: true];

    let title = ns_string(&frame.target.app_name);
    let label: *mut AnyObject = msg_send![objc2::class!(NSTextField), labelWithString: title];
    let _: () = msg_send![label, setFrame: NSRect::new(
        NSPoint::new(10.0, 4.0),
        NSSize::new((title_width - 20.0).max(20.0), TITLE_HEIGHT - 8.0),
    )];
    let font: *mut AnyObject =
        msg_send![objc2::class!(NSFont), systemFontOfSize: 11.0_f64 weight: 0.35_f64];
    let _: () = msg_send![label, setFont: font];
    let _: () = msg_send![label, setTextColor: color(1.0, 1.0, 1.0, 0.94)];
    let _: () = msg_send![label, setLineBreakMode: 4u64];
    let _: () = msg_send![title_glass, addSubview: label];
    let _: () = msg_send![card, addSubview: title_glass];

    // A transparent surface above the preview makes the whole card draggable.
    // The hover control is added after it and therefore remains clickable.
    let drag_allocated: *mut AnyObject = msg_send![pip_card_view_class(), alloc];
    let drag_surface: *mut AnyObject = msg_send![drag_allocated, initWithFrame: bounds];
    let _: () = msg_send![drag_surface, setAutoresizingMask: 18u64];
    let _: () = msg_send![card, addSubview: drag_surface];

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
    let _: () = msg_send![button, setTitle: ns_string("×")];
    let button_font: *mut AnyObject =
        msg_send![objc2::class!(NSFont), systemFontOfSize: 17.0_f64 weight: 0.2_f64];
    let _: () = msg_send![button, setFont: button_font];
    let _: () = msg_send![button, setContentTintColor: color(1.0, 1.0, 1.0, 0.96)];
    let _: () = msg_send![button, setToolTip: ns_string("Hide this app preview")];
    let _: () = msg_send![button, setTag: frame.target.pid as isize];
    let _: () = msg_send![button, setTarget: delegate];
    let _: () = msg_send![button, setAction: objc2::sel!(hideAppPreview:)];
    let _: () = msg_send![controls, addSubview: button];
    let _: () = msg_send![controls, setHidden: true];
    let _: () = msg_send![card, addSubview: controls];

    install_tracking_area(card);
    let _: () = msg_send![canvas, addSubview: card];
    NativeCardHandles {
        image_view: image_view as usize,
        controls: controls as usize,
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
    let empty: *mut AnyObject = msg_send![objc2::class!(NSArray), array];
    let _: () = msg_send![canvas, setSubviews: empty];
    let bounds: objc2_foundation::NSRect = msg_send![canvas, bounds];
    let layout = card_layout(bounds.size.width, bounds.size.height, snapshot.len());
    for (frame, rect) in snapshot.iter().zip(layout) {
        let handles = render_card(canvas, delegate, frame, rect);
        CARD_HANDLES
            .lock()
            .unwrap()
            .insert(frame.target.pid, handles);
    }

    if snapshot.is_empty() {
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
    let minimum = NSSize::new(320.0, 200.0);
    let width = (cfg.geometry.width as f64).max(minimum.width);
    let height = (cfg.geometry.height as f64).max(minimum.height);
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
    let _: () = msg_send![window, setHasShadow: true];
    let _: () = msg_send![window, setIgnoresMouseEvents: false];
    let _: () = msg_send![window, setAcceptsMouseMovedEvents: true];
    let _: () = msg_send![window, setBecomesKeyOnlyIfNeeded: true];
    let _: () = msg_send![window, setMovableByWindowBackground: true];
    let _: () = msg_send![window, setFloatingPanel: false];
    // Match Codex's ordinary window level instead of pinning the preview above
    // every application. 0x108 = transient + full-screen auxiliary.
    let _: () = msg_send![window, setLevel: 0i64];
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
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSView), alloc];
        msg_send![allocated, initWithFrame: bounds]
    };
    let _: () = msg_send![canvas, setAutoresizingMask: 18u64];
    let _: () = msg_send![content_view, addSubview: canvas];
    install_resize_hit_views(content_view, bounds);

    let delegate = pip_delegate_instance();
    let _: () = msg_send![window, setDelegate: delegate];
    *HANDLES.lock().unwrap() = Some(NativeHandles {
        window: window as usize,
        canvas: canvas as usize,
        delegate: delegate as usize,
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

    CARD_HANDLES.lock().unwrap().clear();
    CARD_VIEW_PIDS.lock().unwrap().clear();
    RESIZE_VIEW_DIRECTIONS.lock().unwrap().clear();
    VIEW_MODEL.lock().unwrap().take();
    HIDDEN_APPS.lock().unwrap().clear();
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
    fn grid_gives_every_app_its_own_non_overlapping_card() {
        let cards = card_layout(620.0, 420.0, 5);
        assert_eq!(cards.len(), 5);
        for (index, left) in cards.iter().enumerate() {
            assert!(left.width > 0.0 && left.height > 0.0);
            for right in cards.iter().skip(index + 1) {
                let overlaps = left.x < right.x + right.width
                    && left.x + left.width > right.x
                    && left.y < right.y + right.height
                    && left.y + left.height > right.y;
                assert!(!overlaps, "cards {left:?} and {right:?} overlap");
            }
        }
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
}
