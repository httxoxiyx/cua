//! Native activity evidence. No permission requests, input contents or retry timer.
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use core_foundation::{
    base::TCFType,
    runloop::{kCFRunLoopDefaultMode, CFRunLoop},
};
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, EventField};
use cua_driver_core::foreground_activity::{Activity, Snapshot, Source, State};
use cua_driver_core::tool::{ProtectedResourceOwnership, Tool, ToolDef};
use std::cell::Cell;

/// Runtime registration guard. Preserve all authorization attestations and
/// reject input whose reflex activation cannot be observed safely. A healthy
/// stream with recent external input still permits genuine background work.
pub(crate) fn guard_tool(inner: Box<dyn Tool>) -> Box<dyn Tool> {
    if matches!(
        inner.def().name.as_str(),
        "click"
            | "double_click"
            | "right_click"
            | "drag"
            | "scroll"
            | "move_cursor"
            | "type_text"
            | "press_key"
            | "hotkey"
            | "set_value"
            | "perform_secondary_action"
            | "invoke_menu"
            | "launch_app"
            | "bring_to_front"
            | "set_window_frame"
    ) {
        Box::new(ActivityGuardedTool { inner })
    } else {
        inner
    }
}

struct ActivityGuardedTool {
    inner: Box<dyn Tool>,
}

#[async_trait::async_trait]
impl Tool for ActivityGuardedTool {
    fn def(&self) -> &ToolDef {
        self.inner.def()
    }
    async fn protected_resource_ownership(
        &self,
        adapter: &str,
        args: &serde_json::Value,
    ) -> ProtectedResourceOwnership {
        self.inner.protected_resource_ownership(adapter, args).await
    }
    async fn protected_resource_scope(
        &self,
        adapter: &str,
        args: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, String> {
        self.inner.protected_resource_scope(adapter, args).await
    }
    async fn validate_protected_resource_scope(
        &self,
        adapter: &str,
        args: &serde_json::Value,
        scope: &serde_json::Value,
    ) -> Result<(), String> {
        self.inner
            .validate_protected_resource_scope(adapter, args, scope)
            .await
    }
    async fn invoke(&self, args: serde_json::Value) -> cua_driver_core::protocol::ToolResult {
        if self.def().name == "move_cursor"
            && args.get("scope").and_then(serde_json::Value::as_str) != Some("desktop")
        {
            return self.inner.invoke(args).await;
        }
        let unsupported = matches!(self.def().name.as_str(), "bring_to_front" | "invoke_menu")
            || args
                .get("scope")
                .or_else(|| args.get("capture_scope"))
                .and_then(serde_json::Value::as_str)
                == Some("desktop");
        if unsupported || !snapshot().reliable {
            return cua_driver_core::protocol::ToolResult::error(
                "Native activity coverage or a bounded foreground episode is unavailable; no input was dispatched.")
                .with_structured(serde_json::json!({
                    "code": "foreground_activity_unavailable", "effect": "refused", "retryable": false,
                }));
        }
        self.inner.invoke(args).await
    }
}

#[derive(Clone, Copy)]
struct Lease {
    generation: u64,
    pid: i32,
    window: u32,
    started: u64,
}
thread_local! { static LEASE: Cell<Option<Lease>> = const { Cell::new(None) }; }

/// One synchronous native action. Drop never changes focus; only normal
/// completion may restore the exact original window.
pub(crate) struct Episode {
    original: Option<(i32, u32)>,
    lease: Lease,
    _writer: std::sync::MutexGuard<'static, ()>,
}

impl Episode {
    pub(crate) fn begin(pid: i32, window: u32) -> anyhow::Result<Self> {
        static WRITER: Mutex<()> = Mutex::new(());
        let writer = WRITER
            .try_lock()
            .map_err(|_| anyhow::anyhow!("foreground input is already in progress"))?;
        let generation = require_idle()?;
        if !matches!(
            crate::windows::resolve_window_owner(pid, window),
            crate::windows::WindowOwner::SamePid
        ) {
            anyhow::bail!("foreground target ownership is unavailable");
        }
        let original = crate::apps::frontmost_pid().and_then(|pid| {
            let window = crate::ax::bindings::focused_window_id_of_pid(pid)?;
            matches!(
                crate::windows::resolve_window_owner(pid, window),
                crate::windows::WindowOwner::SamePid
            )
            .then_some((pid, window))
        });
        let lease = Lease {
            generation,
            pid,
            window,
            started: clock_ms(),
        };
        LEASE.with(|slot| slot.set(Some(lease)));
        Ok(Self {
            original,
            lease,
            _writer: writer,
        })
    }

    pub(crate) fn check(&self) -> anyhow::Result<()> {
        check_activity(self.lease)
    }

    pub(crate) fn finish<T>(self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        let value = result?;
        self.check()?;
        if let Some((pid, window)) = self.original {
            if (pid, window) != (self.lease.pid, self.lease.window)
                && exact_target_is_frontmost(self.lease)
                && matches!(
                    crate::windows::resolve_window_owner(pid, window),
                    crate::windows::WindowOwner::SamePid
                )
            {
                self.check()?;
                if !crate::input::skylight::restore_exact_window(pid, window) {
                    anyhow::bail!(
                        "foreground action completed but exact original-window restoration failed"
                    );
                }
            }
        }
        Ok(value)
    }
}

impl Drop for Episode {
    fn drop(&mut self) {
        LEASE.with(|slot| slot.set(None));
    }
}

fn check_activity(lease: Lease) -> anyhow::Result<()> {
    if !generation_is_current(lease.generation)
        || clock_ms().saturating_sub(lease.started) >= 120_000
    {
        anyhow::bail!(
            "foreground_activity_interrupted: stop task input; observe before continuing"
        );
    }
    Ok(())
}

fn exact_target_is_frontmost(lease: Lease) -> bool {
    crate::apps::frontmost_pid() == Some(lease.pid)
        && crate::ax::bindings::focused_window_id_of_pid(lease.pid) == Some(lease.window)
}

pub(crate) fn check_input() -> anyhow::Result<()> {
    let lease = LEASE
        .with(Cell::get)
        .ok_or_else(|| anyhow::anyhow!("bounded foreground episode is required"))?;
    check_activity(lease)?;
    if !exact_target_is_frontmost(lease) {
        anyhow::bail!("foreground target changed; stop input and observe again");
    }
    Ok(())
}

pub(crate) fn check_if_foreground_episode() -> anyhow::Result<()> {
    if LEASE.with(Cell::get).is_some() {
        check_input()?;
    }
    Ok(())
}

pub(crate) fn check_targeted_input(pid: i32) -> anyhow::Result<()> {
    if LEASE.with(Cell::get).is_some() {
        return check_input();
    }
    if crate::apps::frontmost_pid().is_some_and(|front| front != pid) {
        return Ok(());
    }
    require_idle().map(|_| ())
}

/// Releases bypass the activity check, but are still attributed as generated
/// input. Callers must release only controls their operation actually pressed.
pub(crate) fn post_global(event: &CGEvent, release: bool) -> anyhow::Result<()> {
    if !release {
        check_input()?;
    }
    mark_generated(event);
    event.post(CGEventTapLocation::HID);
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RestoreEvidence {
    generation: u64,
    pid: i32,
    window: u32,
}

pub(crate) fn capture_restore(pid: i32) -> Option<RestoreEvidence> {
    if crate::apps::frontmost_pid() != Some(pid) {
        return None;
    }
    let current = snapshot();
    if !current.reliable {
        return None;
    }
    let generation = current.generation;
    let window = crate::ax::bindings::focused_window_id_of_pid(pid)?;
    if !matches!(
        crate::windows::resolve_window_owner(pid, window),
        crate::windows::WindowOwner::SamePid
    ) {
        return None;
    }
    Some(RestoreEvidence {
        generation,
        pid,
        window,
    })
}

pub(crate) fn restore_background_focus(evidence: RestoreEvidence, expected_pid: i32) {
    if expected_pid == evidence.pid
        && {
            let current = snapshot();
            current.reliable && current.generation == evidence.generation
        }
        && matches!(
            crate::windows::resolve_window_owner(evidence.pid, evidence.window),
            crate::windows::WindowOwner::SamePid
        )
    {
        let _ = crate::input::skylight::submit_exact_window_restore(evidence.pid, evidence.window);
    }
}

fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn activity() -> &'static Mutex<Activity> {
    static ACTIVITY: OnceLock<Mutex<Activity>> = OnceLock::new();
    ACTIVITY.get_or_init(|| Mutex::new(Activity::default()))
}

fn cookie() -> i64 {
    static COOKIE: OnceLock<i64> = OnceLock::new();
    *COOKIE.get_or_init(|| uuid::Uuid::new_v4().as_u128() as i64)
}

pub(crate) fn mark_generated(event: &CGEvent) {
    event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, cookie());
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> core_foundation::dictionary::CFDictionaryRef;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGEventTapCreate(
        point: u32,
        placement: u32,
        options: u32,
        mask: u64,
        callback: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            u32,
            core_graphics::sys::CGEventRef,
            *mut std::ffi::c_void,
        ) -> core_graphics::sys::CGEventRef,
        user_info: *mut std::ffi::c_void,
    ) -> core_foundation::mach_port::CFMachPortRef;
    fn CGEventGetIntegerValueField(event: *const std::ffi::c_void, field: u32) -> i64;
    fn CGEventTapEnable(port: core_foundation::mach_port::CFMachPortRef, enabled: bool);
    fn CGEventTapIsEnabled(port: core_foundation::mach_port::CFMachPortRef) -> bool;
    fn CGGetEventTapList(max: u32, taps: *mut TapInfo, count: *mut u32) -> i32;
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

// CGEventTapInformation, including the effective mask (the OS may remove
// keyboard events when the process lacks the existing listening permission).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TapInfo {
    id: u32,
    point: u32,
    options: u32,
    mask: u64,
    tapping_pid: i32,
    target_pid: i32,
    enabled: bool,
    min_latency: f32,
    avg_latency: f32,
    max_latency: f32,
}

fn effective_mask_is_complete(mask: u64) -> bool {
    let mut count = 0;
    if unsafe { CGGetEventTapList(0, std::ptr::null_mut(), &mut count) } != 0
        || count == 0
        || count > 4096
    {
        return false;
    }
    let mut taps = vec![TapInfo::default(); count as usize];
    if unsafe { CGGetEventTapList(count, taps.as_mut_ptr(), &mut count) } != 0 {
        return false;
    }
    taps.iter().take(count as usize).any(|tap| {
        tap.tapping_pid == std::process::id() as i32
            && tap.point == 1
            && tap.options == 1
            && tap.enabled
            && tap.mask & mask == mask
    })
}

fn active_console_session() -> bool {
    use core_foundation::{
        base::{CFGetTypeID, CFType},
        boolean::{CFBoolean, CFBooleanGetTypeID},
        dictionary::{CFDictionary, CFDictionaryGetValue},
        string::CFString,
    };
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        return false;
    }
    let session: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_create_rule(raw) };
    let flag = |key: &str| -> Option<bool> {
        let key = CFString::new(key);
        let value = unsafe {
            CFDictionaryGetValue(
                session.as_concrete_TypeRef(),
                key.as_concrete_TypeRef().cast(),
            )
        };
        if value.is_null() || unsafe { CFGetTypeID(value) != CFBooleanGetTypeID() } {
            return None;
        }
        Some(bool::from(unsafe {
            CFBoolean::wrap_under_get_rule(value.cast())
        }))
    };
    console_flags_allow_monitor(flag)
}

fn console_flags_allow_monitor(flag: impl Fn(&str) -> Option<bool>) -> bool {
    // The SDK's kCGSessionOnConsoleKey macro uses "CGSSession" in its value.
    flag("kCGSSessionOnConsoleKey") == Some(true)
        && flag("kCGSessionLoginDoneKey") == Some(true)
        && flag("CGSSessionScreenIsLocked") != Some(true)
}

#[cfg(test)]
mod console_session_tests {
    use super::console_flags_allow_monitor;
    use std::collections::HashMap;

    fn logged_in_console() -> HashMap<&'static str, bool> {
        // CGSession.h's public kCGSessionOnConsoleKey macro expands to this
        // dictionary key, which differs from the macro's own name.
        HashMap::from([
            ("kCGSSessionOnConsoleKey", true),
            ("kCGSessionLoginDoneKey", true),
        ])
    }

    #[test]
    fn sdk_console_dictionary_admits_monitoring() {
        let flags = logged_in_console();
        assert!(console_flags_allow_monitor(|key| flags.get(key).copied()));
    }

    #[test]
    fn locked_logged_out_or_missing_console_remains_unavailable() {
        for (key, value) in [
            ("kCGSSessionOnConsoleKey", Some(false)),
            ("kCGSSessionOnConsoleKey", None),
            ("kCGSessionLoginDoneKey", Some(false)),
            ("kCGSessionLoginDoneKey", None),
            ("CGSSessionScreenIsLocked", Some(true)),
        ] {
            let mut flags = logged_in_console();
            if let Some(value) = value {
                flags.insert(key, value);
            } else {
                flags.remove(key);
            }
            assert!(!console_flags_allow_monitor(|key| flags.get(key).copied()));
        }
    }
}

fn environment_is_reliable() -> bool {
    crate::session::has_graphic_access()
        && active_console_session()
        && unsafe { CGPreflightListenEventAccess() && !IsSecureEventInputEnabled() }
}

// Handle disabled/null control notifications before touching a CGEvent.
// The callback owns no heap closure; all content-free state has process lifetime.
unsafe extern "C" fn observe_event(
    _: *mut std::ffi::c_void,
    kind: u32,
    event: core_graphics::sys::CGEventRef,
    _: *mut std::ffi::c_void,
) -> core_graphics::sys::CGEventRef {
    let mut state = activity().lock().unwrap_or_else(|e| e.into_inner());
    if event.is_null() || kind == u32::MAX || kind == u32::MAX - 1 {
        state.invalidate();
    } else {
        let own =
            CGEventGetIntegerValueField(event.cast(), EventField::EVENT_SOURCE_UNIX_PROCESS_ID)
                == std::process::id() as i64
                && CGEventGetIntegerValueField(event.cast(), EventField::EVENT_SOURCE_USER_DATA)
                    == cookie();
        state.event(
            clock_ms(),
            if own {
                Source::OwnGenerated
            } else {
                Source::Unknown
            },
        );
    }
    event
}

struct RegisteredTap {
    port: core_foundation::mach_port::CFMachPort,
    source: core_foundation::runloop::CFRunLoopSource,
    run_loop: CFRunLoop,
}
impl Drop for RegisteredTap {
    fn drop(&mut self) {
        unsafe {
            CGEventTapEnable(self.port.as_concrete_TypeRef(), false);
            self.run_loop
                .remove_source(&self.source, kCFRunLoopDefaultMode);
            core_foundation::mach_port::CFMachPortInvalidate(self.port.as_concrete_TypeRef());
        }
        activity()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate();
    }
}

fn start_monitor() {
    static START: Once = Once::new();
    START.call_once(|| {
        // Permission changes require a new process. Do not request a grant or
        // continuously retry a denied monitor in the background.
        if !crate::session::has_graphic_access() || !unsafe { CGPreflightListenEventAccess() } {
            return;
        }
        let _ = std::thread::Builder::new()
            .name("foreground-activity".into())
            .spawn(|| {
                // Secure Input at first use is a temporary coverage gap, not a
                // consumed one-shot initialization attempt. No input is retried.
                while !environment_is_reliable() {
                    std::thread::sleep(Duration::from_millis(100));
                }
                use CGEventType::*;
                let events = vec![
                    LeftMouseDown,
                    LeftMouseUp,
                    RightMouseDown,
                    RightMouseUp,
                    MouseMoved,
                    LeftMouseDragged,
                    RightMouseDragged,
                    KeyDown,
                    KeyUp,
                    FlagsChanged,
                    ScrollWheel,
                    TabletPointer,
                    TabletProximity,
                    OtherMouseDown,
                    OtherMouseUp,
                    OtherMouseDragged,
                ];
                let mask = events
                    .iter()
                    .fold(0_u64, |mask, event| mask | (1_u64 << *event as u32));
                let port =
                    unsafe { CGEventTapCreate(1, 0, 1, mask, observe_event, std::ptr::null_mut()) };
                if port.is_null() {
                    return;
                }
                let tap =
                    unsafe { core_foundation::mach_port::CFMachPort::wrap_under_create_rule(port) };
                let Ok(source) = tap.create_runloop_source(0) else {
                    unsafe {
                        core_foundation::mach_port::CFMachPortInvalidate(port);
                    }
                    return;
                };
                let run_loop = CFRunLoop::get_current();
                unsafe {
                    run_loop.add_source(&source, kCFRunLoopDefaultMode);
                    CGEventTapEnable(port, true);
                }
                let _registration = RegisteredTap {
                    port: tap,
                    source,
                    run_loop,
                };
                loop {
                    let environment_ready = environment_is_reliable();
                    if environment_ready && !unsafe { CGEventTapIsEnabled(port) } {
                        activity()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .invalidate();
                        unsafe {
                            CGEventTapEnable(port, true);
                        }
                    }
                    let reliable = environment_ready
                        && unsafe { CGEventTapIsEnabled(port) }
                        && effective_mask_is_complete(mask);
                    activity()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .health(clock_ms(), reliable);
                    unsafe {
                        CFRunLoop::run_in_mode(
                            kCFRunLoopDefaultMode,
                            Duration::from_millis(100),
                            false,
                        );
                    }
                }
            });
    });
}

pub(crate) fn snapshot() -> Snapshot {
    start_monitor();
    let environment_ready = environment_is_reliable();
    let mut state = activity().lock().unwrap_or_else(|e| e.into_inner());
    if !environment_ready {
        state.invalidate();
    }
    state.snapshot(clock_ms())
}

pub(crate) fn require_idle() -> anyhow::Result<u64> {
    let current = snapshot();
    if current.state != State::Idle {
        anyhow::bail!("foreground_activity_unavailable: five seconds of reliable native idle evidence required");
    }
    Ok(current.generation)
}

pub(crate) fn generation_is_current(generation: u64) -> bool {
    let current = snapshot();
    current.state == State::Idle && current.generation == generation
}

pub(crate) fn diagnostic_state() -> serde_json::Value {
    let current = snapshot();
    serde_json::json!({
        "contract_version": 1,
        "monitor": if current.reliable { "ready" } else { "unknown" },
        "state": match current.state { State::Idle => "idle", State::Active => "active", State::Unknown => "unknown" },
        "idle_ms": current.idle_ms,
        // Keep paired-wrapper automatic foreground disabled until the entire
        // dispatch/cleanup/restore boundary, including multi-RPC episodes, is
        // implemented and verified. Monitoring alone is not that capability.
        "native_dispatch_guard": false,
        "native_cleanup": false,
        "exact_window_restore": false,
        "batch_foreground_segments": false,
    })
}
