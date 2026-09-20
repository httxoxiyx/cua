//! SkyLight SPI bridge — Rust port of Swift's `SkyLightEventPost`.
//!
//! Two-layer story (matches Swift reference exactly):
//!
//! 1. **Post path** — `SLEventPostToPid` goes through `SLEventPostToPSN` →
//!    `CGSTickleActivityMonitor` → `SLSUpdateSystemActivityWithLocation` →
//!    `IOHIDPostEvent`. The public `CGEventPostToPid` skips the activity-monitor
//!    tickle so Chromium/Catalyst targets don't accept those events as live input.
//!
//! 2. **Authentication** (keyboard only) — on macOS 14+, WindowServer gates
//!    synthetic keyboard events on Chromium-like targets on an attached
//!    `SLSEventAuthenticationMessage`. We build one via the ObjC factory and
//!    attach it with `SLEventSetAuthenticationMessage` before posting.
//!
//! All symbols are resolved once at first use via `dlopen` + `dlsym`.
//! If anything fails to resolve the functions return `false` and callers
//! fall back to the public `CGEvent::post_to_pid`.

use libc::pid_t;
use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::OnceLock;

// ── Function-pointer typedefs ──────────────────────────────────────────────

/// `void SLEventPostToPid(pid_t, CGEventRef)`
type PostToPidFn = unsafe extern "C" fn(pid_t, *mut c_void);

/// `void SLEventSetAuthenticationMessage(CGEventRef, id)`
type SetAuthMsgFn = unsafe extern "C" fn(*mut c_void, *mut c_void);

/// `void CGEventSetWindowLocation(CGEventRef, double x, double y)`
///
/// NOTE: CGPoint on 64-bit ARM/x86 is two f64 values packed consecutively.
/// We pass them as two separate f64 arguments which has identical ABI.
type SetWindowLocFn = unsafe extern "C" fn(*mut c_void, f64, f64);

/// `void SLEventSetIntegerValueField(CGEventRef, uint32_t field, int64_t value)`
type SetIntFieldFn = unsafe extern "C" fn(*mut c_void, u32, i64);

/// `uint32_t CGSMainConnectionID(void)`
type ConnectionIDFn = unsafe extern "C" fn() -> u32;

/// `CGError CGSSetConnectionProperty(CGSConnectionID, CGSConnectionID, CFStringRef, CFTypeRef)`
type SetConnectionPropertyFn = unsafe extern "C" fn(u32, u32, *const c_void, *const c_void) -> i32;

/// `uint64_t CGSGetActiveSpace(uint32_t cid)`
type GetActiveSpaceFn = unsafe extern "C" fn(u32) -> u64;

/// `CFArrayRef SLSCopySpacesForWindows(uint32_t cid, int selector, CFArrayRef windowIDs)`
type CopySpacesForWindowsFn = unsafe extern "C" fn(u32, i32, *const c_void) -> *mut c_void;

/// `CFStringRef SLSCopyManagedDisplayForWindow(uint32_t cid, uint32_t wid)`
type CopyManagedDisplayForWindowFn = unsafe extern "C" fn(u32, u32) -> *mut c_void;

/// `uint64_t SLSManagedDisplayGetCurrentSpace(uint32_t cid, CFStringRef display)`
type ManagedDisplayGetCurrentSpaceFn = unsafe extern "C" fn(u32, *const c_void) -> u64;

// ── NSMenu shortcut activation SPIs ──────────────────────────────────────────

/// `OSStatus SLPSSetFrontProcessWithOptions(const void *psn, uint32_t windowID, uint32_t options)`
type SetFrontProcessFn = unsafe extern "C" fn(*const c_void, u32, u32) -> i32;

/// `OSStatus SLSGetWindowOwner(uint32_t cid, uint32_t wid, uint32_t *out_cid)`
type GetWindowOwnerFn = unsafe extern "C" fn(u32, u32, *mut u32) -> i32;

/// `OSStatus SLSGetConnectionPSN(uint32_t cid, void *psn)`
type GetConnectionPSNFn = unsafe extern "C" fn(u32, *mut c_void) -> i32;

// ── Focus-without-raise SPIs ──────────────────────────────────────────────────

/// `OSStatus SLPSPostEventRecordTo(const void *psn, const uint8_t *bytes)`
/// Posts a 248-byte synthetic event record into the target process's Carbon
/// event queue. Build the buffer with bytes[0x04]=0xf8, bytes[0x08]=0x0d,
/// target window id at bytes 0x3c–0x3f (little-endian), focus/defocus marker
/// at bytes[0x8a] (0x01 = focus, 0x02 = defocus), all other bytes zero.
type PostEventRecordToFn = unsafe extern "C" fn(*const c_void, *const u8) -> i32;

/// `OSStatus _SLPSGetFrontProcess(void *psn)`
/// Writes the current frontmost process's 8-byte PSN into `psn`.
type GetFrontProcessFn = unsafe extern "C" fn(*mut c_void) -> i32;

/// `OSStatus GetProcessForPID(pid_t, void *psn)`
/// Deprecated but still resolves. Writes the target pid's 8-byte PSN.
type GetProcessForPIDFn = unsafe extern "C" fn(pid_t, *mut c_void) -> i32;

/// Factory: `+[SLSEventAuthenticationMessage messageWithEventRecord:pid:version:]`
/// ObjC send: `(id self, SEL _cmd, void* record, int32 pid, uint32 version) -> id`
type FactoryMsgSendFn = unsafe extern "C" fn(
    *mut c_void, // Class (receiver)
    *mut c_void, // SEL
    *mut c_void, // SLSEventRecord*
    c_int,       // pid
    c_uint,      // version
) -> *mut c_void;

// ── Symbol resolution ──────────────────────────────────────────────────────

/// Load SkyLight once so all dlsym lookups via RTLD_DEFAULT find it.
fn ensure_skylight_loaded() {
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        let path = b"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight\0";
        unsafe {
            libc::dlopen(
                path.as_ptr() as *const c_char,
                libc::RTLD_LAZY | libc::RTLD_GLOBAL,
            );
        }
    });
}

/// Look up a symbol by name via RTLD_DEFAULT (after loading SkyLight).
/// Returns `None` when the symbol doesn't resolve.
fn find_sym(name: &[u8]) -> Option<*mut c_void> {
    ensure_skylight_loaded();
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const c_char) };
    if ptr.is_null() {
        None
    } else {
        Some(ptr)
    }
}

/// Reinterpret a raw symbol pointer as a function pointer of type `T`.
/// Safety: caller guarantees T matches the symbol's actual signature.
unsafe fn as_fn<T: Copy>(ptr: *mut c_void) -> T {
    std::mem::transmute_copy::<*mut c_void, T>(&ptr)
}

// ── Lazily-resolved handles ────────────────────────────────────────────────

fn post_to_pid_fn() -> Option<PostToPidFn> {
    static SYM: OnceLock<Option<PostToPidFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLEventPostToPid\0").map(|p| unsafe { as_fn(p) }))
}

fn set_auth_msg_fn() -> Option<SetAuthMsgFn> {
    static SYM: OnceLock<Option<SetAuthMsgFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLEventSetAuthenticationMessage\0").map(|p| unsafe { as_fn(p) }))
}

fn set_window_loc_fn() -> Option<SetWindowLocFn> {
    static SYM: OnceLock<Option<SetWindowLocFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"CGEventSetWindowLocation\0").map(|p| unsafe { as_fn(p) }))
}

fn set_int_field_fn() -> Option<SetIntFieldFn> {
    static SYM: OnceLock<Option<SetIntFieldFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLEventSetIntegerValueField\0").map(|p| unsafe { as_fn(p) }))
}

fn connection_id_fn() -> Option<ConnectionIDFn> {
    static SYM: OnceLock<Option<ConnectionIDFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"CGSMainConnectionID\0").map(|p| unsafe { as_fn(p) }))
}

fn set_connection_property_fn() -> Option<SetConnectionPropertyFn> {
    static SYM: OnceLock<Option<SetConnectionPropertyFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSSetConnectionProperty\0")
            .or_else(|| find_sym(b"CGSSetConnectionProperty\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

/// Allow this process's AppKit cursor requests to win while its nonactivating
/// PiP panel is above a foreground application.
pub(crate) fn enable_background_cursor_updates(
    property: *const c_void,
    value: *const c_void,
) -> bool {
    let (Some(connection), Some(set_property)) = (connection_id_fn(), set_connection_property_fn())
    else {
        return false;
    };
    let connection = unsafe { connection() };
    connection != 0 && unsafe { set_property(connection, connection, property, value) } == 0
}

fn get_active_space_fn() -> Option<GetActiveSpaceFn> {
    static SYM: OnceLock<Option<GetActiveSpaceFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSGetActiveSpace\0")
            .or_else(|| find_sym(b"CGSGetActiveSpace\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

fn copy_spaces_for_windows_fn() -> Option<CopySpacesForWindowsFn> {
    static SYM: OnceLock<Option<CopySpacesForWindowsFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSCopySpacesForWindows\0")
            .or_else(|| find_sym(b"CGSCopySpacesForWindows\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

fn copy_managed_display_for_window_fn() -> Option<CopyManagedDisplayForWindowFn> {
    static SYM: OnceLock<Option<CopyManagedDisplayForWindowFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSCopyManagedDisplayForWindow\0")
            .or_else(|| find_sym(b"CGSCopyManagedDisplayForWindow\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

fn managed_display_get_current_space_fn() -> Option<ManagedDisplayGetCurrentSpaceFn> {
    static SYM: OnceLock<Option<ManagedDisplayGetCurrentSpaceFn>> = OnceLock::new();
    *SYM.get_or_init(|| {
        find_sym(b"SLSManagedDisplayGetCurrentSpace\0")
            .or_else(|| find_sym(b"CGSManagedDisplayGetCurrentSpace\0"))
            .map(|p| unsafe { as_fn(p) })
    })
}

fn factory_msg_send_fn() -> Option<FactoryMsgSendFn> {
    static SYM: OnceLock<Option<FactoryMsgSendFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"objc_msgSend\0").map(|p| unsafe { as_fn(p) }))
}

fn set_front_process_fn() -> Option<SetFrontProcessFn> {
    static SYM: OnceLock<Option<SetFrontProcessFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLPSSetFrontProcessWithOptions\0").map(|p| unsafe { as_fn(p) }))
}

fn get_window_owner_fn() -> Option<GetWindowOwnerFn> {
    static SYM: OnceLock<Option<GetWindowOwnerFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLSGetWindowOwner\0").map(|p| unsafe { as_fn(p) }))
}

fn get_connection_psn_fn() -> Option<GetConnectionPSNFn> {
    static SYM: OnceLock<Option<GetConnectionPSNFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLSGetConnectionPSN\0").map(|p| unsafe { as_fn(p) }))
}

fn post_event_record_to_fn() -> Option<PostEventRecordToFn> {
    static SYM: OnceLock<Option<PostEventRecordToFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"SLPSPostEventRecordTo\0").map(|p| unsafe { as_fn(p) }))
}

fn get_front_process_fn() -> Option<GetFrontProcessFn> {
    static SYM: OnceLock<Option<GetFrontProcessFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"_SLPSGetFrontProcess\0").map(|p| unsafe { as_fn(p) }))
}

fn get_process_for_pid_fn() -> Option<GetProcessForPIDFn> {
    static SYM: OnceLock<Option<GetProcessForPIDFn>> = OnceLock::new();
    *SYM.get_or_init(|| find_sym(b"GetProcessForPID\0").map(|p| unsafe { as_fn(p) }))
}

/// `true` when `SLEventPostToPid` resolved.
pub fn is_available() -> bool {
    post_to_pid_fn().is_some()
}

/// `true` when the target-only synthetic-focus SPIs resolved, including either
/// the modern window-owner PSN lookup or the deprecated pid fallback.
///
/// `_SLPSGetFrontProcess` is observation-only here: teardown reads it so a user
/// who genuinely activates the target during an in-flight click keeps control.
/// The background action never addresses, defocuses, or later re-focuses the
/// user's real foreground process.
pub fn is_synthetic_target_focus_available() -> bool {
    let has_psn_lookup = (connection_id_fn().is_some()
        && get_window_owner_fn().is_some()
        && get_connection_psn_fn().is_some())
        || get_process_for_pid_fn().is_some();
    get_front_process_fn().is_some() && has_psn_lookup && post_event_record_to_fn().is_some()
}

// ── ObjC runtime helpers ───────────────────────────────────────────────────

/// Look up an ObjC class by C-string name via `objc_getClass`.
fn objc_class(name: &CStr) -> *mut c_void {
    type GetClassFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    static SYM: OnceLock<Option<GetClassFn>> = OnceLock::new();
    let f = *SYM.get_or_init(|| find_sym(b"objc_getClass\0").map(|p| unsafe { as_fn(p) }));
    match f {
        Some(f) => unsafe { f(name.as_ptr()) },
        None => std::ptr::null_mut(),
    }
}

/// Register / look up an ObjC selector by C-string name via `sel_registerName`.
fn sel_register(name: &CStr) -> *mut c_void {
    type SelRegFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
    static SYM: OnceLock<Option<SelRegFn>> = OnceLock::new();
    let f = *SYM.get_or_init(|| find_sym(b"sel_registerName\0").map(|p| unsafe { as_fn(p) }));
    match f {
        Some(f) => unsafe { f(name.as_ptr()) },
        None => std::ptr::null_mut(),
    }
}

/// Whether `cls` implements the CLASS method `sel`, via `class_getClassMethod`.
/// `class_respondsToSelector(cls, sel)` tests methods of instances of `cls`,
/// so it incorrectly rejects a factory implemented only on the metaclass.
///
/// macOS 14 (Sonoma) compatibility guard: `SLSEventAuthenticationMessage`
/// exists on macOS 14, but `messageWithEventRecord:pid:version:` was only
/// added in macOS 15 (Sequoia). `sel_registerName` always succeeds (it just
/// interns the string), so a `!sel.is_null()` check is not enough — we must
/// confirm that class method exists before calling `objc_msgSend`, or the runtime
/// raises `NSInvalidArgumentException: unrecognized selector`. See #1503.
fn class_has_class_method(cls: *mut c_void, sel: *mut c_void) -> bool {
    if cls.is_null() || sel.is_null() {
        return false;
    }
    type GetClassMethodFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    static SYM: OnceLock<Option<GetClassMethodFn>> = OnceLock::new();
    let f = *SYM.get_or_init(|| find_sym(b"class_getClassMethod\0").map(|p| unsafe { as_fn(p) }));
    match f {
        Some(f) => !unsafe { f(cls, sel) }.is_null(),
        None => false,
    }
}

// ── SLSEventRecord extraction ──────────────────────────────────────────────

/// Extract the embedded `SLSEventRecord *` from a `CGEvent`.
///
/// Layout of `__CGEvent` (SkyLight ObjC type encodings):
///   `{CFRuntimeBase, uint32_t, SLSEventRecord *}`
/// On 64-bit: CFRuntimeBase=16, uint32=4, 4 bytes pad → record pointer at offset 24.
/// We probe offsets 24, 32, 16 for resilience across OS versions (same as Swift).
unsafe fn extract_event_record(event_ptr: *mut c_void) -> *mut c_void {
    for &offset in &[24usize, 32, 16] {
        let slot = (event_ptr as *const u8).add(offset).cast::<*mut c_void>();
        let p = std::ptr::read_unaligned(slot);
        if !p.is_null() {
            return p;
        }
    }
    std::ptr::null_mut()
}

// ── Public entry points ────────────────────────────────────────────────────

/// Post `event_ptr` (raw `CGEventRef`) to `pid` via `SLEventPostToPid`.
///
/// `attach_auth_message`: pass `true` for keyboard events (Chromium path),
/// `false` for mouse events (see Swift doc comment on `postToPid`).
///
/// Returns `true` when `SLEventPostToPid` resolved and the post was attempted.
/// Returns `false` when the SPI is absent — caller falls back to `CGEvent::post_to_pid`.
pub(super) fn post_to_pid(pid: pid_t, event_ptr: *mut c_void, attach_auth_message: bool) -> bool {
    let post_fn = match post_to_pid_fn() {
        Some(f) => f,
        None => return false,
    };

    if attach_auth_message {
        // Build and attach SLSEventAuthenticationMessage.
        //
        // macOS 14 (Sonoma) compatibility: the class exists on macOS 14 but
        // `messageWithEventRecord:pid:version:` was added in macOS 15. Guard
        // with `class_getClassMethod` (a `!sel.is_null()` check is not
        // enough — `sel_registerName` interns any name); when the selector is
        // absent we skip the auth envelope and fall through to the plain
        // `SLEventPostToPid` below. Chromium-class targets may not receive the
        // event on macOS 14, but the daemon no longer crashes. See #1503.
        let cls = objc_class(c"SLSEventAuthenticationMessage");
        let sel = sel_register(c"messageWithEventRecord:pid:version:");
        let factory = factory_msg_send_fn();

        if class_has_class_method(cls, sel) {
            if let Some(factory_fn) = factory {
                let record = unsafe { extract_event_record(event_ptr) };
                if !record.is_null() {
                    let msg = unsafe { factory_fn(cls, sel, record, pid as c_int, 0u32) };
                    if !msg.is_null() {
                        if let Some(set_auth) = set_auth_msg_fn() {
                            unsafe { set_auth(event_ptr, msg) };
                        }
                    }
                }
            }
        }
    }

    unsafe { post_fn(pid, event_ptr) };
    true
}

/// One prebuilt authenticated PID post for the experimental exact-background
/// route. Unlike the legacy transport, unavailable authentication is a refusal
/// before dispatch, never a fallback to another posting mechanism.
pub(super) struct AuthenticatedPidPost {
    pid: pid_t,
    event: core_graphics::event::CGEvent,
    post: PostToPidFn,
}

impl AuthenticatedPidPost {
    pub(super) fn post(self) {
        use foreign_types::ForeignType;
        unsafe { (self.post)(self.pid, self.event.as_ptr().cast()) };
    }
}

/// The window-tagged Return experiment may select one final PID transport.
/// This does not select an event format, relax admission or permit a retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WindowReturnPostRoute {
    SkyLight,
    PublicCg,
}

impl WindowReturnPostRoute {
    fn from_flag(value: Option<&std::ffi::OsStr>) -> Self {
        if value == Some(std::ffi::OsStr::new("1")) {
            Self::PublicCg
        } else {
            Self::SkyLight
        }
    }

    pub(super) fn trace_stage(self) -> &'static str {
        match self {
            Self::SkyLight => "post.route.skylight",
            Self::PublicCg => "post.route.public_cg",
        }
    }
}

pub(super) fn window_return_post_route() -> WindowReturnPostRoute {
    WindowReturnPostRoute::from_flag(
        std::env::var_os("CUA_EXPERIMENTAL_CHROME_RETURN_PUBLIC_POST").as_deref(),
    )
}

/// Select only one function. In particular, the public route never resolves
/// the private posting symbol and a missing private symbol never falls back.
fn select_window_return_post<T>(
    route: WindowReturnPostRoute,
    public_post: T,
    resolve_skylight: impl FnOnce() -> Option<T>,
) -> Option<T> {
    match route {
        WindowReturnPostRoute::PublicCg => Some(public_post),
        WindowReturnPostRoute::SkyLight => resolve_skylight(),
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    #[link_name = "CGEventPostToPid"]
    fn public_cg_event_post_to_pid(pid: pid_t, event: core_graphics::sys::CGEventRef);
}

unsafe extern "C" fn public_cg_post(pid: pid_t, event: *mut c_void) {
    // The owning AuthenticatedPidPost keeps this exact CGEvent alive through
    // the public call. This is PID-routed, not a global HID post.
    unsafe { public_cg_event_post_to_pid(pid, event.cast()) };
}

pub(super) fn prepare_authenticated_window_return_post(
    pid: pid_t,
    event: core_graphics::event::CGEvent,
    route: WindowReturnPostRoute,
) -> anyhow::Result<AuthenticatedPidPost> {
    let post = select_window_return_post(route, public_cg_post as PostToPidFn, post_to_pid_fn)
        .ok_or_else(|| anyhow::anyhow!("authenticated PID posting is unavailable"))?;
    prepare_authenticated_pid_post_with(pid, event, post)
}

pub(super) fn prepare_authenticated_pid_post(
    pid: pid_t,
    event: core_graphics::event::CGEvent,
) -> anyhow::Result<AuthenticatedPidPost> {
    let post = post_to_pid_fn()
        .ok_or_else(|| anyhow::anyhow!("authenticated PID posting is unavailable"))?;
    prepare_authenticated_pid_post_with(pid, event, post)
}

fn prepare_authenticated_pid_post_with(
    pid: pid_t,
    event: core_graphics::event::CGEvent,
    post: PostToPidFn,
) -> anyhow::Result<AuthenticatedPidPost> {
    use foreign_types::ForeignType;
    let set_auth = set_auth_msg_fn()
        .ok_or_else(|| anyhow::anyhow!("keyboard event authentication is unavailable"))?;
    let factory = factory_msg_send_fn()
        .ok_or_else(|| anyhow::anyhow!("keyboard event authentication factory is unavailable"))?;
    let cls = objc_class(c"SLSEventAuthenticationMessage");
    let selector = sel_register(c"messageWithEventRecord:pid:version:");
    if !class_has_class_method(cls, selector) {
        anyhow::bail!("authenticated PID keyboard is unsupported on this OS");
    }
    let event_ptr = event.as_ptr().cast();
    let record = unsafe { extract_event_record(event_ptr) };
    if record.is_null() {
        anyhow::bail!("keyboard event authentication record is unavailable");
    }
    let message = unsafe { factory(cls, selector, record, pid, 0) };
    if message.is_null() {
        anyhow::bail!("keyboard event authentication could not be prepared");
    }
    unsafe { set_auth(event_ptr, message) };
    Ok(AuthenticatedPidPost { pid, event, post })
}

/// Queue construction on the actual main thread, never authentication/posting.
/// NSEvent character translation uses TIS/TSM and must not run on input workers.
pub(super) fn window_tagged_return_event(
    window_id: u32,
    down: bool,
    trace: &super::return_trace::Trace,
) -> anyhow::Result<core_graphics::event::CGEvent> {
    super::return_main_thread::construct_return_event(window_id, down, trace)
}

pub(super) fn window_tagged_return_event_on_main(
    window_id: u32,
    down: bool,
) -> anyhow::Result<core_graphics::event::CGEvent> {
    if unsafe { libc::pthread_main_np() } == 0 {
        anyhow::bail!("standard Return construction requires the actual main thread");
    }
    construct_window_tagged_return_event(window_id, down)
}

/// Construct, but never post, a directly owned standard HID-source CG Return.
/// The production caller above proves main-thread execution for this entire
/// function, including the pre-tag baseline and all NSEvent character reads.
/// Neither copying nor flattening is used: those can change source metadata.
fn construct_window_tagged_return_event(
    window_id: u32,
    down: bool,
) -> anyhow::Result<core_graphics::event::CGEvent> {
    use core_graphics::{
        event::{CGEvent, CGEventFlags, EventField},
        event_source::{CGEventSource, CGEventSourceStateID},
    };
    use foreign_types::ForeignType;
    use objc2::{class, msg_send};
    use objc2_app_kit::{NSEvent, NSEventType};
    if window_id == 0 {
        anyhow::bail!("window-tagged Return requires an exact nonzero window");
    }
    objc2::rc::autoreleasepool(|_| unsafe {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("standard Return HID source is unavailable"))?;
        let event = CGEvent::new_keyboard_event(source, 36, down)
            .map_err(|_| anyhow::anyhow!("standard Return event is unavailable"))?;
        event.set_flags(CGEventFlags::CGEventFlagNull);
        let keyboard_type = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYBOARD_TYPE);
        let source_state = event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID);
        extern "C" {
            fn CGEventKeyboardGetUnicodeString(
                event: core_graphics::sys::CGEventRef,
                max_length: usize,
                actual_length: *mut usize,
                characters: *mut u16,
            );
        }
        let mut baseline_characters = [0u16; 4];
        let mut baseline_length = 0;
        CGEventKeyboardGetUnicodeString(
            event.as_ptr(),
            baseline_characters.len(),
            &mut baseline_length,
            baseline_characters.as_mut_ptr(),
        );
        if baseline_length > 1 || (baseline_length == 1 && baseline_characters[0] != 13) {
            anyhow::bail!("standard Return has an unexpected or truncated CG Unicode baseline");
        }
        event.set_integer_value_field(51, i64::from(window_id));
        crate::foreground_activity::mark_generated(&event);
        let generated_cookie = event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA);

        // Check the owned event after all stamps and before authentication.
        // NSEvent is a readback only, never the event's source or a copy path.
        let kind = if down {
            NSEventType::KeyDown
        } else {
            NSEventType::KeyUp
        };
        let event_ptr = event.as_ptr().cast::<c_void>();
        let readback: *mut NSEvent = msg_send![class!(NSEvent), eventWithCGEvent: event_ptr];
        let readback = readback
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("standard Return cannot be read back"))?;
        let native_characters = readback.characters().map(|s| s.to_string());
        let native_ignoring = readback
            .charactersIgnoringModifiers()
            .map(|s| s.to_string());
        // A standard CG Return may have an empty optional CG Unicode payload
        // on a worker thread even when native characters are CR. Preserve the
        // same event's pre-tag baseline exactly; never write a Unicode override.
        let mut characters = [0u16; 4];
        let mut actual_length = 0;
        CGEventKeyboardGetUnicodeString(
            event.as_ptr(),
            characters.len(),
            &mut actual_length,
            characters.as_mut_ptr(),
        );
        if readback.windowNumber() != window_id as isize
            || readback.r#type() != kind
            || readback.keyCode() != 36
            || readback.isARepeat()
            || !readback.modifierFlags().is_empty()
            || native_characters.as_deref() != Some("\r")
            || native_ignoring.as_deref() != Some("\r")
            || event.get_integer_value_field(51) != i64::from(window_id)
            || event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYBOARD_TYPE)
                != keyboard_type
            || event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID) != source_state
            || event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) != generated_cookie
            || source_state != CGEventSourceStateID::HIDSystemState as i64
            || actual_length != baseline_length
            || characters[..actual_length] != baseline_characters[..baseline_length]
        {
            anyhow::bail!(
                "standard Return format unproven: native_window={}, cg_window={}, native_type={:?}, key={}, repeat={}, flags={:?}, native_characters={:?}, native_ignoring={:?}, keyboard_type={}->{}, source_state={}->{}, cookie_matches={}, unicode_length={}->{}, unicode={:?}->{:?}",
                readback.windowNumber(), event.get_integer_value_field(51), readback.r#type(),
                readback.keyCode(), readback.isARepeat(), readback.modifierFlags(),
                native_characters, native_ignoring,
                keyboard_type, event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYBOARD_TYPE),
                source_state, event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID),
                event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == generated_cookie,
                baseline_length, actual_length, baseline_characters, characters,
            );
        }
        Ok(event)
    })
}

/// Stamp a window-local `(x, y)` point onto `event_ptr` via the private
/// `CGEventSetWindowLocation` SPI. Returns `true` when the SPI resolved.
pub(super) fn set_window_location(event_ptr: *mut c_void, x: f64, y: f64) -> bool {
    match set_window_loc_fn() {
        Some(f) => {
            unsafe { f(event_ptr, x, y) };
            true
        }
        None => false,
    }
}

/// Stamp `value` onto `event_ptr` at raw SkyLight field index `field` via
/// `SLEventSetIntegerValueField`. Returns `false` when SPI absent.
pub(super) fn set_integer_field(event_ptr: *mut c_void, field: u32, value: i64) -> bool {
    match set_int_field_fn() {
        Some(f) => {
            unsafe { f(event_ptr, field, value) };
            true
        }
        None => false,
    }
}

/// Return the Skylight main connection ID for the current process.
pub fn main_connection_id() -> Option<u32> {
    connection_id_fn().map(|f| unsafe { f() })
}

/// Return the current active macOS Space (desktop) ID.
///
/// Uses the private `CGSGetActiveSpace` SPI from SkyLight.  Returns `None`
/// when the symbol is unavailable (future macOS version that removes it).
pub fn get_active_space() -> Option<u64> {
    let cid = main_connection_id()?;
    nonzero_space_id(get_active_space_fn().map(|f| unsafe { f(cid) }))
}

fn nonzero_space_id(space_id: Option<u64>) -> Option<u64> {
    space_id.filter(|id| *id != 0)
}

/// A consistent WindowServer connection and active-Space snapshot for one
/// enumeration. Space membership must be queried one window at a time:
/// `SLSCopySpacesForWindows` returns the set union for its input window list,
/// not a positionally aligned result.
pub(crate) struct SpaceQuery {
    connection_id: u32,
    current_space_id: Option<u64>,
    copy_spaces_for_windows: Option<CopySpacesForWindowsFn>,
    copy_managed_display_for_window: Option<CopyManagedDisplayForWindowFn>,
    managed_display_get_current_space: Option<ManagedDisplayGetCurrentSpaceFn>,
}

impl SpaceQuery {
    pub(crate) fn new() -> Option<Self> {
        let connection_id = main_connection_id()?;
        let current_space_id =
            nonzero_space_id(get_active_space_fn().map(|f| unsafe { f(connection_id) }));
        Some(Self {
            connection_id,
            current_space_id,
            copy_spaces_for_windows: copy_spaces_for_windows_fn(),
            copy_managed_display_for_window: copy_managed_display_for_window_fn(),
            managed_display_get_current_space: managed_display_get_current_space_fn(),
        })
    }

    pub(crate) fn current_space_id(&self) -> Option<u64> {
        self.current_space_id
    }

    /// Return every Space containing `window_id`.
    pub(crate) fn window_space_ids(&self, window_id: u32) -> Option<Vec<u64>> {
        use core_foundation::{
            array::CFArray,
            base::{CFGetTypeID, CFTypeRef, TCFType},
            number::CFNumber,
        };

        let copy_spaces = self.copy_spaces_for_windows?;
        let window_number = CFNumber::from(window_id as i64);
        let window_ref = window_number.as_concrete_TypeRef() as *const c_void;
        let windows = CFArray::<CFTypeRef>::from_copyable(&[window_ref]);
        let result_ptr = unsafe {
            copy_spaces(
                self.connection_id,
                0x7,
                windows.as_concrete_TypeRef() as *const c_void,
            )
        };
        if result_ptr.is_null() {
            return None;
        }

        let result: CFArray<CFTypeRef> =
            unsafe { CFArray::wrap_under_create_rule(result_ptr as _) };
        let mut space_ids = Vec::with_capacity(result.len() as usize);
        for item in result.iter() {
            let item = *item;
            if unsafe { CFGetTypeID(item) } != CFNumber::type_id() {
                return None;
            }
            let number = unsafe { CFNumber::wrap_under_get_rule(item as _) };
            let space_id = u64::try_from(number.to_i64()?).ok()?;
            if space_id != 0 {
                space_ids.push(space_id);
            }
        }

        (!space_ids.is_empty()).then_some(space_ids)
    }

    /// Return the active Space on the display WindowServer associates with
    /// `window_id`. This avoids comparing every window against the main
    /// display's active Space when displays use independent Spaces.
    pub(crate) fn current_space_for_window(&self, window_id: u32) -> Option<u64> {
        use core_foundation::{base::TCFType, string::CFString};

        let copy_display = self.copy_managed_display_for_window?;
        let get_current_space = self.managed_display_get_current_space?;
        let display_ptr = unsafe { copy_display(self.connection_id, window_id) };
        if display_ptr.is_null() {
            return None;
        }
        let display = unsafe { CFString::wrap_under_create_rule(display_ptr as _) };
        nonzero_space_id(Some(unsafe {
            get_current_space(
                self.connection_id,
                display.as_concrete_TypeRef() as *const c_void,
            )
        }))
    }
}

// ── Target-only synthetic focus ───────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
struct SyntheticFocusCommand {
    psn: [u8; 8],
    window_id: u32,
    focused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SyntheticTargetFocusPlan {
    activate_target: SyntheticFocusCommand,
    deactivate_target: SyntheticFocusCommand,
}

/// Opaque cleanup token for a target-only synthetic-focus session.
///
/// The token deliberately contains only the target identity. Structurally, the
/// cleanup path cannot send a focus record to whichever application happens to
/// be in the real foreground.
#[derive(Debug)]
pub struct SyntheticTargetFocusContext {
    deactivate_target: Option<SyntheticFocusCommand>,
}

impl SyntheticTargetFocusContext {
    /// Establish the exact target's app-local key window without changing
    /// WindowServer's foreground process. A synthetic app activation alone
    /// leaves standard nil-target NSMenu actions without a responder chain.
    pub(crate) fn make_window_key(&self) -> anyhow::Result<()> {
        let command = self
            .deactivate_target
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("target-only synthetic focus was already ended"))?;
        post_exact_key_window_records(command.psn, command.window_id)?;
        std::thread::sleep(std::time::Duration::from_millis(40));
        Ok(())
    }
}

impl Drop for SyntheticTargetFocusContext {
    fn drop(&mut self) {
        // Error/cancellation safety: never leave the target believing it is
        // active merely because event construction or task joining failed.
        // The explicit end path below remains preferable because it also gives
        // AppKit one short settle interval after the record is posted.
        if let Some(command) = self.deactivate_target.take() {
            let front_psn = current_front_process_psn();
            if should_deactivate_synthetic_target(front_psn, command.psn) {
                let _ = post_synthetic_focus_command(&command);
            }
        }
    }
}

fn synthetic_target_focus_plan(target_psn: [u8; 8], target_wid: u32) -> SyntheticTargetFocusPlan {
    SyntheticTargetFocusPlan {
        activate_target: SyntheticFocusCommand {
            psn: target_psn,
            window_id: target_wid,
            focused: true,
        },
        deactivate_target: SyntheticFocusCommand {
            psn: target_psn,
            window_id: target_wid,
            focused: false,
        },
    }
}

fn synthetic_focus_record(window_id: u32, focused: bool) -> [u8; 0xF8] {
    let mut record = [0u8; 0xF8];
    record[0x04] = 0xF8;
    record[0x08] = 0x0D;
    record[0x3C..0x40].copy_from_slice(&window_id.to_le_bytes());
    record[0x8A] = if focused { 0x01 } else { 0x02 };
    record
}

fn current_front_process_psn() -> Option<[u8; 8]> {
    let get_front = get_front_process_fn()?;
    let mut psn = [0u8; 8];
    (unsafe { get_front(psn.as_mut_ptr() as *mut c_void) } == 0).then_some(psn)
}

fn workspace_front_process_psn() -> Option<[u8; 8]> {
    let pid = crate::apps::frontmost_pid().filter(|pid| *pid > 0)?;
    let get_pid_psn = get_process_for_pid_fn()?;
    let mut psn = [0u8; 8];
    (unsafe { get_pid_psn(pid, psn.as_mut_ptr() as *mut c_void) } == 0).then_some(psn)
}

fn should_restore_previous_process(
    current_front_psn: Option<[u8; 8]>,
    assisted_target_psn: [u8; 8],
) -> bool {
    // Restore is compare-and-swap, not unconditional. If the target is no
    // longer frontmost, a user or unrelated system event has already chosen a
    // newer foreground and must win. Same-process window changes can also be
    // caused by the assisted action itself (for example Cmd+W), so foreground
    // delivery still requires an external exclusive desktop lease.
    current_front_psn == Some(assisted_target_psn)
}

fn temporary_activation_moves_foreground(
    previous_psn: Option<[u8; 8]>,
    assisted_psns: &[[u8; 8]],
) -> bool {
    previous_psn.is_some_and(|previous| !assisted_psns.contains(&previous))
}

fn should_deactivate_synthetic_target(
    current_front_psn: Option<[u8; 8]>,
    target_psn: [u8; 8],
) -> bool {
    // Unknown is fail-safe for user intervention: leaving a process-local
    // synthetic belief behind is preferable to deactivating a target the user
    // may just have made genuinely frontmost.
    current_front_psn.is_some_and(|front_psn| front_psn != target_psn)
}

fn post_synthetic_focus_command(command: &SyntheticFocusCommand) -> anyhow::Result<()> {
    let post = post_event_record_to_fn()
        .ok_or_else(|| anyhow::anyhow!("target-only synthetic focus is unavailable"))?;
    let record = synthetic_focus_record(command.window_id, command.focused);
    let status = unsafe { post(command.psn.as_ptr() as *const c_void, record.as_ptr()) };
    if status != 0 {
        anyhow::bail!("target-only synthetic focus event failed with OSStatus {status}");
    }
    Ok(())
}

/// Make only `target_pid`'s exact window synthetically active for event routing.
///
/// This is intentionally not the traditional yabai/Cua
/// "focus-without-raise" sequence. That sequence first posted a defocus record
/// to the user's real foreground process; even though WindowServer did not
/// raise the target, AppKit delivered `resignActive`/`resignKey` and could move
/// the user's first responder. A background action must not mutate that state.
///
/// The returned context must be passed to [`end_synthetic_target_focus`] after
/// the target renderer has consumed the final input event.
pub fn begin_synthetic_target_focus(
    target_pid: pid_t,
    target_wid: u32,
) -> anyhow::Result<SyntheticTargetFocusContext> {
    if !is_synthetic_target_focus_available() {
        anyhow::bail!("target-only synthetic focus is unavailable");
    }

    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        anyhow::bail!(
            "could not resolve pid {target_pid} window {target_wid} for target-only synthetic focus"
        );
    }

    let plan = synthetic_target_focus_plan(target_psn, target_wid);
    // Own cleanup before posting: even a failed native post may have changed
    // the target's local activation state. Drop never addresses the real front.
    let context = SyntheticTargetFocusContext {
        deactivate_target: Some(plan.deactivate_target),
    };
    post_synthetic_focus_command(&plan.activate_target)?;
    std::thread::sleep(std::time::Duration::from_millis(40));
    Ok(context)
}

/// Remove the synthetic event-routing state installed on the target only.
pub fn end_synthetic_target_focus(mut context: SyntheticTargetFocusContext) -> anyhow::Result<()> {
    let command = context
        .deactivate_target
        .take()
        .ok_or_else(|| anyhow::anyhow!("target-only synthetic focus was already ended"))?;
    let front_psn = current_front_process_psn();
    if front_psn.is_none() {
        anyhow::bail!(
            "could not verify the real foreground during target-only synthetic focus cleanup; target state was left unchanged"
        );
    }
    if !should_deactivate_synthetic_target(front_psn, command.psn) {
        // Real activation supersedes the synthetic belief. In particular, do
        // not undo a user takeover that happened while the click was in flight.
        return Ok(());
    }
    post_synthetic_focus_command(&command)?;
    std::thread::sleep(std::time::Duration::from_millis(40));
    Ok(())
}

// ── NSMenu shortcut activation ────────────────────────────────────────────────

/// Gets the PSN for the process that owns `window_id`.
/// Uses `CGSMainConnectionID` + `SLSGetWindowOwner` + `SLSGetConnectionPSN`.
/// Falls back to `GetProcessForPID(pid)` when the SkyLight path fails.
pub fn get_process_psn_for_window(window_id: u32, pid: libc::pid_t, out_psn: &mut [u8; 8]) -> bool {
    // Try modern path: CGSMainConnectionID → SLSGetWindowOwner → SLSGetConnectionPSN
    if let (Some(get_owner), Some(get_psn), Some(conn_id_fn)) = (
        get_window_owner_fn(),
        get_connection_psn_fn(),
        connection_id_fn(),
    ) {
        let main_cid = unsafe { conn_id_fn() };
        let mut owner_cid: u32 = 0;
        let ok = unsafe { get_owner(main_cid, window_id, &mut owner_cid) } == 0;
        if ok && owner_cid != 0 {
            let psn_ok = unsafe { get_psn(owner_cid, out_psn.as_mut_ptr() as *mut c_void) } == 0;
            if psn_ok {
                return true;
            }
        }
    }
    // Fallback: GetProcessForPID
    if let Some(get_pid_psn) = get_process_for_pid_fn() {
        return unsafe { get_pid_psn(pid, out_psn.as_mut_ptr() as *mut c_void) } == 0;
    }
    false
}

/// Return whether WindowServer currently considers the exact window's process
/// frontmost. Unlike `NSWorkspace.frontmostApplication`, this query does not
/// depend on the caller's AppKit run loop processing an activation update.
pub fn front_process_matches(target_pid: libc::pid_t, target_wid: u32) -> Option<bool> {
    let get_front = get_front_process_fn()?;
    let mut front_psn = [0u8; 8];
    if unsafe { get_front(front_psn.as_mut_ptr() as *mut c_void) } != 0 {
        return None;
    }
    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        return None;
    }
    Some(front_psn == target_psn)
}

/// Make `target_pid` and `target_wid` WindowServer-frontmost and leave them
/// there. Unlike [`with_foreground_assist`], this deliberately does not save or
/// restore the previous process. It is the persistent counterpart required by
/// focus-proxy surfaces whose input channel is armed only while genuinely
/// frontmost.
///
/// Returns `true` only when the target PSN resolved and WindowServer accepted
/// `SLPSSetFrontProcessWithOptions`.
pub fn set_front_process_persistently(target_pid: libc::pid_t, target_wid: u32) -> bool {
    let Some(set_front) = set_front_process_fn() else {
        return false;
    };
    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        return false;
    }

    // kCPSNoWindows = 0x400. Supplying the exact target window still makes
    // that window's process frontmost while avoiding a broad all-window raise.
    crate::focus_steal::cancel_deferred_suppression(target_pid);
    unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x400) == 0 }
}

/// Called only by the successful bounded episode finalizer, after activity
/// and exact original-window identity have both been revalidated.
pub(crate) fn restore_exact_window_guarded(
    pid: i32,
    window: u32,
    mut check_activity: impl FnMut() -> anyhow::Result<()>,
) -> bool {
    let Some(set_front) = set_front_process_fn() else {
        return false;
    };
    let mut psn = [0_u8; 8];
    if !get_process_psn_for_window(window, pid, &mut psn)
        || check_exact_activation_owner(pid, window, &mut check_activity).is_err()
    {
        return false;
    }
    if unsafe { set_front(psn.as_ptr() as *const c_void, window, 0x200) } != 0 {
        return false;
    }
    if post_exact_key_window_records_guarded(psn, window, || {
        check_exact_activation_owner(pid, window, &mut check_activity)
    })
    .is_err()
    {
        return false;
    }
    if let Err(error) = complete_exact_ax_window_activation(pid, window, &mut check_activity) {
        tracing::warn!(pid, window, %error, "exact foreground restoration AX completion failed");
        return false;
    }
    match await_exact_window_ready_guarded(pid, window, psn, check_activity) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(pid, window, %error, "exact foreground restoration readiness failed");
            false
        }
    }
}

const FOREGROUND_AX_TIMEOUT_SECONDS: f32 = 0.1;
const MAX_FOREGROUND_AX_WINDOWS: usize = 32;

/// Own a Create/Copy reference immediately, including every member of AXWindows.
/// This is deliberately local to the synchronous activation/restore worker.
struct OwnedActivationAx(crate::ax::bindings::AXUIElementRef);

impl Drop for OwnedActivationAx {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { core_foundation::base::CFRelease(self.0.cast()) };
        }
    }
}

fn bound_activation_ax(element: &OwnedActivationAx) -> anyhow::Result<()> {
    if element.0.is_null()
        || unsafe {
            crate::ax::bindings::AXUIElementSetMessagingTimeout(
                element.0,
                FOREGROUND_AX_TIMEOUT_SECONDS,
            )
        } != crate::ax::bindings::kAXErrorSuccess
    {
        anyhow::bail!("bounded exact-window accessibility messaging is unavailable");
    }
    Ok(())
}

fn check_exact_activation_owner(
    pid: i32,
    window: u32,
    mut check_activity: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    check_activity()?;
    if !matches!(
        crate::windows::resolve_window_owner(pid, window),
        crate::windows::WindowOwner::SamePid
    ) {
        anyhow::bail!("exact activation window no longer belongs to the requested process");
    }
    check_activity()
}

fn exact_ax_activation_steps(
    mut check: impl FnMut() -> anyhow::Result<()>,
    mut write: impl FnMut(&'static str) -> i32,
    mut cleanup_unconfirmed: impl FnMut(),
) -> anyhow::Result<[i32; 3]> {
    let mut statuses = [0; 3];
    for (index, operation) in ["AXRaise", "AXMain", "AXFocused"].into_iter().enumerate() {
        check()?;
        statuses[index] = write(operation);
        if statuses[index] == crate::ax::bindings::kAXErrorCannotComplete {
            cleanup_unconfirmed();
        }
        if !matches!(
            statuses[index],
            crate::ax::bindings::kAXErrorSuccess
                | crate::ax::bindings::kAXErrorAttributeUnsupported
                | crate::ax::bindings::kAXErrorActionUnsupported
        ) {
            // CannotComplete can mean the action ran but its reply timed out.
            // Never continue the sequence (or dispatch HID) on that ambiguity.
            anyhow::bail!(
                "exact activation {operation} failed with AXError {}; an activation effect is possible",
                statuses[index]
            );
        }
    }
    check()?;
    Ok(statuses)
}

/// Finish only the requested native window's activation. Process activation
/// alone can leave a sibling window key, especially within the same process.
/// Attribute support varies, so receipts are diagnostic only: exact readiness
/// still has to be observed before HID, irrespective of these write statuses.
fn complete_exact_ax_window_activation(
    pid: i32,
    window_id: u32,
    mut check_activity: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<[i32; 3]> {
    use crate::ax::bindings::{
        ax_get_window_id, copy_string_attr, kAXErrorSuccess, perform_action, set_bool_attr_true,
        try_copy_ax_windows, AXUIElementCreateApplication, AXUIElementGetPid,
    };

    check_exact_activation_owner(pid, window_id, &mut check_activity)?;
    let app = OwnedActivationAx(unsafe { AXUIElementCreateApplication(pid) });
    bound_activation_ax(&app)?;
    let snapshot = unsafe { try_copy_ax_windows(app.0) }
        .map_err(|status| anyhow::anyhow!("exact activation AXWindows failed: {status}"))?;
    // Adopt ALL copied references before any fallible check/early return.
    let windows: Vec<_> = snapshot
        .windows
        .into_iter()
        .map(OwnedActivationAx)
        .collect();
    if !snapshot.complete || windows.len() > MAX_FOREGROUND_AX_WINDOWS {
        anyhow::bail!("exact activation AXWindows snapshot is incomplete or exceeds its bound");
    }
    let mut target = None;
    for window in &windows {
        check_activity()?;
        bound_activation_ax(window)?;
        if unsafe { ax_get_window_id(window.0) } == Some(window_id) {
            if target.replace(window).is_some() {
                anyhow::bail!("exact activation AX window is ambiguous");
            }
        }
    }
    let target =
        target.ok_or_else(|| anyhow::anyhow!("exact activation AX window is unavailable"))?;
    if unsafe { copy_string_attr(target.0, "AXRole") }.as_deref() != Some("AXWindow") {
        anyhow::bail!("exact activation requires an AXWindow, not a child or delegated surface");
    }
    exact_ax_activation_steps(
        || {
            check_exact_activation_owner(pid, window_id, &mut check_activity)?;
            let mut ax_pid = 0;
            if unsafe { AXUIElementGetPid(target.0, &mut ax_pid) } != kAXErrorSuccess
                || ax_pid != pid
                || unsafe { ax_get_window_id(target.0) } != Some(window_id)
            {
                anyhow::bail!("exact activation AX target identity changed");
            }
            check_activity()
        },
        |operation| unsafe {
            if operation == "AXRaise" {
                perform_action(target.0, operation)
            } else {
                set_bool_attr_true(target.0, operation)
            }
        },
        crate::foreground_activity::mark_native_cleanup_unconfirmed,
    )
}

fn bounded_focused_window_id(pid: i32) -> Option<u32> {
    let app = OwnedActivationAx(unsafe { crate::ax::bindings::AXUIElementCreateApplication(pid) });
    bound_activation_ax(&app).ok()?;
    let window = OwnedActivationAx(unsafe {
        crate::ax::bindings::copy_element_attr(app.0, "AXFocusedWindow")?
    });
    bound_activation_ax(&window).ok()?;
    unsafe { crate::ax::bindings::ax_get_window_id(window.0) }
}

/// Observable AX context only. AXFocusedWindow is NOT a proof of the target's
/// private NSWindow.isKeyWindow/main/active state; this experiment makes no such
/// claim. All references are owned by this synchronous, bounded operation.
struct BackgroundAxContext {
    window: OwnedActivationAx,
    window_id: u32,
    field: OwnedActivationAx,
}

#[derive(Debug)]
struct BackgroundBoolRead {
    value: Option<bool>,
    ax_error: i32,
    kind: &'static str,
}

fn background_bool_read(element: &OwnedActivationAx, name: &str) -> BackgroundBoolRead {
    use core_foundation::{
        base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
        boolean::CFBoolean,
        number::CFNumber,
        string::CFString,
    };
    let attribute = CFString::new(name);
    let mut raw: CFTypeRef = std::ptr::null();
    let ax_error = unsafe {
        crate::ax::bindings::AXUIElementCopyAttributeValue(
            element.0,
            attribute.as_concrete_TypeRef(),
            &mut raw,
        )
    };
    if ax_error != crate::ax::bindings::kAXErrorSuccess || raw.is_null() {
        if !raw.is_null() {
            unsafe { CFRelease(raw) };
        }
        return BackgroundBoolRead {
            value: None,
            ax_error,
            kind: if ax_error == 0 { "missing" } else { "error" },
        };
    }
    let type_id = unsafe { CFGetTypeID(raw) };
    let (value, kind) = if type_id == CFBoolean::type_id() {
        (
            Some(bool::from(unsafe {
                CFBoolean::wrap_under_create_rule(raw.cast())
            })),
            "boolean",
        )
    } else if type_id == CFNumber::type_id() {
        (
            unsafe { CFNumber::wrap_under_create_rule(raw.cast()) }
                .to_f64()
                .filter(|value| value.is_finite())
                .map(|value| value != 0.0),
            "number",
        )
    } else {
        unsafe { CFRelease(raw) };
        (None, "unexpected_type")
    };
    BackgroundBoolRead {
        value,
        ax_error,
        kind,
    }
}

#[derive(Debug)]
struct BackgroundVisibilityRead {
    pid: Option<i32>,
    pid_ax_error: i32,
    window: Option<u32>,
    window_ax_error: i32,
    minimized: BackgroundBoolRead,
    hidden: BackgroundBoolRead,
}

impl BackgroundVisibilityRead {
    fn permits(&self, pid: i32, window: u32) -> bool {
        self.pid == Some(pid)
            && self.window == Some(window)
            && self.minimized.value == Some(false)
            && self.hidden.value == Some(false)
    }

    fn may_refresh_unknown(&self, pid: i32, window: u32) -> bool {
        self.pid.is_none_or(|value| value == pid)
            && self.window.is_none_or(|value| value == window)
            && self.minimized.value != Some(true)
            && self.hidden.value != Some(true)
            && [
                self.pid_ax_error,
                self.window_ax_error,
                self.minimized.ax_error,
                self.hidden.ax_error,
            ]
            .into_iter()
            .all(|error| error != crate::ax::bindings::kAXErrorAPIDisabled)
            && !self.permits(pid, window)
    }
}

fn background_visibility_read(
    app: &OwnedActivationAx,
    window: &OwnedActivationAx,
) -> BackgroundVisibilityRead {
    let mut pid = 0;
    let pid_ax_error = unsafe { crate::ax::bindings::AXUIElementGetPid(window.0, &mut pid) };
    let mut window_id = 0;
    let window_ax_error =
        unsafe { crate::ax::bindings::_AXUIElementGetWindow(window.0, &mut window_id) };
    BackgroundVisibilityRead {
        pid: (pid_ax_error == 0 && pid > 0).then_some(pid),
        pid_ax_error,
        window: (window_ax_error == 0 && window_id != 0).then_some(window_id),
        window_ax_error,
        minimized: background_bool_read(window, "AXMinimized"),
        hidden: background_bool_read(app, "AXHidden"),
    }
}

fn capture_background_ax_window(
    app: &OwnedActivationAx,
    pid: i32,
    window_id: u32,
) -> anyhow::Result<OwnedActivationAx> {
    use crate::ax::bindings::{ax_get_window_id, copy_string_attr, try_copy_ax_windows};
    let copied = unsafe { try_copy_ax_windows(app.0) }
        .map_err(|status| anyhow::anyhow!("background AXWindows failed: {status}"))?;
    let windows: Vec<_> = copied.windows.into_iter().map(OwnedActivationAx).collect();
    if !copied.complete || windows.len() > MAX_FOREGROUND_AX_WINDOWS {
        anyhow::bail!("background AXWindows is incomplete or exceeds its bound");
    }
    let mut found = None;
    for window in windows {
        bound_activation_ax(&window)?;
        if unsafe { ax_get_window_id(window.0) } == Some(window_id) {
            background_ax_owner(&window, pid)?;
            if unsafe { copy_string_attr(window.0, "AXRole") }.as_deref() != Some("AXWindow")
                || found.is_some()
            {
                anyhow::bail!("background AX window is ambiguous or not a top-level window");
            }
            found = Some(window);
        }
    }
    found.ok_or_else(|| anyhow::anyhow!("background target is not in fresh AXWindows"))
}

fn background_ax_attribute(
    element: &OwnedActivationAx,
    attribute: &str,
) -> anyhow::Result<OwnedActivationAx> {
    let copied = unsafe { crate::ax::bindings::copy_element_attr(element.0, attribute) }
        .ok_or_else(|| anyhow::anyhow!("background AX context attribute is unavailable"))?;
    let copied = OwnedActivationAx(copied);
    bound_activation_ax(&copied)?;
    Ok(copied)
}

fn same_background_ax(left: &OwnedActivationAx, right: &OwnedActivationAx) -> bool {
    unsafe { core_foundation::base::CFEqual(left.0.cast(), right.0.cast()) != 0 }
}

fn background_ax_owner(element: &OwnedActivationAx, pid: i32) -> anyhow::Result<()> {
    let mut actual_pid = 0;
    if unsafe { crate::ax::bindings::AXUIElementGetPid(element.0, &mut actual_pid) }
        != crate::ax::bindings::kAXErrorSuccess
        || actual_pid != pid
    {
        anyhow::bail!("background AX element owner changed");
    }
    Ok(())
}

/// Strict parent-chain proof, bounded on every newly copied AX reference.
/// The native address field must reach this exact AXWindow without crossing
/// any web/document surface or a different process/window.
fn background_native_field_ancestry(
    field: &OwnedActivationAx,
    pid: i32,
    window_id: u32,
) -> anyhow::Result<()> {
    use crate::ax::bindings::{ax_get_window_id, copy_string_attr};
    let mut parent = None;
    for _ in 0..40 {
        let current = parent.as_ref().unwrap_or(field);
        background_ax_owner(current, pid)?;
        match unsafe { copy_string_attr(current.0, "AXRole") }.as_deref() {
            Some("AXWindow") => {
                if unsafe { ax_get_window_id(current.0) } != Some(window_id) {
                    anyhow::bail!("background field belongs to a sibling window");
                }
                return Ok(());
            }
            Some("AXWebArea" | "AXDocument" | "AXApplication") | None => {
                anyhow::bail!("background Return requires a native field outside web content");
            }
            _ => {}
        }
        parent = Some(background_ax_attribute(current, "AXParent")?);
    }
    anyhow::bail!("background field ancestry exceeded its bound")
}

fn capture_background_ax_context(
    app: &OwnedActivationAx,
    pid: i32,
) -> anyhow::Result<BackgroundAxContext> {
    use crate::ax::bindings::ax_get_window_id;
    let window = background_ax_attribute(app, "AXFocusedWindow")?;
    background_ax_owner(&window, pid)?;
    let window_id = unsafe { ax_get_window_id(window.0) }
        .ok_or_else(|| anyhow::anyhow!("background AX focused window identity is unavailable"))?;
    let field = background_ax_attribute(app, "AXFocusedUIElement")?;
    background_ax_owner(&field, pid)?;
    // Original context may be a web element. Its direct AXWindow relation is
    // sufficient for restoring this observed identity, not for key dispatch.
    let field_window = background_ax_attribute(&field, "AXWindow")?;
    if unsafe { ax_get_window_id(field_window.0) } != Some(window_id) {
        anyhow::bail!("original background AX window and field disagree");
    }
    Ok(BackgroundAxContext {
        window,
        window_id,
        field,
    })
}

const WINDOW_RETURN_ANCESTRY_LIMIT: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowReturnAxReadError {
    scope: &'static str,
    attribute: &'static str,
    status: i32,
    detail: &'static str,
    attribute_absent: bool,
    returned_value: bool,
}

impl std::fmt::Display for WindowReturnAxReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "window-tagged Return {}.{}: AXError={} ({}, returned_value={})",
            self.scope, self.attribute, self.status, self.detail, self.returned_value
        )
    }
}

impl std::error::Error for WindowReturnAxReadError {}

impl WindowReturnAxReadError {
    fn permits_parent_read(&self) -> bool {
        self.attribute_absent
            && matches!(
                self.status,
                crate::ax::bindings::kAXErrorNoValue
                    | crate::ax::bindings::kAXErrorAttributeUnsupported
            )
    }

    fn permits_native_field_focus_read(&self) -> bool {
        self.scope == "application"
            && self.attribute == "AXFocusedUIElement"
            && self.attribute_absent
            && self.status == crate::ax::bindings::kAXErrorNoValue
            && !self.returned_value
    }
}

/// This route needs the exact AX status: the shared try_copy_element_attr
/// intentionally merges NoValue and malformed success+NULL for legacy callers.
/// Keep that behavior unchanged, and retain precise errors in this reader only.
fn window_return_copy_status(
    scope: &'static str,
    attribute: &'static str,
    status: i32,
    has_value: bool,
    correct_type: bool,
) -> Result<(), WindowReturnAxReadError> {
    let detail = if status != crate::ax::bindings::kAXErrorSuccess {
        "attribute read failed"
    } else if !has_value {
        "success returned NULL"
    } else if !correct_type {
        "success returned wrong CF type"
    } else {
        return Ok(());
    };
    Err(WindowReturnAxReadError {
        scope,
        attribute,
        status,
        detail,
        returned_value: has_value,
        attribute_absent: matches!(
            status,
            crate::ax::bindings::kAXErrorNoValue
                | crate::ax::bindings::kAXErrorAttributeUnsupported
        ),
    })
}

// A private read-only seam for deterministic ancestry/focus tests. It is not an
// alternate source of focus evidence and is used only by the new Return route.
trait WindowReturnAxReader {
    type Node;
    fn element(
        &self,
        node: &Self::Node,
        attribute: &'static str,
        scope: &'static str,
    ) -> Result<Self::Node, WindowReturnAxReadError>;
    fn boolean(
        &self,
        node: &Self::Node,
        attribute: &'static str,
        scope: &'static str,
    ) -> Result<bool, WindowReturnAxReadError>;
    fn role(&self, node: &Self::Node, pid: i32, scope: &'static str) -> anyhow::Result<String>;
    fn window_id(&self, node: &Self::Node, scope: &'static str) -> anyhow::Result<u32>;
    fn same(&self, left: &Self::Node, right: &Self::Node) -> bool;
}

struct NativeWindowReturnAx;

impl WindowReturnAxReader for NativeWindowReturnAx {
    type Node = OwnedActivationAx;

    fn element(
        &self,
        node: &Self::Node,
        attribute: &'static str,
        scope: &'static str,
    ) -> Result<Self::Node, WindowReturnAxReadError> {
        use crate::ax::bindings::{
            AXUIElementCopyAttributeValue, AXUIElementGetTypeID, AXUIElementSetMessagingTimeout,
        };
        use core_foundation::{
            base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
            string::CFString,
        };
        let name = CFString::new(attribute);
        let mut value: CFTypeRef = std::ptr::null();
        let status = unsafe {
            AXUIElementCopyAttributeValue(node.0, name.as_concrete_TypeRef(), &mut value)
        };
        let valid_type =
            !value.is_null() && unsafe { CFGetTypeID(value) == AXUIElementGetTypeID() };
        if let Err(error) =
            window_return_copy_status(scope, attribute, status, !value.is_null(), valid_type)
        {
            // AX may supply an owned object even on failure.
            if !value.is_null() {
                unsafe { CFRelease(value) };
            }
            return Err(error);
        }
        let copied = OwnedActivationAx(value as crate::ax::bindings::AXUIElementRef);
        let status =
            unsafe { AXUIElementSetMessagingTimeout(copied.0, FOREGROUND_AX_TIMEOUT_SECONDS) };
        if status != 0 {
            return Err(WindowReturnAxReadError {
                scope,
                attribute,
                status,
                detail: "copied reference messaging timeout unavailable",
                attribute_absent: false,
                returned_value: true,
            });
        }
        Ok(copied)
    }

    fn boolean(
        &self,
        node: &Self::Node,
        attribute: &'static str,
        scope: &'static str,
    ) -> Result<bool, WindowReturnAxReadError> {
        use core_foundation::{
            base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
            boolean::CFBoolean,
            string::CFString,
        };
        let name = CFString::new(attribute);
        let mut value: CFTypeRef = std::ptr::null();
        let status = unsafe {
            crate::ax::bindings::AXUIElementCopyAttributeValue(
                node.0,
                name.as_concrete_TypeRef(),
                &mut value,
            )
        };
        let is_boolean = !value.is_null() && unsafe { CFGetTypeID(value) == CFBoolean::type_id() };
        if let Err(error) =
            window_return_copy_status(scope, attribute, status, !value.is_null(), is_boolean)
        {
            if !value.is_null() {
                unsafe { CFRelease(value) };
            }
            return Err(error);
        }
        // No numeric coercion: CFNumber(1) is not native AXFocused=true.
        Ok(bool::from(unsafe {
            CFBoolean::wrap_under_create_rule(value.cast())
        }))
    }

    fn role(&self, node: &Self::Node, pid: i32, scope: &'static str) -> anyhow::Result<String> {
        use crate::ax::bindings::{AXUIElementCopyAttributeValue, AXUIElementGetPid};
        use core_foundation::{
            base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
            string::CFString,
        };
        let mut actual_pid = 0;
        let status = unsafe { AXUIElementGetPid(node.0, &mut actual_pid) };
        anyhow::ensure!(status == 0 && actual_pid == pid, "window-tagged Return {scope}.AXPID: AXError={status}, expected={pid}, observed={actual_pid}");
        let name = CFString::new("AXRole");
        let mut value: CFTypeRef = std::ptr::null();
        let status = unsafe {
            AXUIElementCopyAttributeValue(node.0, name.as_concrete_TypeRef(), &mut value)
        };
        let valid_type = !value.is_null() && unsafe { CFGetTypeID(value) == CFString::type_id() };
        if let Err(error) =
            window_return_copy_status(scope, "AXRole", status, !value.is_null(), valid_type)
        {
            if !value.is_null() {
                unsafe { CFRelease(value) };
            }
            return Err(error.into());
        }
        Ok(unsafe { CFString::wrap_under_create_rule(value.cast()) }.to_string())
    }

    fn window_id(&self, node: &Self::Node, scope: &'static str) -> anyhow::Result<u32> {
        let mut window_id = 0;
        let status = unsafe { crate::ax::bindings::_AXUIElementGetWindow(node.0, &mut window_id) };
        anyhow::ensure!(status == 0 && window_id != 0, "window-tagged Return {scope}._AXUIElementGetWindow: AXError={status}, observed={window_id}");
        Ok(window_id)
    }

    fn same(&self, left: &Self::Node, right: &Self::Node) -> bool {
        same_background_ax(left, right)
    }
}

fn window_return_exact_ax_window<R: WindowReturnAxReader>(
    reader: &R,
    window: &R::Node,
    expected: &R::Node,
    pid: i32,
    window_id: u32,
    scope: &'static str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        reader.role(window, pid, scope)? == "AXWindow",
        "window-tagged Return {scope}.AXRole is not AXWindow"
    );
    let actual = reader.window_id(window, scope)?;
    anyhow::ensure!(actual == window_id && reader.same(window, expected), "window-tagged Return {scope}.AXWindow conflicts with exact membership: expected={window_id}, observed={actual}");
    Ok(())
}

fn window_return_field_window<R: WindowReturnAxReader>(
    reader: &R,
    field: &R::Node,
    expected: &R::Node,
    pid: i32,
    window_id: u32,
    scope: &'static str,
    check: &impl Fn() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    check()?;
    anyhow::ensure!(
        reader.role(field, pid, scope)? == "AXTextField",
        "window-tagged Return {scope}.AXRole is not AXTextField"
    );
    match reader.element(field, "AXWindow", scope) {
        Ok(window) => {
            check()?;
            // A present but contradictory direct edge is never repaired by
            // searching parents for a more convenient window.
            return window_return_exact_ax_window(reader, &window, expected, pid, window_id, scope);
        }
        Err(error) if error.permits_parent_read() => {}
        Err(error) => return Err(error.into()),
    }
    let mut parents = Vec::new();
    for _ in 0..WINDOW_RETURN_ANCESTRY_LIMIT {
        check()?;
        let current = parents.last().unwrap_or(field);
        let parent = reader.element(current, "AXParent", scope)?;
        anyhow::ensure!(
            !reader.same(&parent, field)
                && !parents.iter().any(|prior| reader.same(prior, &parent)),
            "window-tagged Return {scope}.AXParent cycle"
        );
        check()?;
        match reader.role(&parent, pid, scope)?.as_str() {
            "AXWindow" => {
                return window_return_exact_ax_window(
                    reader, &parent, expected, pid, window_id, scope,
                )
            }
            "AXSheet" | "AXApplication" => {
                anyhow::bail!("window-tagged Return {scope}.AXParent reached a forbidden boundary")
            }
            "" => anyhow::bail!("window-tagged Return {scope}.AXRole is empty"),
            _ => parents.push(parent),
        }
    }
    anyhow::bail!(
        "window-tagged Return {scope}.AXParent exceeded {WINDOW_RETURN_ANCESTRY_LIMIT} references"
    )
}

fn window_return_native_focus<R: WindowReturnAxReader>(
    reader: &R,
    app: &R::Node,
    field: &R::Node,
    expected: &R::Node,
    pid: i32,
    window_id: u32,
    check: &impl Fn() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    check()?;
    let focused_window = reader.element(app, "AXFocusedWindow", "application")?;
    window_return_exact_ax_window(
        reader,
        &focused_window,
        expected,
        pid,
        window_id,
        "focused_window",
    )?;
    check()?;
    let focused_field = match reader.element(app, "AXFocusedUIElement", "application") {
        Ok(focused_field) => focused_field,
        Err(error) if error.permits_native_field_focus_read() => {
            // An inactive Chrome app may report no application-level focused
            // object while the exact native field reports AXFocused=true.
            // This is alternate positive window-local AX focus evidence, not
            // equivalence to the app getter or proof of key-event delivery.
            window_return_field_window(
                reader,
                field,
                expected,
                pid,
                window_id,
                "focused_field",
                check,
            )?;
            check()?;
            anyhow::ensure!(
                reader.boolean(field, "AXFocused", "focused_field")?,
                "window-tagged Return focused_field.AXFocused is false"
            );
            check()?;
            tracing::debug!(
                pid,
                window_id,
                focus_evidence = "native_field_ax_focused_after_app_no_value",
                "window-tagged Return observed window-local native focus"
            );
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(reader.same(&focused_field, field), "window-tagged Return application.AXFocusedUIElement differs from the exact field; no focus will be written");
    window_return_field_window(
        reader,
        &focused_field,
        expected,
        pid,
        window_id,
        "focused_field",
        check,
    )
}

/// Read-only admission for a window-tagged Return. It deliberately has no
/// preparation, focus setter, synthetic context, restoration, or custom Drop.
pub(super) struct WindowReturnObservation<F: Fn() -> anyhow::Result<()>> {
    pid: i32,
    window_id: u32,
    app: OwnedActivationAx,
    field: OwnedActivationAx,
    front_pid: i32,
    front_window: u32,
    front_psn: [u8; 8],
    generation: u64,
    deadline: std::time::Instant,
    check_authority: F,
    trace: super::return_trace::Trace,
}

impl<F: Fn() -> anyhow::Result<()>> WindowReturnObservation<F> {
    pub(super) fn capture(
        pid: i32,
        window_id: u32,
        field: &crate::ax::cache::RetainedElement,
        trace: super::return_trace::Trace,
        check_authority: F,
    ) -> anyhow::Result<Self> {
        check_authority()?;
        crate::foreground_activity::check_request()?;
        if crate::apps::bundle_id_for_pid(pid).as_deref() != Some("com.google.Chrome") {
            anyhow::bail!("window-tagged Return is restricted to Google Chrome");
        }
        let generation = trace.run("capture.idle", crate::foreground_activity::require_idle)?;
        let front_pid = crate::apps::frontmost_pid()
            .filter(|front| *front > 0 && *front != pid)
            .ok_or_else(|| {
                anyhow::anyhow!("window-tagged Return requires another foreground process")
            })?;
        let front_window = trace.run("capture.front_window", || {
            bounded_focused_window_id(front_pid)
                .ok_or_else(|| anyhow::anyhow!("foreground window is unavailable"))
        })?;
        let front_psn = current_front_process_psn()
            .ok_or_else(|| anyhow::anyhow!("foreground process identity is unavailable"))?;
        let app =
            OwnedActivationAx(unsafe { crate::ax::bindings::AXUIElementCreateApplication(pid) });
        bound_activation_ax(&app)?;
        let raw = field.as_ptr() as crate::ax::bindings::AXUIElementRef;
        if raw.is_null() {
            anyhow::bail!("window-tagged Return field is unavailable");
        }
        unsafe { core_foundation::base::CFRetain(raw.cast()) };
        let field = OwnedActivationAx(raw);
        bound_activation_ax(&field)?;
        let observation = Self {
            pid,
            window_id,
            app,
            field,
            front_pid,
            front_window,
            front_psn,
            generation,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(3),
            check_authority,
            trace,
        };
        observation
            .trace
            .run("check.initial", || observation.check_before_post())?;
        Ok(observation)
    }

    /// Application effects may legitimately change the field after Return.
    /// Post-dispatch checks never try to refocus it or restore user state.
    pub(super) fn check_after_post(&self) -> anyhow::Result<()> {
        (self.check_authority)()?;
        crate::foreground_activity::check_request()?;
        if std::time::Instant::now() >= self.deadline
            || !crate::foreground_activity::generation_is_current(self.generation)
            || current_front_process_psn() != Some(self.front_psn)
            || crate::apps::frontmost_pid() != Some(self.front_pid)
            || bounded_focused_window_id(self.front_pid) != Some(self.front_window)
            || !matches!(
                crate::windows::resolve_window_owner(self.pid, self.window_id),
                crate::windows::WindowOwner::SamePid
            )
            || !matches!(
                crate::windows::resolve_window_owner(self.front_pid, self.front_window),
                crate::windows::WindowOwner::SamePid
            )
        {
            anyhow::bail!("window-tagged Return owner/activity/foreground evidence changed");
        }
        // AX/WindowServer queries above may block. Never let their duration
        // turn the generation/deadline sample at entry into post authority.
        (self.check_authority)()?;
        crate::foreground_activity::check_request()?;
        if std::time::Instant::now() >= self.deadline
            || !crate::foreground_activity::generation_is_current(self.generation)
            || current_front_process_psn() != Some(self.front_psn)
        {
            anyhow::bail!("window-tagged Return final activity/foreground admission changed");
        }
        Ok(())
    }

    pub(super) fn check_before_post(&self) -> anyhow::Result<()> {
        use crate::ax::bindings::{ax_get_window_id, copy_string_attr, try_copy_ax_windows};
        self.trace
            .run("admission.owner_activity", || self.check_after_post())?;
        let check = || {
            (self.check_authority)()?;
            crate::foreground_activity::check_request()?;
            anyhow::ensure!(
                std::time::Instant::now() < self.deadline
                    && crate::foreground_activity::generation_is_current(self.generation)
                    && current_front_process_psn() == Some(self.front_psn),
                "window-tagged Return read-only ancestry authority/activity changed"
            );
            Ok(())
        };
        let copied = self.trace.run("admission.ax_windows", || {
            unsafe { try_copy_ax_windows(self.app.0) }
                .map_err(|error| anyhow::anyhow!("window-tagged AXWindows unavailable: {error}"))
        })?;
        let windows: Vec<_> = copied.windows.into_iter().map(OwnedActivationAx).collect();
        self.trace.window_scan(windows.len(), copied.complete);
        if !copied.complete || windows.len() > MAX_FOREGROUND_AX_WINDOWS {
            anyhow::bail!("window-tagged AXWindows is incomplete");
        }
        let mut target_count = 0;
        let mut target_window = None;
        for window in windows {
            check()?;
            bound_activation_ax(&window)?;
            background_ax_owner(&window, self.pid)?;
            if unsafe { copy_string_attr(window.0, "AXRole") }.as_deref() != Some("AXWindow") {
                anyhow::bail!("window-tagged AXWindows contains an unproven window role");
            }
            let Some(id) = (unsafe { ax_get_window_id(window.0) }) else {
                anyhow::bail!("window-tagged AX window identity is unknown");
            };
            let minimized_span = self.trace.begin("admission.window_minimized");
            let minimized = background_bool_read(&window, "AXMinimized").value;
            minimized_span.finish(minimized.is_some());
            self.trace.window(id, minimized, id == self.window_id);
            if id == self.window_id {
                target_count += 1;
                if minimized != Some(false) {
                    anyhow::bail!("window-tagged target is not visibly eligible");
                }
                target_window = Some(window);
            } else if minimized != Some(true) {
                self.trace.event("admission.competing_window_refused");
                anyhow::bail!("window-tagged Return refuses competing or unknown Chrome windows");
            }
        }
        if target_count != 1 || background_bool_read(&self.app, "AXHidden").value != Some(false) {
            anyhow::bail!("window-tagged target membership or visibility is unproven");
        }
        let target_window = target_window
            .ok_or_else(|| anyhow::anyhow!("window-tagged exact AXWindow membership is missing"))?;
        self.trace.run("admission.field_ancestry", || {
            window_return_field_window(
                &NativeWindowReturnAx,
                &self.field,
                &target_window,
                self.pid,
                self.window_id,
                "requested_field",
                &check,
            )
        })?;
        // Ancestry only proves membership. Exact native window and field focus
        // evidence remain separate and mandatory; DOM focus is never used.
        self.trace.run("admission.native_focus", || {
            window_return_native_focus(
                &NativeWindowReturnAx,
                &self.app,
                &self.field,
                &target_window,
                self.pid,
                self.window_id,
                &check,
            )
        })?;
        // Re-sample after potentially slow AX reads, immediately before post.
        self.trace
            .run("admission.final_owner_activity", || self.check_after_post())
    }
}

/// Experimental target-local context. No set-front, AXRaise, suppression
/// restore, global HID or legacy retry is reachable from this implementation.
pub(super) struct ExactBackgroundReturnContext<F: Fn() -> anyhow::Result<()>> {
    pid: i32,
    window_id: u32,
    target_psn: [u8; 8],
    app: std::cell::RefCell<OwnedActivationAx>,
    target_window: std::cell::RefCell<OwnedActivationAx>,
    field: OwnedActivationAx,
    original: BackgroundAxContext,
    original_front_pid: i32,
    original_front_window: u32,
    original_front_psn: [u8; 8],
    generation: u64,
    deadline: std::time::Instant,
    check_authority: F,
    dirty: bool,
    cleanup_attempted: bool,
    native_write_unknown: bool,
}

impl<F: Fn() -> anyhow::Result<()>> ExactBackgroundReturnContext<F> {
    /// The caller must hold its same-PID mutation lease and retain a freshly
    /// resolved exact element token; check_authority revalidates that capability
    /// and canonical owner before each preparation/dispatch/cleanup write.
    pub(super) fn capture(
        pid: i32,
        window_id: u32,
        field: &crate::ax::cache::RetainedElement,
        check_authority: F,
    ) -> anyhow::Result<Self> {
        use crate::ax::bindings::{copy_string_attr, AXUIElementCreateApplication};
        check_authority()?;
        crate::foreground_activity::check_request()?;
        if crate::apps::bundle_id_for_pid(pid).as_deref() != Some("com.google.Chrome") {
            anyhow::bail!("experimental background Return is restricted to Google Chrome");
        }
        let generation = crate::foreground_activity::require_idle()?;
        let original_front_pid = crate::apps::frontmost_pid()
            .filter(|front| *front > 0 && *front != pid)
            .ok_or_else(|| {
                anyhow::anyhow!("background Return requires another foreground process")
            })?;
        let original_front_psn = current_front_process_psn()
            .ok_or_else(|| anyhow::anyhow!("real foreground identity is unavailable"))?;
        let original_front_window = bounded_focused_window_id(original_front_pid)
            .ok_or_else(|| anyhow::anyhow!("real foreground window is unavailable"))?;
        let app = OwnedActivationAx(unsafe { AXUIElementCreateApplication(pid) });
        bound_activation_ax(&app)?;
        let target_window = capture_background_ax_window(&app, pid, window_id)?;
        let raw_field = field.as_ptr() as crate::ax::bindings::AXUIElementRef;
        if raw_field.is_null() {
            anyhow::bail!("background Return field is unavailable");
        }
        unsafe { core_foundation::base::CFRetain(raw_field.cast()) };
        let field = OwnedActivationAx(raw_field);
        bound_activation_ax(&field)?;
        if unsafe { copy_string_attr(field.0, "AXRole") }.as_deref() != Some("AXTextField")
            || !["AXTitle", "AXDescription"].into_iter().any(|attribute| {
                unsafe { copy_string_attr(field.0, attribute) }.as_deref()
                    == Some("Address and search bar")
            })
        {
            anyhow::bail!("background Return requires a recognized native Chrome address field");
        }
        background_native_field_ancestry(&field, pid, window_id)?;
        let original = capture_background_ax_context(&app, pid)?;
        // Navigating this window can destroy its old web field. Such an
        // original context cannot truthfully be promised restorable. A native
        // toolbar field, or an unchanged sibling's field, remains admissible.
        if original.window_id == window_id {
            background_native_field_ancestry(&original.field, pid, window_id)?;
        }
        let mut target_psn = [0; 8];
        if !is_synthetic_target_focus_available()
            || !get_process_psn_for_window(window_id, pid, &mut target_psn)
        {
            anyhow::bail!("exact target-only keyboard context is unavailable");
        }
        let context = Self {
            pid,
            window_id,
            target_psn,
            app: std::cell::RefCell::new(app),
            target_window: std::cell::RefCell::new(target_window),
            field,
            original,
            original_front_pid,
            original_front_window,
            original_front_psn,
            generation,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(3),
            check_authority,
            dirty: false,
            cleanup_attempted: false,
            native_write_unknown: false,
        };
        context.check()?;
        Ok(context)
    }

    fn check(&self) -> anyhow::Result<()> {
        (self.check_authority)()?;
        crate::foreground_activity::check_request()?;
        if self.native_write_unknown
            || std::time::Instant::now() >= self.deadline
            || !crate::foreground_activity::generation_is_current(self.generation)
        {
            anyhow::bail!("background Return activity, deadline or cleanup evidence changed");
        }
        for (pid, window) in [
            (self.pid, self.window_id),
            (self.pid, self.original.window_id),
            (self.original_front_pid, self.original_front_window),
        ] {
            if !matches!(
                crate::windows::resolve_window_owner(pid, window),
                crate::windows::WindowOwner::SamePid
            ) {
                anyhow::bail!("background Return exact window ownership changed");
            }
        }
        self.check_visibility()?;
        let mut origin_psn = [0; 8];
        if !get_process_psn_for_window(
            self.original_front_window,
            self.original_front_pid,
            &mut origin_psn,
        ) || origin_psn != self.original_front_psn
        {
            anyhow::bail!("real foreground process/window identities disagree");
        }
        if current_front_process_psn() != Some(self.original_front_psn)
            || crate::apps::frontmost_pid() != Some(self.original_front_pid)
            || bounded_focused_window_id(self.original_front_pid)
                != Some(self.original_front_window)
        {
            anyhow::bail!("real foreground changed during background Return");
        }
        background_native_field_ancestry(&self.field, self.pid, self.window_id)?;
        // AX reads above can yield to the target process. Re-sample authority,
        // foreground and activity AFTER them, immediately before the caller's
        // next native write; a long ancestry query must not reuse old admission.
        (self.check_authority)()?;
        crate::foreground_activity::check_request()?;
        if current_front_process_psn() != Some(self.original_front_psn)
            || bounded_focused_window_id(self.original_front_pid)
                != Some(self.original_front_window)
            || std::time::Instant::now() >= self.deadline
            || !crate::foreground_activity::generation_is_current(self.generation)
        {
            anyhow::bail!("background Return final foreground/activity admission changed");
        }
        Ok(())
    }

    /// Read failures are not input attempts. Reacquire only the same exact
    /// window's observation handles, never the field token or mutation plan.
    /// Positive contradictory facts and revoked leases cannot enter this path.
    fn check_visibility(&self) -> anyhow::Result<()> {
        let held = background_visibility_read(&self.app.borrow(), &self.target_window.borrow());
        if held.permits(self.pid, self.window_id) {
            return Ok(());
        }
        if !held.may_refresh_unknown(self.pid, self.window_id) {
            anyhow::bail!("background target visibility or AX identity is no longer proven: expected_pid={}, expected_window={}, held={held:?}", self.pid, self.window_id);
        }
        let mut consecutive = 0;
        let mut last_read = None;
        for sample in 0..3 {
            (self.check_authority)()?;
            crate::foreground_activity::check_request()?;
            if self.native_write_unknown
                || std::time::Instant::now() >= self.deadline
                || !crate::foreground_activity::generation_is_current(self.generation)
                || current_front_process_psn() != Some(self.original_front_psn)
            {
                anyhow::bail!("background read-only reacquisition stopped: owner/activity/deadline/foreground changed; held={held:?}, last={last_read:?}");
            }
            if !matches!(
                crate::windows::resolve_window_owner(self.pid, self.window_id),
                crate::windows::WindowOwner::SamePid
            ) {
                anyhow::bail!("exact window owner changed during read-only reacquisition");
            }
            let acquired = (|| {
                let app = OwnedActivationAx(unsafe {
                    crate::ax::bindings::AXUIElementCreateApplication(self.pid)
                });
                bound_activation_ax(&app)?;
                let window = capture_background_ax_window(&app, self.pid, self.window_id)?;
                Ok::<_, anyhow::Error>((app, window))
            })();
            match acquired {
                Ok((app, window)) => {
                    let fresh = background_visibility_read(&app, &window);
                    if fresh.permits(self.pid, self.window_id) {
                        consecutive += 1;
                        if consecutive == 2 {
                            // The enclosing check still validates exact real
                            // foreground/field ancestry and samples authority +
                            // activity AFTER these reads, before any write.
                            *self.app.borrow_mut() = app;
                            *self.target_window.borrow_mut() = window;
                            return Ok(());
                        }
                    } else {
                        consecutive = 0;
                        if !fresh.may_refresh_unknown(self.pid, self.window_id) {
                            anyhow::bail!("background target contradicted read-only reacquisition: expected_pid={}, expected_window={}, held={held:?}, fresh={fresh:?}", self.pid, self.window_id);
                        }
                    }
                    last_read = Some(fresh);
                }
                Err(error) => {
                    // Failed exact membership/owner acquisition is not a
                    // visibility sample and must not be hidden by later reads.
                    anyhow::bail!("background read-only exact-window reacquisition failed: {error}; held={held:?}, fresh={last_read:?}");
                }
            }
            if sample < 2 {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        anyhow::bail!("background target visibility or AX identity is no longer proven after bounded read-only reacquisition: expected_pid={}, expected_window={}, held={held:?}, fresh={last_read:?}, samples=3", self.pid, self.window_id)
    }

    fn focus_field(&mut self, original: bool) -> anyhow::Result<()> {
        self.check()?;
        let field = if original {
            &self.original.field
        } else {
            &self.field
        };
        background_ax_owner(field, self.pid)?;
        let window = background_ax_attribute(field, "AXWindow")?;
        let expected_window = if original {
            self.original.window_id
        } else {
            self.window_id
        };
        if unsafe { crate::ax::bindings::ax_get_window_id(window.0) } != Some(expected_window) {
            anyhow::bail!("background field moved outside its bound window before focus");
        }
        self.check()?;
        let status = unsafe { crate::ax::bindings::set_bool_attr_true(field.0, "AXFocused") };
        if status != crate::ax::bindings::kAXErrorSuccess {
            if status == crate::ax::bindings::kAXErrorCannotComplete {
                self.native_write_unknown = true;
                crate::foreground_activity::mark_native_cleanup_unconfirmed();
            }
            anyhow::bail!("background AX field focus failed with AXError {status}");
        }
        self.check()
    }

    pub(super) fn prepare(&mut self) -> anyhow::Result<()> {
        self.check()?;
        self.dirty = true;
        post_synthetic_focus_command(&SyntheticFocusCommand {
            psn: self.target_psn,
            window_id: self.window_id,
            focused: true,
        })?;
        self.check()?;
        post_exact_key_window_records_guarded(self.target_psn, self.window_id, || self.check())?;
        self.focus_field(false)
    }

    fn observes_context(&self, original: bool) -> anyhow::Result<bool> {
        self.check()?;
        let observed = capture_background_ax_context(&self.app.borrow(), self.pid)?;
        let (window, field) = if original {
            (self.original.window_id, &self.original.field)
        } else {
            (self.window_id, &self.field)
        };
        let matches = super::background_keyboard::exact_background_ax_context_matches(
            window,
            observed.window_id,
            same_background_ax(&observed.field, field),
        );
        self.check()?;
        Ok(matches)
    }

    fn await_context(&self, original: bool) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
        let mut consecutive = 0;
        loop {
            if self.observes_context(original)? {
                consecutive += 1;
            } else {
                consecutive = 0;
            }
            if consecutive == 2 {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("exact background AX window/field readiness was not observed");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    pub(super) fn ready(&self) -> anyhow::Result<()> {
        self.await_context(false)
    }

    pub(super) fn verify_for_dispatch(&self) -> anyhow::Result<()> {
        if !self.observes_context(false)? {
            anyhow::bail!("background exact field focus changed before Return dispatch");
        }
        Ok(())
    }

    pub(super) fn settle(&self) -> anyhow::Result<()> {
        self.check()?;
        std::thread::sleep(std::time::Duration::from_millis(40));
        self.check()
    }

    pub(super) fn cleanup(&mut self) -> anyhow::Result<()> {
        self.cleanup_attempted = true;
        if !self.dirty {
            return Ok(());
        }
        let result = (|| {
            self.check()?;
            background_ax_owner(&self.original.window, self.pid)?;
            background_ax_owner(&self.original.field, self.pid)?;
            if unsafe { crate::ax::bindings::ax_get_window_id(self.original.window.0) }
                != Some(self.original.window_id)
            {
                anyhow::bail!("original background AX window identity changed");
            }
            if !self.observes_context(true)? {
                post_exact_key_window_records_guarded(
                    self.target_psn,
                    self.original.window_id,
                    || self.check(),
                )?;
                self.focus_field(true)?;
                self.await_context(true)?;
            }
            self.check()?;
            post_synthetic_focus_command(&SyntheticFocusCommand {
                psn: self.target_psn,
                window_id: self.original.window_id,
                focused: false,
            })?;
            self.await_context(true)?;
            self.dirty = false;
            Ok(())
        })();
        if result.is_err() {
            crate::foreground_activity::mark_native_cleanup_unconfirmed();
        }
        result
    }
}

impl<F: Fn() -> anyhow::Result<()>> Drop for ExactBackgroundReturnContext<F> {
    fn drop(&mut self) {
        if self.dirty && !self.cleanup_attempted {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.cleanup()));
            if !matches!(result, Ok(Ok(()))) {
                crate::foreground_activity::mark_native_cleanup_unconfirmed();
            }
        }
    }
}

/// No suppression re-entry or polling while the background observer holds its
/// final cancellation/expiry check lock.
pub(crate) fn submit_exact_window_restore(pid: i32, window: u32) -> bool {
    let Some(set_front) = set_front_process_fn() else {
        return false;
    };
    let mut psn = [0_u8; 8];
    if !get_process_psn_for_window(window, pid, &mut psn) {
        return false;
    }
    if unsafe { set_front(psn.as_ptr() as *const c_void, window, 0x200) } != 0 {
        return false;
    }
    post_exact_key_window_records(psn, window).is_ok()
}

fn make_key_window_record(window_id: u32, event_kind: u8) -> [u8; 0xF8] {
    let mut record = [0u8; 0xF8];
    record[0x04] = 0xF8;
    record[0x08] = event_kind;
    record[0x3A] = 0x10;
    record[0x3C..0x40].copy_from_slice(&window_id.to_le_bytes());
    record[0x20..0x30].fill(0xFF);
    record
}

fn post_exact_key_window_records(target_psn: [u8; 8], window_id: u32) -> anyhow::Result<()> {
    post_exact_key_window_records_guarded(target_psn, window_id, || Ok(()))
}

fn post_exact_key_window_records_guarded(
    target_psn: [u8; 8],
    window_id: u32,
    mut check_activity: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let post = post_event_record_to_fn()
        .ok_or_else(|| anyhow::anyhow!("exact key-window record posting is unavailable"))?;
    for event_kind in [0x01, 0x02] {
        let record = make_key_window_record(window_id, event_kind);
        check_activity()?;
        let status = unsafe { post(target_psn.as_ptr() as *const c_void, record.as_ptr()) };
        if status != 0 {
            anyhow::bail!("exact key-window record failed with OSStatus {status}");
        }
    }
    Ok(())
}

/// Make one exact application window native-key and frontmost.
///
/// Accessibility's `AXFocusedWindow` can change without AppKit making the
/// corresponding `NSWindow` key. Native menu validation observes the latter,
/// so focus-sensitive commands remain disabled in that split state. This is
/// the bounded exact-window sequence used by established macOS window tools:
/// mark the front-process request as user generated, synthesize the paired
/// make-key records for the requested WindowServer id, then let the caller
/// raise the matching AX window. No other application window is addressed.
pub fn make_exact_window_key(target_pid: libc::pid_t, target_wid: u32) -> bool {
    let Some(set_front) = set_front_process_fn() else {
        return false;
    };
    if post_event_record_to_fn().is_none() {
        return false;
    }
    let mut target_psn = [0u8; 8];
    if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
        return false;
    }

    // kCPSUserGenerated = 0x200. Unlike kCPSNoWindows, this permits AppKit to
    // establish the requested native key window before it validates NSMenu.
    crate::focus_steal::cancel_deferred_suppression(target_pid);
    if unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x200) } != 0 {
        return false;
    }
    post_exact_key_window_records(target_psn, target_wid).is_ok()
}

/// Tool-agnostic foreground-assist: briefly front `window_id`, wait for the
/// activation to actually land, run `body` (which posts the synthetic input),
/// then restore the prior frontmost process.
///
/// This is the `delivery_mode:"foreground"` rung of the best-effort-background
/// ladder, shared by `type_text` and `click`. Reached only when the agent has
/// seen the background rungs fail (clicks) or the field is unverifiable +
/// focus-sensitive (Catalyst typing).
///
/// ## Why this does not delegate to [`with_menu_shortcut_activation`]
///
/// It used to. That helper posts `set_front` and calls `action` immediately,
/// which is correct for its own purpose: NSMenu key dispatch only needs the key
/// event *enqueued* in the target's run-loop queue, so first-responder identity
/// is irrelevant and the sub-millisecond front → act → restore is a feature.
///
/// Input delivery has the opposite requirement. `set_front` is asynchronous, so
/// running the body straight away means the body's `AXFocused` write races
/// AppKit's own activation. AppKit wins: when activation completes it installs
/// the window's remembered first responder and clobbers the write. The
/// keystrokes then land wherever the app chose — for a Catalyst app such as
/// WhatsApp, the message list rather than the composer, which silently scrolls
/// the transcript instead of typing.
///
/// So this waits for the activation to be observable before running `body`, the
/// same ordering [`with_foreground_hid_activation`] already relies on. The wait
/// is a bounded poll rather than a fixed sleep: an app that activates in 10 ms
/// pays 10 ms, and a slow Catalyst/RDP surface still gets its full budget.
///
/// Returns `Ok(true)` when the brief activation happened, `Ok(false)` when the
/// fronting SPIs are unavailable (the body still ran, just without a front).
pub fn with_foreground_assist(
    target_pid: libc::pid_t,
    target_wid: u32,
    body: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    with_foreground_hid_activation(target_pid, target_wid, body)?;
    Ok(true)
}

/// Foreground an app-owned native Open/Save panel whose WindowServer surface
/// lives in Apple's background-only panel service. The helper itself is not an
/// activatable public application: activate the logical host, then require both
/// the host AX proxy and helper AX application to identify the exact delegated
/// panel before any global HID action runs.
pub(crate) fn with_foreground_app_context_panel_activation(
    route: crate::ax::app_context::AppContextDelegationRoute,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let _ = (route, action);
    anyhow::bail!(
        "foreground_activity_unavailable: delegated foreground episodes are not supported"
    )
}

pub(crate) fn with_foreground_assist_delegated(
    target_pid: libc::pid_t,
    target_wid: u32,
    delegation: Option<crate::ax::app_context::AppContextDelegationRoute>,
    body: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    if let Some(route) = delegation {
        if route.delegation.target
            != (crate::ax::app_context::AppContextTarget {
                pid: target_pid,
                window_id: target_wid,
            })
        {
            anyhow::bail!("delegated Open/Save panel target changed before activation");
        }
        with_foreground_app_context_panel_activation(route, body)?;
        Ok(true)
    } else {
        with_foreground_assist(target_pid, target_wid, body)
    }
}

/// Upper bound on how long [`with_foreground_assist`] waits for a requested
/// activation to become observable. Chosen to cover a Catalyst app's activation
/// plus key-window install; expiry refuses input without attempting focus restore.
const ACTIVATION_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);

/// Poll interval for foreground transition checks. Short enough that a fast native
/// app pays roughly one tick, long enough not to spin on the WindowServer.
const ACTIVATION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

const REQUIRED_READY_SAMPLES: u8 = 2;

fn exact_window_is_ready(
    current_front_psn: Option<[u8; 8]>,
    target_psn: [u8; 8],
    focused_window_id: Option<u32>,
    target_window_id: u32,
) -> bool {
    current_front_psn == Some(target_psn) && focused_window_id == Some(target_window_id)
}

/// Block until WindowServer and Accessibility agree that the exact target is
/// active, and require two consecutive samples so a stale AX value cannot make
/// an asynchronous foreground transition look complete.
///
/// A missing front-process query never counts as ready. Global HID callers fail
/// closed in that case because the event itself has no process address; the
/// older best-effort foreground-assist API may still choose to continue.
fn await_exact_window_ready(pid: libc::pid_t, window_id: u32, target_psn: [u8; 8]) -> bool {
    await_exact_window_ready_guarded(pid, window_id, target_psn, || Ok(())).is_ok()
}

fn await_exact_window_ready_guarded(
    pid: libc::pid_t,
    window_id: u32,
    target_psn: [u8; 8],
    mut check_activity: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    await_exact_window_ready_with(
        window_id,
        target_psn,
        || check_exact_activation_owner(pid, window_id, &mut check_activity),
        || (current_front_process_psn(), bounded_focused_window_id(pid)),
        || started.elapsed(),
        || std::thread::sleep(ACTIVATION_POLL_INTERVAL),
    )
}

fn await_exact_window_ready_with(
    window_id: u32,
    target_psn: [u8; 8],
    mut check_activity: impl FnMut() -> anyhow::Result<()>,
    mut sample: impl FnMut() -> (Option<[u8; 8]>, Option<u32>),
    mut elapsed: impl FnMut() -> std::time::Duration,
    mut pause: impl FnMut(),
) -> anyhow::Result<()> {
    let mut consecutive_ready_samples = 0u8;
    let mut samples = 0u32;
    let mut front_match = None;
    let mut focused_window = None;
    loop {
        check_activity().map_err(|error| anyhow::anyhow!(
            "exact foreground target readiness interrupted: {error}; front_match={front_match:?}, ax_focused_window={focused_window:?}, samples={samples}, ready_samples={consecutive_ready_samples}, elapsed_ms={}",
            elapsed().as_millis()
        ))?;
        let (front, focused) = sample();
        front_match = front.map(|psn| psn == target_psn);
        focused_window = focused;
        samples += 1;
        if exact_window_is_ready(front, target_psn, focused_window, window_id) {
            consecutive_ready_samples += 1;
        } else {
            consecutive_ready_samples = 0;
        }
        // AX is synchronous. Recheck activity after the bounded sample too,
        // before accepting even the second matching observation.
        check_activity().map_err(|error| anyhow::anyhow!(
            "exact foreground target readiness interrupted: {error}; front_match={front_match:?}, ax_focused_window={focused_window:?}, samples={samples}, ready_samples={consecutive_ready_samples}, elapsed_ms={}",
            elapsed().as_millis()
        ))?;
        let elapsed_ms = elapsed();
        if elapsed_ms >= ACTIVATION_WAIT_TIMEOUT {
            anyhow::bail!(
                "exact foreground target did not become ready: timeout; front_match={front_match:?}, ax_focused_window={focused_window:?}, target_window={window_id}, samples={samples}, ready_samples={consecutive_ready_samples}, elapsed_ms={}",
                elapsed_ms.as_millis()
            );
        }
        if consecutive_ready_samples >= REQUIRED_READY_SAMPLES {
            return Ok(());
        }
        pause();
    }
}

fn exact_window_activation_required(
    front_pid: Option<i32>,
    focused_window: Option<u32>,
    target_pid: i32,
    target_window: u32,
) -> bool {
    front_pid != Some(target_pid) || focused_window != Some(target_window)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestoreTransition {
    Pending,
    Restored,
    Superseded,
}

fn restore_transition(
    current_front_psn: Option<[u8; 8]>,
    assisted_psns: &[[u8; 8]],
    previous_psn: [u8; 8],
) -> RestoreTransition {
    match current_front_psn {
        Some(current) if current == previous_psn => RestoreTransition::Restored,
        Some(current) if !assisted_psns.contains(&current) => RestoreTransition::Superseded,
        _ => RestoreTransition::Pending,
    }
}

fn workspace_confirmed_restore_transition(
    current_front_psn: Option<[u8; 8]>,
    assisted_psns: &[[u8; 8]],
    previous_psn: [u8; 8],
    workspace_front_psn: impl FnOnce() -> Option<[u8; 8]>,
) -> RestoreTransition {
    let transition = restore_transition(current_front_psn, assisted_psns, previous_psn);
    if transition == RestoreTransition::Restored {
        // PiP also consumes NSWorkspace's frontmost application, which can lag
        // WindowServer. Keep the activation hold until both agree, without
        // querying AppKit while WindowServer is pending or already superseded.
        restore_transition(workspace_front_psn(), assisted_psns, previous_psn)
    } else {
        transition
    }
}

/// Wait for an asynchronous foreground restore to settle before allowing the
/// next action or PiP refresh to sample foreground state. WindowServer and
/// NSWorkspace must both confirm restoration. A third-party takeover ends the
/// wait immediately; the driver must never overwrite or delay genuine input.
fn await_previous_process_restore(
    assisted_psns: &[[u8; 8]],
    previous_psn: [u8; 8],
) -> RestoreTransition {
    let deadline = std::time::Instant::now() + ACTIVATION_WAIT_TIMEOUT;
    loop {
        let transition = workspace_confirmed_restore_transition(
            current_front_process_psn(),
            assisted_psns,
            previous_psn,
            workspace_front_process_psn,
        );
        if transition != RestoreTransition::Pending || std::time::Instant::now() >= deadline {
            return transition;
        }
        std::thread::sleep(ACTIVATION_POLL_INTERVAL);
    }
}

fn await_previous_process_restore_or_log(
    target_pid: libc::pid_t,
    target_wid: u32,
    assisted_psns: &[[u8; 8]],
    previous_psn: [u8; 8],
) {
    if await_previous_process_restore(assisted_psns, previous_psn) == RestoreTransition::Pending {
        tracing::warn!(
            target: "platform_macos::input::skylight",
            target_pid,
            target_wid,
            "foreground process restore did not settle before timeout"
        );
    }
}

/// Activate an exact target window for a global HID keyboard action.
///
/// This helper must not run `action` when the foreground SPI or native activity
/// evidence is unavailable: a global HID event has no
/// pid addressing and would otherwise land in whichever application is
/// currently frontmost. The short settles keep the target frontmost until
/// WindowServer has routed both sides of the key chord. Exact prior-window
/// restoration requires success and an uninterrupted activity episode.
pub fn with_foreground_hid_activation(
    target_pid: libc::pid_t,
    target_wid: u32,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_foreground_hid_activation_inner(target_pid, target_wid, None, None, action)
}

pub(crate) fn with_foreground_hid_activation_delegated(
    target_pid: libc::pid_t,
    target_wid: u32,
    delegation: Option<crate::ax::app_context::AppContextDelegationRoute>,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_foreground_hid_activation_inner(target_pid, target_wid, None, delegation, action)
}

fn with_foreground_hid_activation_inner(
    target_pid: libc::pid_t,
    target_wid: u32,
    transient_route: Option<crate::transient_ui::TransientRoute>,
    app_context_delegation: Option<crate::ax::app_context::AppContextDelegationRoute>,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // Delegated panels without an exact native foreground identity need an
    // atomic native episode contract. Do not revive the old activation bypass.
    if app_context_delegation.is_some() {
        anyhow::bail!(
            "foreground_activity_unavailable: delegated foreground episodes are not supported"
        );
    }
    let _ = transient_route;
    let episode = crate::foreground_activity::Episode::begin(target_pid, target_wid)?;
    let mut pip_activation = None;
    let result = (|| {
        crate::focus_steal::cancel_deferred_suppression(target_pid);
        let set_front = set_front_process_fn()
            .ok_or_else(|| anyhow::anyhow!("foreground HID delivery is unavailable"))?;
        let mut target_psn = [0u8; 8];
        if !get_process_psn_for_window(target_wid, target_pid, &mut target_psn) {
            anyhow::bail!("could not resolve target window for foreground HID delivery");
        }
        if exact_window_activation_required(
            crate::apps::frontmost_pid(),
            bounded_focused_window_id(target_pid),
            target_pid,
            target_wid,
        ) {
            episode.check()?;
            pip_activation = Some(crate::pip::begin_temporary_activation(
                target_pid, target_wid, None,
            ));
            check_exact_activation_owner(target_pid, target_wid, || episode.check())?;
            if unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x400) } != 0 {
                anyhow::bail!("WindowServer rejected foreground HID activation");
            }
            check_exact_activation_owner(target_pid, target_wid, || episode.check())?;
            // Do not call the unguarded multi-write activation helper: native
            // intervention between any two SPI writes must stop the sequence.
            if unsafe { set_front(target_psn.as_ptr() as *const c_void, target_wid, 0x200) } != 0 {
                anyhow::bail!("WindowServer rejected exact foreground key-window activation");
            }
            post_exact_key_window_records_guarded(target_psn, target_wid, || {
                check_exact_activation_owner(target_pid, target_wid, || episode.check())
            })?;
            let ax_statuses =
                complete_exact_ax_window_activation(target_pid, target_wid, || episode.check())?;
            await_exact_window_ready_guarded(target_pid, target_wid, target_psn, || {
                episode.check()
            })
            .map_err(|error| anyhow::anyhow!("{error}; ax_activation_statuses={ax_statuses:?}"))?;
            crate::foreground_activity::check_input()?;
            let result = action();
            std::thread::sleep(std::time::Duration::from_millis(40));
            return result;
        }
        crate::foreground_activity::check_input()?;
        action()
    })();
    // Keep the PiP temporary-activation hold through restoration even when an
    // activation/action error requires normal safe settlement.
    let result = episode.finish(result);
    drop(pip_activation);
    result
}

fn transient_route_authorizes_auxiliary_bypass(
    route: Option<crate::transient_ui::TransientRoute>,
    target: crate::transient_ui::WindowTarget,
) -> bool {
    route.is_some_and(|route| route.target == target)
}

const BLENDER_BUNDLE_ID: &str = "org.blenderfoundation.blender";

pub(crate) fn foreground_keyboard_focus_click_for_bundle_id(bundle_id: Option<&str>) -> bool {
    bundle_id == Some(BLENDER_BUNDLE_ID)
}

pub(crate) fn foreground_keyboard_focus_click_policy(
    bundle_id: Option<&str>,
    explicit_focus_already_established: bool,
) -> bool {
    !explicit_focus_already_established && foreground_keyboard_focus_click_for_bundle_id(bundle_id)
}

fn foreground_keyboard_focus_click_for_pid(
    pid: i32,
    explicit_focus_already_established: bool,
) -> bool {
    foreground_keyboard_focus_click_policy(
        crate::apps::bundle_id_for_pid(pid).as_deref(),
        explicit_focus_already_established,
    )
}

/// Select the exact-window foreground keyboard activation policy from the
/// target application's identity. Blender/GHOST needs one real click at the
/// validated internal anchor before shortcuts or text are accepted; ordinary
/// applications (including Screen Sharing) retain move-only context priming.
/// An explicit pixel or AX focus action also suppresses the derived click so it
/// cannot overwrite a more specific caller-selected keyboard destination.
pub fn with_foreground_keyboard_target_activation(
    target_pid: libc::pid_t,
    target_wid: u32,
    remembered_cursor: Option<(f64, f64)>,
    explicit_focus_already_established: bool,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_foreground_keyboard_context_activation_inner(
        target_pid,
        target_wid,
        remembered_cursor,
        foreground_keyboard_focus_click_for_pid(target_pid, explicit_focus_already_established),
        None,
        None,
        action,
    )
}

pub(crate) fn with_foreground_keyboard_target_activation_routed(
    target_pid: libc::pid_t,
    target_wid: u32,
    remembered_cursor: Option<(f64, f64)>,
    explicit_focus_already_established: bool,
    transient_route: Option<crate::transient_ui::TransientRoute>,
    app_context_delegation: Option<crate::ax::app_context::AppContextDelegationRoute>,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_foreground_keyboard_context_activation_inner(
        target_pid,
        target_wid,
        remembered_cursor,
        foreground_keyboard_focus_click_for_pid(target_pid, explicit_focus_already_established),
        transient_route,
        app_context_delegation,
        action,
    )
}

/// Activate one exact window, establish the pointer-owned keyboard context
/// used by custom canvases, and then run a foreground keyboard action.
///
/// The remembered agent-cursor position is accepted only while it remains
/// inside the exact window's current WindowServer frame. Otherwise the live
/// frame's center is used. Pointer restoration happens inside the activation
/// guard and deliberately emits no restoring `MouseMoved`, so applications
/// such as Blender/GHOST retain the target context while the user's hardware
/// cursor returns to its original position.
pub fn with_foreground_keyboard_context_activation(
    target_pid: libc::pid_t,
    target_wid: u32,
    remembered_cursor: Option<(f64, f64)>,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_foreground_keyboard_context_activation_inner(
        target_pid,
        target_wid,
        remembered_cursor,
        false,
        None,
        None,
        action,
    )
}

pub(crate) fn with_foreground_keyboard_context_activation_routed(
    target_pid: libc::pid_t,
    target_wid: u32,
    remembered_cursor: Option<(f64, f64)>,
    transient_route: Option<crate::transient_ui::TransientRoute>,
    app_context_delegation: Option<crate::ax::app_context::AppContextDelegationRoute>,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_foreground_keyboard_context_activation_inner(
        target_pid,
        target_wid,
        remembered_cursor,
        false,
        transient_route,
        app_context_delegation,
        action,
    )
}

/// Exact-window keyboard activation with one internal focus click at the
/// derived anchor. Used only by Blender foreground keyboard actions, whose
/// editor context is not established by activation and pointer motion alone.
pub fn with_foreground_keyboard_focus_activation(
    target_pid: libc::pid_t,
    target_wid: u32,
    remembered_cursor: Option<(f64, f64)>,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_foreground_keyboard_context_activation_inner(
        target_pid,
        target_wid,
        remembered_cursor,
        true,
        None,
        None,
        action,
    )
}

pub(crate) fn with_foreground_keyboard_focus_activation_routed(
    target_pid: libc::pid_t,
    target_wid: u32,
    remembered_cursor: Option<(f64, f64)>,
    transient_route: Option<crate::transient_ui::TransientRoute>,
    app_context_delegation: Option<crate::ax::app_context::AppContextDelegationRoute>,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_foreground_keyboard_context_activation_inner(
        target_pid,
        target_wid,
        remembered_cursor,
        true,
        transient_route,
        app_context_delegation,
        action,
    )
}

fn with_foreground_keyboard_context_activation_inner(
    target_pid: libc::pid_t,
    target_wid: u32,
    remembered_cursor: Option<(f64, f64)>,
    focus_click: bool,
    transient_route: Option<crate::transient_ui::TransientRoute>,
    app_context_delegation: Option<crate::ax::app_context::AppContextDelegationRoute>,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    with_foreground_hid_activation_inner(
        target_pid,
        target_wid,
        transient_route,
        app_context_delegation,
        || {
            let bounds = crate::windows::window_bounds_by_id(target_wid).ok_or_else(|| {
                anyhow::anyhow!(
                    "target window {target_wid} closed before foreground keyboard delivery"
                )
            })?;
            let anchor =
                crate::input::mouse::foreground_keyboard_anchor(&bounds, remembered_cursor)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "target window {target_wid} has no valid frame for foreground keyboard delivery"
                        )
                    })?;
            if focus_click {
                crate::input::mouse::with_foreground_keyboard_pointer_context_and_focus_click(
                    anchor, action,
                )
            } else {
                crate::input::mouse::with_foreground_keyboard_pointer_context(anchor, action)
            }
        },
    )
}

fn preserves_exact_existing_focus(
    previous_process_known: bool,
    previous_psn: [u8; 8],
    target_psn: [u8; 8],
    focused_window_id: Option<u32>,
    target_window_id: u32,
) -> bool {
    previous_process_known
        && previous_psn == target_psn
        && focused_window_id == Some(target_window_id)
}

/// Activate `target_pid`'s window `target_wid` for NSMenu key dispatch, run `action`,
/// then immediately restore the prior frontmost process.
///
/// Activation, action, and restoration are requested without a fixed settle
/// delay: NSMenu receives the key event before restoration is requested. Then
/// confirm the bounded restore before releasing PiP's temporary activation hold.
///
/// Returns `Ok(true)` when activation succeeded, `Ok(false)` when SPIs unavailable.
pub fn with_menu_shortcut_activation(
    target_pid: libc::pid_t,
    target_wid: u32,
    action: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    with_foreground_hid_activation(target_pid, target_wid, action)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn window_return_public_post_requires_exact_opt_in() {
        use super::WindowReturnPostRoute;
        use std::ffi::OsStr;
        for value in [None, Some(""), Some("0"), Some("true"), Some("01")] {
            assert_eq!(
                WindowReturnPostRoute::from_flag(value.map(OsStr::new)),
                WindowReturnPostRoute::SkyLight
            );
        }
        assert_eq!(
            WindowReturnPostRoute::from_flag(Some(OsStr::new("1"))),
            WindowReturnPostRoute::PublicCg
        );
    }

    #[test]
    fn window_return_public_post_never_requires_private_symbol() {
        assert_eq!(
            super::select_window_return_post(
                super::WindowReturnPostRoute::PublicCg,
                "public",
                || { panic!("public route must not resolve the private posting symbol") }
            ),
            Some("public")
        );
        let resolutions = std::cell::Cell::new(0);
        assert_eq!(
            super::select_window_return_post(
                super::WindowReturnPostRoute::SkyLight,
                "public",
                || {
                    resolutions.set(resolutions.get() + 1);
                    None
                }
            ),
            None,
            "missing private transport must not fall back to public"
        );
        assert_eq!(resolutions.get(), 1);
    }

    #[test]
    fn window_return_post_selector_invokes_only_one_selected_fake() {
        use std::cell::Cell;
        type FakePost = fn(&Cell<usize>, &Cell<usize>);
        fn public(public_calls: &Cell<usize>, _: &Cell<usize>) {
            public_calls.set(public_calls.get() + 1);
        }
        fn skylight(_: &Cell<usize>, skylight_calls: &Cell<usize>) {
            skylight_calls.set(skylight_calls.get() + 1);
        }
        for route in [
            super::WindowReturnPostRoute::PublicCg,
            super::WindowReturnPostRoute::SkyLight,
        ] {
            let public_calls = Cell::new(0);
            let skylight_calls = Cell::new(0);
            let resolutions = Cell::new(0);
            let selected = super::select_window_return_post(route, public as FakePost, || {
                resolutions.set(resolutions.get() + 1);
                Some(skylight as FakePost)
            })
            .unwrap();
            selected(&public_calls, &skylight_calls);
            let public_selected = route == super::WindowReturnPostRoute::PublicCg;
            assert_eq!(public_calls.get(), usize::from(public_selected));
            assert_eq!(skylight_calls.get(), usize::from(!public_selected));
            assert_eq!(resolutions.get(), usize::from(!public_selected));
        }
        // Only local counter functions execute. No CGEvent or native post.
    }

    #[test]
    fn window_return_public_post_abi_is_pid_and_cg_event() {
        let _native: unsafe extern "C" fn(libc::pid_t, core_graphics::sys::CGEventRef) =
            super::public_cg_event_post_to_pid;
        let _bridge: super::PostToPidFn = super::public_cg_post;
        // Compile-time ABI checks only: neither function is invoked.
    }

    #[derive(Clone)]
    struct WindowReturnFakeNode {
        pid: i32,
        role: &'static str,
        window_id: u32,
    }

    struct WindowReturnFakeAx {
        nodes: std::collections::HashMap<usize, WindowReturnFakeNode>,
        edges: std::collections::HashMap<
            (usize, &'static str),
            Result<usize, super::WindowReturnAxReadError>,
        >,
        booleans: std::collections::HashMap<
            (usize, &'static str),
            Result<bool, super::WindowReturnAxReadError>,
        >,
        reads: std::cell::RefCell<Vec<(usize, &'static str)>>,
    }

    impl WindowReturnFakeAx {
        fn new() -> Self {
            Self {
                nodes: [
                    (0, "AXTextField", 0),
                    (1, "AXWindow", 42),
                    (2, "AXWebArea", 0),
                    (3, "AXApplication", 0),
                    (4, "AXTextField", 0),
                    (5, "AXWindow", 43),
                ]
                .into_iter()
                .map(|(id, role, window_id)| {
                    (
                        id,
                        WindowReturnFakeNode {
                            pid: 7,
                            role,
                            window_id,
                        },
                    )
                })
                .collect(),
                edges: [
                    ((0, "AXParent"), Ok(2)),
                    ((2, "AXParent"), Ok(1)),
                    ((3, "AXFocusedWindow"), Ok(1)),
                    ((3, "AXFocusedUIElement"), Ok(0)),
                ]
                .into_iter()
                .collect(),
                booleans: Default::default(),
                reads: Default::default(),
            }
        }

        fn failure(&mut self, node: usize, attribute: &'static str, status: i32) {
            self.edges.insert(
                (node, attribute),
                Err(
                    super::window_return_copy_status("fake", attribute, status, false, false)
                        .unwrap_err(),
                ),
            );
        }

        fn prove(&self) -> anyhow::Result<()> {
            super::window_return_field_window(self, &0, &1, 7, 42, "requested_field", &|| Ok(()))
        }
    }

    impl super::WindowReturnAxReader for WindowReturnFakeAx {
        type Node = usize;

        fn element(
            &self,
            node: &usize,
            attribute: &'static str,
            scope: &'static str,
        ) -> Result<usize, super::WindowReturnAxReadError> {
            self.reads.borrow_mut().push((*node, attribute));
            let result = self
                .edges
                .get(&(*node, attribute))
                .cloned()
                .unwrap_or_else(|| {
                    Err(super::window_return_copy_status(
                        scope,
                        attribute,
                        crate::ax::bindings::kAXErrorNoValue,
                        false,
                        false,
                    )
                    .unwrap_err())
                });
            result.map_err(|mut error| {
                error.scope = scope;
                error.attribute = attribute;
                error
            })
        }

        fn role(&self, node: &usize, pid: i32, scope: &'static str) -> anyhow::Result<String> {
            let value = &self.nodes[node];
            anyhow::ensure!(value.pid == pid, "{scope}.AXPID mismatch");
            Ok(value.role.to_owned())
        }

        fn boolean(
            &self,
            node: &usize,
            attribute: &'static str,
            scope: &'static str,
        ) -> Result<bool, super::WindowReturnAxReadError> {
            self.reads.borrow_mut().push((*node, attribute));
            self.booleans
                .get(&(*node, attribute))
                .cloned()
                .unwrap_or_else(|| {
                    Err(super::window_return_copy_status(
                        scope,
                        attribute,
                        crate::ax::bindings::kAXErrorNoValue,
                        false,
                        false,
                    )
                    .unwrap_err())
                })
                .map_err(|mut error| {
                    error.scope = scope;
                    error.attribute = attribute;
                    error
                })
        }

        fn window_id(&self, node: &usize, _scope: &'static str) -> anyhow::Result<u32> {
            Ok(self.nodes[node].window_id)
        }

        fn same(&self, left: &usize, right: &usize) -> bool {
            left == right
        }
    }

    #[test]
    fn window_return_ancestry_accepts_only_explicit_absence_then_exact_parent() {
        for status in [
            crate::ax::bindings::kAXErrorNoValue,
            crate::ax::bindings::kAXErrorAttributeUnsupported,
        ] {
            let mut reader = WindowReturnFakeAx::new();
            reader.failure(0, "AXWindow", status);
            reader.prove().unwrap();
            assert_eq!(
                *reader.reads.borrow(),
                [(0, "AXWindow"), (0, "AXParent"), (2, "AXParent")]
            );
        }
    }

    #[test]
    fn window_return_ancestry_direct_conflict_never_searches_parents() {
        for target in [4, 5] {
            let mut reader = WindowReturnFakeAx::new();
            reader.edges.insert((0, "AXWindow"), Ok(target));
            assert!(reader.prove().is_err());
            assert_eq!(*reader.reads.borrow(), [(0, "AXWindow")]);
        }
        let mut reader = WindowReturnFakeAx::new();
        reader.nodes.get_mut(&5).unwrap().window_id = 42;
        reader.edges.insert((0, "AXWindow"), Ok(5));
        assert!(
            reader.prove().is_err(),
            "matching numeric window ID does not replace native AX identity"
        );
    }

    #[test]
    fn window_return_ancestry_rejects_foreign_sheet_cycle_and_nearest_sibling() {
        let mut foreign = WindowReturnFakeAx::new();
        foreign.nodes.get_mut(&2).unwrap().pid = 8;
        assert!(foreign.prove().unwrap_err().to_string().contains("AXPID"));
        let mut sheet = WindowReturnFakeAx::new();
        sheet.nodes.get_mut(&2).unwrap().role = "AXSheet";
        assert!(sheet
            .prove()
            .unwrap_err()
            .to_string()
            .contains("forbidden boundary"));
        let mut cycle = WindowReturnFakeAx::new();
        cycle.edges.insert((2, "AXParent"), Ok(0));
        assert!(cycle.prove().unwrap_err().to_string().contains("cycle"));
        let mut sibling = WindowReturnFakeAx::new();
        sibling.edges.insert((2, "AXParent"), Ok(5));
        sibling.edges.insert((5, "AXParent"), Ok(1));
        assert!(sibling.prove().is_err());
        assert!(!sibling.reads.borrow().contains(&(5, "AXParent")));
    }

    #[test]
    fn window_return_ancestry_rejects_timeout_and_reports_exact_attribute_scope() {
        for attribute in ["AXWindow", "AXParent"] {
            let mut reader = WindowReturnFakeAx::new();
            reader.failure(0, attribute, crate::ax::bindings::kAXErrorCannotComplete);
            let error = reader.prove().unwrap_err().to_string();
            assert!(
                error.contains(&format!("requested_field.{attribute}")),
                "{error}"
            );
            assert!(error.contains("AXError=-25204"), "{error}");
        }
        let mut reader = WindowReturnFakeAx::new();
        reader.failure(
            3,
            "AXFocusedWindow",
            crate::ax::bindings::kAXErrorCannotComplete,
        );
        let error = super::window_return_native_focus(&reader, &3, &0, &1, 7, 42, &|| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("application.AXFocusedWindow") && error.contains("AXError=-25204"));
    }

    #[test]
    fn window_return_ancestry_null_wrong_type_and_timeout_setup_never_fallback() {
        for (has_value, correct_type) in [(false, false), (true, false)] {
            let error = super::window_return_copy_status(
                "requested_field",
                "AXWindow",
                0,
                has_value,
                correct_type,
            )
            .unwrap_err();
            assert!(!error.permits_parent_read());
            let mut reader = WindowReturnFakeAx::new();
            reader.edges.insert((0, "AXWindow"), Err(error));
            assert!(reader.prove().is_err());
            assert_eq!(*reader.reads.borrow(), [(0, "AXWindow")]);
        }
        for status in [
            crate::ax::bindings::kAXErrorNoValue,
            crate::ax::bindings::kAXErrorAttributeUnsupported,
        ] {
            let mut reader = WindowReturnFakeAx::new();
            reader.edges.insert(
                (0, "AXWindow"),
                Err(super::WindowReturnAxReadError {
                    scope: "requested_field",
                    attribute: "AXWindow",
                    status,
                    detail: "copied reference messaging timeout unavailable",
                    attribute_absent: false,
                    returned_value: true,
                }),
            );
            assert!(reader.prove().is_err());
            assert_eq!(*reader.reads.borrow(), [(0, "AXWindow")]);
        }
    }

    #[test]
    fn window_return_native_focus_remains_separate_from_ancestry() {
        let reader = WindowReturnFakeAx::new();
        super::window_return_native_focus(&reader, &3, &0, &1, 7, 42, &|| Ok(())).unwrap();
        for (attribute, target) in [("AXFocusedWindow", 5), ("AXFocusedUIElement", 4)] {
            let mut reader = WindowReturnFakeAx::new();
            reader.edges.insert((3, attribute), Ok(target));
            assert!(reader.prove().is_ok(), "membership alone still holds");
            assert!(
                super::window_return_native_focus(&reader, &3, &0, &1, 7, 42, &|| Ok(())).is_err()
            );
        }
        let mut reader = WindowReturnFakeAx::new();
        reader.failure(
            3,
            "AXFocusedUIElement",
            crate::ax::bindings::kAXErrorNoValue,
        );
        let error = super::window_return_native_focus(&reader, &3, &0, &1, 7, 42, &|| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("focused_field.AXFocused") && error.contains("AXError=-25212"));
    }

    #[test]
    fn window_return_native_field_focus_true_only_after_empty_app_no_value() {
        let mut reader = WindowReturnFakeAx::new();
        reader.failure(
            3,
            "AXFocusedUIElement",
            crate::ax::bindings::kAXErrorNoValue,
        );
        reader.booleans.insert((0, "AXFocused"), Ok(true));
        super::window_return_native_focus(&reader, &3, &0, &1, 7, 42, &|| Ok(())).unwrap();
        assert_eq!(
            *reader.reads.borrow(),
            [
                (3, "AXFocusedWindow"),
                (3, "AXFocusedUIElement"),
                (0, "AXWindow"),
                (0, "AXParent"),
                (2, "AXParent"),
                (0, "AXFocused")
            ]
        );
    }

    #[test]
    fn window_return_native_field_focus_rejects_false_null_number_and_read_errors() {
        let mut invalid = vec![Ok(false)];
        // A success with NULL, or any non-CFBoolean (including CFNumber(1)),
        // is refused by the exact classifier used by the native bool reader.
        for (status, has_value, is_boolean) in [
            (0, false, false),
            (0, true, false),
            (crate::ax::bindings::kAXErrorNoValue, false, false),
            (
                crate::ax::bindings::kAXErrorAttributeUnsupported,
                false,
                false,
            ),
            (crate::ax::bindings::kAXErrorCannotComplete, false, false),
        ] {
            invalid.push(Err(super::window_return_copy_status(
                "focused_field",
                "AXFocused",
                status,
                has_value,
                is_boolean,
            )
            .unwrap_err()));
        }
        for value in invalid {
            let mut reader = WindowReturnFakeAx::new();
            reader.failure(
                3,
                "AXFocusedUIElement",
                crate::ax::bindings::kAXErrorNoValue,
            );
            reader.booleans.insert((0, "AXFocused"), value);
            assert!(
                super::window_return_native_focus(&reader, &3, &0, &1, 7, 42, &|| Ok(())).is_err()
            );
        }
    }

    #[test]
    fn window_return_native_field_focus_does_not_mask_app_getter_errors() {
        let mut failures = Vec::new();
        for (status, has_value, is_element) in [
            (crate::ax::bindings::kAXErrorNoValue, true, true),
            (
                crate::ax::bindings::kAXErrorAttributeUnsupported,
                false,
                false,
            ),
            (crate::ax::bindings::kAXErrorCannotComplete, false, false),
            (0, false, false),
            (0, true, false),
        ] {
            failures.push(
                super::window_return_copy_status(
                    "application",
                    "AXFocusedUIElement",
                    status,
                    has_value,
                    is_element,
                )
                .unwrap_err(),
            );
        }
        for returned_value in [false, true] {
            failures.push(super::WindowReturnAxReadError {
                scope: "application",
                attribute: "AXFocusedUIElement",
                status: crate::ax::bindings::kAXErrorNoValue,
                detail: "copied reference messaging timeout unavailable",
                attribute_absent: false,
                returned_value,
            });
        }
        for failure in failures {
            assert!(!failure.permits_native_field_focus_read());
            let mut reader = WindowReturnFakeAx::new();
            reader.edges.insert((3, "AXFocusedUIElement"), Err(failure));
            reader.booleans.insert((0, "AXFocused"), Ok(true));
            assert!(
                super::window_return_native_focus(&reader, &3, &0, &1, 7, 42, &|| Ok(())).is_err()
            );
            assert!(!reader.reads.borrow().contains(&(0, "AXFocused")));
        }
    }

    #[test]
    fn window_return_native_field_focus_cannot_override_conflicting_identity() {
        for (attribute, target) in [("AXFocusedUIElement", 4), ("AXFocusedWindow", 5)] {
            let mut reader = WindowReturnFakeAx::new();
            reader.booleans.insert((0, "AXFocused"), Ok(true));
            reader.edges.insert((3, attribute), Ok(target));
            assert!(
                super::window_return_native_focus(&reader, &3, &0, &1, 7, 42, &|| Ok(())).is_err()
            );
            assert!(!reader.reads.borrow().contains(&(0, "AXFocused")));
        }
        let mut reader = WindowReturnFakeAx::new();
        reader.failure(
            3,
            "AXFocusedUIElement",
            crate::ax::bindings::kAXErrorNoValue,
        );
        reader.edges.insert((0, "AXWindow"), Ok(5));
        reader.booleans.insert((0, "AXFocused"), Ok(true));
        assert!(super::window_return_native_focus(&reader, &3, &0, &1, 7, 42, &|| Ok(())).is_err());
        assert!(!reader.reads.borrow().contains(&(0, "AXFocused")));
    }

    #[test]
    fn window_return_ancestry_bounds_depth_and_checks_authority_between_reads() {
        let mut reader = WindowReturnFakeAx::new();
        for node in 2..=42 {
            reader.nodes.insert(
                node,
                WindowReturnFakeNode {
                    pid: 7,
                    role: "AXGroup",
                    window_id: 0,
                },
            );
            reader.edges.insert((node, "AXParent"), Ok(node + 1));
        }
        assert!(reader
            .prove()
            .unwrap_err()
            .to_string()
            .contains("exceeded 40"));
        assert_eq!(reader.reads.borrow().len(), 41);
        let reader = WindowReturnFakeAx::new();
        let checks = std::cell::Cell::new(0);
        let error =
            super::window_return_field_window(&reader, &0, &1, 7, 42, "requested_field", &|| {
                checks.set(checks.get() + 1);
                anyhow::ensure!(checks.get() < 3, "request cancelled");
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "request cancelled");
        assert_eq!(*reader.reads.borrow(), [(0, "AXWindow"), (0, "AXParent")]);
    }

    // These CLI tests deliberately exercise the private constructor without a
    // daemon main loop. TIS/TSM permits serialized access in a non-UI process;
    // this test-only lock is not the production main-thread delivery mechanism.
    static WINDOW_RETURN_TIS_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn window_return_native_constructor_preserves_exact_window_and_key_pair() {
        use foreign_types::ForeignType;
        let _tis = WINDOW_RETURN_TIS_TEST.lock().unwrap();
        let down = super::construct_window_tagged_return_event(7_654_321, true).unwrap();
        let up = super::construct_window_tagged_return_event(7_654_321, false).unwrap();
        assert_ne!(down.as_ptr(), up.as_ptr());
        assert!(matches!(
            down.get_type(),
            core_graphics::event::CGEventType::KeyDown
        ));
        assert!(matches!(
            up.get_type(),
            core_graphics::event::CGEventType::KeyUp
        ));
        for event in [&down, &up] {
            assert_eq!(event.get_integer_value_field(51), 7_654_321);
            assert_eq!(event.get_integer_value_field(9), 36);
            assert_eq!(
                event.get_flags(),
                core_graphics::event::CGEventFlags::CGEventFlagNull
            );
        }
        let marker =
            down.get_integer_value_field(core_graphics::event::EventField::EVENT_SOURCE_USER_DATA);
        assert_eq!(
            up.get_integer_value_field(core_graphics::event::EventField::EVENT_SOURCE_USER_DATA),
            marker
        );
        // The pair owns two independent standard CGEvents, not two retained
        // handles to one mutable bridged event.
        down.set_integer_value_field(51, 7_654_322);
        assert_eq!(up.get_integer_value_field(51), 7_654_321);
        assert_eq!(
            up.get_integer_value_field(core_graphics::event::EventField::EVENT_SOURCE_USER_DATA),
            marker
        );
        // Constructor internally also verifies NSEvent CR/ignoring-modifiers,
        // repeat and windowNumber after its in-place window/cookie stamps.
        // These test objects are never authenticated or posted.
    }

    #[test]
    fn window_return_standard_worker_unicode_is_preserved_in_place() {
        let _tis = WINDOW_RETURN_TIS_TEST.lock().unwrap();
        use core_graphics::{
            event::{CGEvent, CGEventFlags, EventField},
            event_source::{CGEventSource, CGEventSourceStateID},
        };
        use foreign_types::ForeignType;
        fn read(event: &CGEvent, capacity: usize) -> Vec<u16> {
            extern "C" {
                fn CGEventKeyboardGetUnicodeString(
                    event: core_graphics::sys::CGEventRef,
                    max_length: usize,
                    actual_length: *mut usize,
                    characters: *mut u16,
                );
            }
            let mut buffer = vec![0u16; capacity];
            let mut count = 0;
            unsafe {
                CGEventKeyboardGetUnicodeString(
                    event.as_ptr(),
                    capacity,
                    &mut count,
                    buffer.as_mut_ptr(),
                )
            };
            assert!(count <= capacity);
            buffer.truncate(count);
            buffer
        }
        for down in [true, false] {
            let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
            let event = CGEvent::new_keyboard_event(source, 36, down).unwrap();
            event.set_flags(CGEventFlags::CGEventFlagNull);
            let baseline = read(&event, 16);
            assert!(baseline.is_empty() || baseline == [13]);
            let source_state = event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID);
            let keyboard_type =
                event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYBOARD_TYPE);
            let sample = |stage: &str, window: i64| {
                assert_eq!(read(&event, 4), baseline, "{stage} small buffer Unicode");
                assert_eq!(read(&event, 16), baseline, "{stage} Unicode");
                assert_eq!(
                    event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID),
                    source_state,
                    "{stage} source"
                );
                assert_eq!(
                    event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYBOARD_TYPE),
                    keyboard_type,
                    "{stage} keyboard type"
                );
                assert_eq!(event.get_integer_value_field(51), window, "{stage} window");
            };
            sample("baseline", 0);
            event.set_integer_value_field(51, 7_654_321);
            sample("tag51", 7_654_321);
            crate::foreground_activity::mark_generated(&event);
            sample("cookie", 7_654_321);
            objc2::rc::autoreleasepool(|_| unsafe {
                use objc2::{class, msg_send};
                use objc2_app_kit::NSEvent;
                let native: *mut NSEvent = msg_send![class!(NSEvent), eventWithCGEvent: event.as_ptr().cast::<std::ffi::c_void>()];
                let native = native.as_ref().unwrap();
                assert_eq!(
                    native
                        .characters()
                        .map(|value| value.to_string())
                        .as_deref(),
                    Some("\r")
                );
                assert_eq!(
                    native
                        .charactersIgnoringModifiers()
                        .map(|value| value.to_string())
                        .as_deref(),
                    Some("\r")
                );
                sample("native_characters", 7_654_321);
            });
        }
        // In-memory invariance only: no auth, posting, copy/serialization or
        // Unicode override. Optional worker payloads remain exactly unchanged.
    }

    #[test]
    fn window_return_native_constructor_preserves_standard_hid_key_facts() {
        let _tis = WINDOW_RETURN_TIS_TEST.lock().unwrap();
        use core_graphics::{
            event::{CGEvent, CGEventFlags, EventField},
            event_source::{CGEventSource, CGEventSourceStateID},
        };
        use foreign_types::ForeignType;
        fn unicode(event: &CGEvent) -> Vec<u16> {
            extern "C" {
                fn CGEventKeyboardGetUnicodeString(
                    event: core_graphics::sys::CGEventRef,
                    max_length: usize,
                    actual_length: *mut usize,
                    characters: *mut u16,
                );
            }
            // Match the constructor's native readback for this standard CG
            // control; CG Unicode may legitimately be empty on this thread.
            objc2::rc::autoreleasepool(|_| unsafe {
                use objc2::{class, msg_send};
                use objc2_app_kit::NSEvent;
                let native: *mut NSEvent = msg_send![class!(NSEvent), eventWithCGEvent: event.as_ptr().cast::<std::ffi::c_void>()];
                let native = native
                    .as_ref()
                    .expect("standard CG event has native readback");
                assert_eq!(
                    native
                        .characters()
                        .map(|value| value.to_string())
                        .as_deref(),
                    Some("\r")
                );
                assert_eq!(
                    native
                        .charactersIgnoringModifiers()
                        .map(|value| value.to_string())
                        .as_deref(),
                    Some("\r")
                );
            });
            let mut buffer = [0u16; 16];
            let mut count = 0;
            unsafe {
                CGEventKeyboardGetUnicodeString(
                    event.as_ptr(),
                    buffer.len(),
                    &mut count,
                    buffer.as_mut_ptr(),
                )
            };
            assert!(count <= buffer.len());
            buffer[..count].to_vec()
        }
        for down in [true, false] {
            let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
            let control = CGEvent::new_keyboard_event(source, 36, down).unwrap();
            control.set_flags(CGEventFlags::CGEventFlagNull);
            let candidate = super::construct_window_tagged_return_event(7_654_321, down).unwrap();
            assert_ne!(candidate.as_ptr(), control.as_ptr());
            assert_eq!(candidate.get_type() as u32, control.get_type() as u32);
            assert_eq!(candidate.get_flags(), control.get_flags());
            for field in [
                EventField::KEYBOARD_EVENT_KEYCODE,
                EventField::KEYBOARD_EVENT_AUTOREPEAT,
                EventField::KEYBOARD_EVENT_KEYBOARD_TYPE,
                EventField::EVENT_SOURCE_STATE_ID,
                EventField::EVENT_SOURCE_UNIX_PROCESS_ID,
            ] {
                assert_eq!(
                    candidate.get_integer_value_field(field),
                    control.get_integer_value_field(field),
                    "known field {field} must retain standard CG behavior"
                );
            }
            assert_eq!(
                candidate.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID),
                CGEventSourceStateID::HIDSystemState as i64
            );
            assert_eq!(unicode(&candidate), unicode(&control));
            assert!(unicode(&candidate).is_empty() || unicode(&candidate) == [13]);
            crate::foreground_activity::mark_generated(&control);
            assert_eq!(
                candidate.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA),
                control.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
            );
            assert_eq!(candidate.get_integer_value_field(51), 7_654_321);
            assert_eq!(control.get_integer_value_field(51), 0);
        }
        // Deliberately no copy/serialization or assertion about unknown field101.
    }

    #[test]
    fn window_return_native_constructor_refuses_unaddressed_window() {
        assert!(super::construct_window_tagged_return_event(0, true).is_err());
    }

    use super::{
        await_exact_window_ready_with, exact_ax_activation_steps, exact_window_activation_required,
        exact_window_is_ready, foreground_keyboard_focus_click_for_bundle_id,
        foreground_keyboard_focus_click_policy, make_key_window_record,
        preserves_exact_existing_focus, restore_transition, should_deactivate_synthetic_target,
        should_restore_previous_process, synthetic_focus_record, synthetic_target_focus_plan,
        temporary_activation_moves_foreground, transient_route_authorizes_auxiliary_bypass,
        workspace_confirmed_restore_transition, RestoreTransition,
    };
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn background_visibility_only_unknown_reads_allow_reacquisition() {
        let read = |pid, window, minimized, hidden| super::BackgroundVisibilityRead {
            pid,
            pid_ax_error: 0,
            window,
            window_ax_error: 0,
            minimized: super::BackgroundBoolRead {
                value: minimized,
                ax_error: 0,
                kind: "boolean",
            },
            hidden: super::BackgroundBoolRead {
                value: hidden,
                ax_error: 0,
                kind: "boolean",
            },
        };
        assert!(read(Some(7), Some(12), Some(false), Some(false)).permits(7, 12));
        for unknown in [
            read(None, Some(12), Some(false), Some(false)),
            read(Some(7), None, Some(false), Some(false)),
            read(Some(7), Some(12), None, Some(false)),
            read(Some(7), Some(12), Some(false), None),
        ] {
            assert!(!unknown.permits(7, 12));
            assert!(unknown.may_refresh_unknown(7, 12));
        }
        for contrary in [
            read(Some(8), Some(12), None, Some(false)),
            read(Some(7), Some(13), None, Some(false)),
            read(Some(7), Some(12), Some(true), None),
            read(Some(7), Some(12), None, Some(true)),
        ] {
            assert!(!contrary.permits(7, 12));
            assert!(!contrary.may_refresh_unknown(7, 12));
        }
        let mut denied = read(Some(7), Some(12), None, Some(false));
        denied.minimized.ax_error = crate::ax::bindings::kAXErrorAPIDisabled;
        assert!(!denied.may_refresh_unknown(7, 12));
    }

    #[test]
    fn authentication_factory_guard_checks_class_methods_not_instance_methods() {
        let class = super::objc_class(c"NSString");
        assert!(!class.is_null(), "Foundation NSString must be loaded");
        // NSObject's root-class/metaclass inheritance makes -init visible to
        // class_getClassMethod on some runtimes. NSString's +string and -length
        // have independently checked, distinct class/instance method slots.
        type GetInstanceMethodFn = unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        let lookup: GetInstanceMethodFn = unsafe {
            super::as_fn(
                super::find_sym(b"class_getInstanceMethod\0")
                    .expect("Objective-C instance method metadata lookup is available"),
            )
        };
        let class_selector = super::sel_register(c"string");
        let instance_selector = super::sel_register(c"length");
        // Metadata only: no object allocation or method invocation.
        assert!(unsafe { lookup(class, class_selector) }.is_null());
        assert!(!unsafe { lookup(class, instance_selector) }.is_null());
        assert!(super::class_has_class_method(class, class_selector));
        assert!(!super::class_has_class_method(class, instance_selector));
    }

    #[test]
    fn authentication_factory_guard_refuses_missing_class_or_method() {
        let class = super::objc_class(c"NSObject");
        assert!(!super::class_has_class_method(
            std::ptr::null_mut(),
            super::sel_register(c"alloc")
        ));
        assert!(!super::class_has_class_method(class, std::ptr::null_mut()));
        assert!(!super::class_has_class_method(
            class,
            super::sel_register(c"cuaMissingAuthenticationFactoryForRegressionTest:")
        ));
    }

    #[test]
    fn foreground_same_process_different_window_requires_activation() {
        assert!(exact_window_activation_required(Some(7), Some(41), 7, 42));
        assert!(exact_window_activation_required(None, Some(42), 7, 42));
        assert!(exact_window_activation_required(Some(7), None, 7, 42));
    }

    #[test]
    fn foreground_exact_focused_target_skips_reactivation() {
        let mut activation_writes = 0;
        if exact_window_activation_required(Some(7), Some(42), 7, 42) {
            activation_writes += 1;
        }
        assert_eq!(activation_writes, 0);
    }

    #[test]
    fn foreground_ax_guard_loss_stops_remaining_writes_and_hid() {
        for lose_at in 0..=3 {
            let checks = Cell::new(0);
            let mut writes = Vec::new();
            let hid = Cell::new(false);
            let result = exact_ax_activation_steps(
                || {
                    let index = checks.get();
                    checks.set(index + 1);
                    anyhow::ensure!(index != lose_at, "owner or activity changed");
                    Ok(())
                },
                |operation| {
                    writes.push(operation);
                    0
                },
                || panic!("guard refusal is not an AX timeout"),
            )
            .map(|_| hid.set(true));
            assert!(result.is_err());
            assert_eq!(writes.len(), lose_at);
            assert!(!hid.get());
        }
    }

    #[test]
    fn foreground_ax_unsupported_receipts_do_not_mark_cleanup_unknown() {
        use crate::ax::bindings::{kAXErrorActionUnsupported, kAXErrorAttributeUnsupported};
        let mut operations = Vec::new();
        let statuses = exact_ax_activation_steps(
            || Ok(()),
            |operation| {
                operations.push(operation);
                if operation == "AXRaise" {
                    kAXErrorActionUnsupported
                } else {
                    kAXErrorAttributeUnsupported
                }
            },
            || panic!("unsupported is not unknown cleanup"),
        )
        .unwrap();
        assert_eq!(operations, ["AXRaise", "AXMain", "AXFocused"]);
        assert_eq!(statuses[0], kAXErrorActionUnsupported);
        // Write receipts alone never count as readiness; the production caller
        // still invokes the strict sampler before HID.
    }

    #[test]
    fn foreground_ax_timeout_marks_unknown_and_stops_writes_and_hid() {
        let writes = Cell::new(0);
        let marked = Cell::new(false);
        let hid = Cell::new(false);
        let result = exact_ax_activation_steps(
            || Ok(()),
            |_| {
                writes.set(writes.get() + 1);
                crate::ax::bindings::kAXErrorCannotComplete
            },
            || marked.set(true),
        )
        .map(|_| hid.set(true));
        assert!(result.is_err());
        assert_eq!(writes.get(), 1);
        assert!(marked.get());
        assert!(!hid.get());
    }

    #[test]
    fn foreground_readiness_requires_two_consecutive_exact_samples() {
        let psn = [1; 8];
        let mut windows = [Some(42), Some(41), Some(42), Some(42)].into_iter();
        let samples = Cell::new(0);
        let elapsed = Cell::new(Duration::ZERO);
        let result = await_exact_window_ready_with(
            42,
            psn,
            || Ok(()),
            || {
                samples.set(samples.get() + 1);
                (Some(psn), windows.next().expect("bounded samples"))
            },
            || elapsed.get(),
            || elapsed.set(elapsed.get() + Duration::from_millis(10)),
        );
        assert!(result.is_ok());
        assert_eq!(samples.get(), 4);
    }

    #[test]
    fn foreground_readiness_mismatch_retains_timeout_diagnostics() {
        let psn = [1; 8];
        let elapsed = Cell::new(Duration::ZERO);
        let hid = Cell::new(false);
        let error = await_exact_window_ready_with(
            42,
            psn,
            || Ok(()),
            || (Some(psn), Some(41)),
            || elapsed.get(),
            || elapsed.set(elapsed.get() + Duration::from_millis(100)),
        )
        .map(|_| hid.set(true))
        .unwrap_err()
        .to_string();
        assert!(error.contains("timeout"));
        assert!(error.contains("front_match=Some(true)"));
        assert!(error.contains("ax_focused_window=Some(41)"));
        assert!(error.contains("samples=5"));
        assert!(error.contains("elapsed_ms=400"));
        assert!(!hid.get());
    }

    #[test]
    fn foreground_readiness_guard_loss_after_sample_stops_hid() {
        let checks = Cell::new(0);
        let hid = Cell::new(false);
        let error = await_exact_window_ready_with(
            42,
            [1; 8],
            || {
                checks.set(checks.get() + 1);
                anyhow::ensure!(checks.get() != 4, "activity changed during AX sample");
                Ok(())
            },
            || (Some([1; 8]), Some(42)),
            || Duration::from_millis(10),
            || {},
        )
        .map(|_| hid.set(true))
        .unwrap_err()
        .to_string();
        assert!(error.contains("readiness interrupted"));
        assert!(error.contains("samples=2"));
        assert!(!hid.get());
    }

    #[test]
    fn direct_helper_target_never_gets_the_no_ax_window_bypass() {
        let source = crate::transient_ui::WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let helper = crate::transient_ui::WindowTarget {
            pid: 20,
            window_id: 200,
        };
        assert!(!transient_route_authorizes_auxiliary_bypass(None, helper));
        assert!(!transient_route_authorizes_auxiliary_bypass(
            Some(crate::transient_ui::TransientRoute {
                source,
                target: crate::transient_ui::WindowTarget {
                    pid: 30,
                    window_id: 300,
                },
            }),
            helper,
        ));
        assert!(transient_route_authorizes_auxiliary_bypass(
            Some(crate::transient_ui::TransientRoute {
                source,
                target: helper,
            }),
            helper,
        ));
    }

    #[test]
    fn foreground_keyboard_focus_click_is_blender_unaddressed_only() {
        assert!(foreground_keyboard_focus_click_for_bundle_id(Some(
            "org.blenderfoundation.blender"
        )));
        assert!(!foreground_keyboard_focus_click_for_bundle_id(Some(
            "org.blenderfoundation.Blender"
        )));
        assert!(!foreground_keyboard_focus_click_for_bundle_id(Some(
            "com.apple.ScreenSharing"
        )));
        assert!(!foreground_keyboard_focus_click_for_bundle_id(Some(
            "com.apple.TextEdit"
        )));
        assert!(!foreground_keyboard_focus_click_for_bundle_id(None));

        assert!(foreground_keyboard_focus_click_policy(
            Some("org.blenderfoundation.blender"),
            false,
        ));
        assert!(!foreground_keyboard_focus_click_policy(
            Some("org.blenderfoundation.blender"),
            true,
        ));
        assert!(!foreground_keyboard_focus_click_policy(
            Some("com.apple.ScreenSharing"),
            false,
        ));
        assert!(!foreground_keyboard_focus_click_policy(
            Some("com.apple.TextEdit"),
            false,
        ));
    }

    #[test]
    fn synthetic_focus_plan_addresses_only_the_target() {
        let target_psn = [1, 2, 3, 4, 5, 6, 7, 8];
        let plan = synthetic_target_focus_plan(target_psn, 0x7856_3412);

        assert_eq!(plan.activate_target.psn, target_psn);
        assert_eq!(plan.deactivate_target.psn, target_psn);
        assert_eq!(plan.activate_target.window_id, 0x7856_3412);
        assert_eq!(plan.deactivate_target.window_id, 0x7856_3412);
        assert!(plan.activate_target.focused);
        assert!(!plan.deactivate_target.focused);
    }

    #[test]
    fn synthetic_focus_records_encode_exact_window_and_transition() {
        let activate = synthetic_focus_record(0x7856_3412, true);
        let deactivate = synthetic_focus_record(0x7856_3412, false);

        assert_eq!(activate.len(), 0xF8);
        assert_eq!(activate[0x04], 0xF8);
        assert_eq!(activate[0x08], 0x0D);
        assert_eq!(&activate[0x3C..0x40], &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(activate[0x8A], 0x01);
        assert_eq!(deactivate[0x8A], 0x02);
    }

    #[test]
    fn synthetic_focus_cleanup_preserves_a_real_user_takeover() {
        let target = [1, 2, 3, 4, 5, 6, 7, 8];
        let other = [8, 7, 6, 5, 4, 3, 2, 1];

        assert!(!should_deactivate_synthetic_target(Some(target), target));
        assert!(should_deactivate_synthetic_target(Some(other), target));
        assert!(
            !should_deactivate_synthetic_target(None, target),
            "an unknown real foreground must fail safe for user intervention"
        );
    }

    #[test]
    fn foreground_restore_is_compare_and_swap() {
        let target = [1, 2, 3, 4, 5, 6, 7, 8];
        let user_takeover = [8, 7, 6, 5, 4, 3, 2, 1];

        assert!(should_restore_previous_process(Some(target), target));
        assert!(!should_restore_previous_process(
            Some(user_takeover),
            target
        ));
        assert!(!should_restore_previous_process(None, target));
    }

    #[test]
    fn temporary_activation_hold_requires_a_different_known_foreground() {
        let target = [1; 8];
        let previous = [2; 8];

        assert!(!temporary_activation_moves_foreground(None, &[target]));
        assert!(!temporary_activation_moves_foreground(
            Some(target),
            &[target]
        ));
        assert!(temporary_activation_moves_foreground(
            Some(previous),
            &[target]
        ));
    }

    #[test]
    fn delegated_activation_hold_excludes_current_host_or_helper() {
        let host = [1; 8];
        let helper = [2; 8];
        let previous = [3; 8];
        let targets = [host, helper];

        assert!(!temporary_activation_moves_foreground(Some(host), &targets));
        assert!(!temporary_activation_moves_foreground(
            Some(helper),
            &targets
        ));
        assert!(temporary_activation_moves_foreground(
            Some(previous),
            &targets
        ));
    }

    #[test]
    fn exact_window_readiness_requires_front_process_and_ax_window() {
        let target = [1, 2, 3, 4, 5, 6, 7, 8];
        let other = [8, 7, 6, 5, 4, 3, 2, 1];

        assert!(exact_window_is_ready(Some(target), target, Some(42), 42));
        assert!(!exact_window_is_ready(Some(other), target, Some(42), 42));
        assert!(!exact_window_is_ready(Some(target), target, Some(41), 42));
        assert!(!exact_window_is_ready(None, target, Some(42), 42));
    }

    #[test]
    fn restore_transition_distinguishes_completion_pending_and_takeover() {
        let target = [1, 2, 3, 4, 5, 6, 7, 8];
        let previous = [8, 7, 6, 5, 4, 3, 2, 1];
        let takeover = [9, 9, 9, 9, 9, 9, 9, 9];

        assert_eq!(
            restore_transition(Some(previous), &[target], previous),
            RestoreTransition::Restored
        );
        assert_eq!(
            restore_transition(Some(target), &[target], previous),
            RestoreTransition::Pending
        );
        assert_eq!(
            restore_transition(None, &[target], previous),
            RestoreTransition::Pending
        );
        assert_eq!(
            restore_transition(Some(takeover), &[target], previous),
            RestoreTransition::Superseded
        );
    }

    #[test]
    fn delegated_restore_waits_for_host_and_helper_but_yields_to_takeover() {
        let host = [1; 8];
        let helper = [2; 8];
        let previous = [3; 8];
        let takeover = [4; 8];
        let targets = [host, helper];

        for pending in [Some(host), Some(helper), None] {
            assert_eq!(
                restore_transition(pending, &targets, previous),
                RestoreTransition::Pending
            );
        }
        assert_eq!(
            restore_transition(Some(previous), &targets, previous),
            RestoreTransition::Restored
        );
        assert_eq!(
            restore_transition(Some(takeover), &targets, previous),
            RestoreTransition::Superseded
        );
    }

    #[test]
    fn restore_completion_requires_workspace_confirmation() {
        let target = [1; 8];
        let previous = [2; 8];
        let takeover = [3; 8];

        for (workspace, expected) in [
            (Some(previous), RestoreTransition::Restored),
            (Some(target), RestoreTransition::Pending),
            (None, RestoreTransition::Pending),
            (Some(takeover), RestoreTransition::Superseded),
        ] {
            assert_eq!(
                workspace_confirmed_restore_transition(Some(previous), &[target], previous, || {
                    workspace
                },),
                expected,
            );
        }
    }

    #[test]
    fn workspace_is_queried_only_after_window_server_restore() {
        let target = [1; 8];
        let previous = [2; 8];
        let takeover = [3; 8];

        for (window_server, expected) in [
            (Some(target), RestoreTransition::Pending),
            (None, RestoreTransition::Pending),
            (Some(takeover), RestoreTransition::Superseded),
        ] {
            assert_eq!(
                workspace_confirmed_restore_transition(
                    window_server,
                    &[target],
                    previous,
                    || panic!("workspace must not be queried before WindowServer restores"),
                ),
                expected,
            );
        }
    }

    #[test]
    fn delegated_workspace_restore_waits_for_host_and_helper() {
        let host = [1; 8];
        let helper = [2; 8];
        let previous = [3; 8];
        let targets = [host, helper];

        for workspace in [Some(host), Some(helper), None] {
            assert_eq!(
                workspace_confirmed_restore_transition(Some(previous), &targets, previous, || {
                    workspace
                },),
                RestoreTransition::Pending,
            );
        }
    }

    #[test]
    fn make_key_records_address_only_the_exact_window() {
        let press = make_key_window_record(0x7856_3412, 0x01);
        let release = make_key_window_record(0x7856_3412, 0x02);
        assert_eq!(press.len(), 0xF8);
        assert_eq!(press[0x04], 0xF8);
        assert_eq!(press[0x08], 0x01);
        assert_eq!(release[0x08], 0x02);
        assert_eq!(&press[0x3C..0x40], &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(press[0x3A], 0x10);
        assert!(press[0x20..0x30].iter().all(|byte| *byte == 0xFF));
    }

    #[test]
    fn exact_existing_focus_avoids_reactivation() {
        let psn = [1, 2, 3, 4, 5, 6, 7, 8];
        assert!(preserves_exact_existing_focus(true, psn, psn, Some(42), 42));
    }

    #[test]
    fn process_or_window_uncertainty_requires_guarded_activation() {
        let target = [1, 2, 3, 4, 5, 6, 7, 8];
        let other = [8, 7, 6, 5, 4, 3, 2, 1];
        assert!(!preserves_exact_existing_focus(
            false,
            target,
            target,
            Some(42),
            42
        ));
        assert!(!preserves_exact_existing_focus(
            true,
            other,
            target,
            Some(42),
            42
        ));
        assert!(!preserves_exact_existing_focus(
            true,
            target,
            target,
            Some(41),
            42
        ));
        assert!(!preserves_exact_existing_focus(
            true, target, target, None, 42
        ));
    }
}
