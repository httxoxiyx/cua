//! Native activity evidence. No permission requests, input contents or retry timer.
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, Once, OnceLock,
};
use std::time::{Duration, Instant};

use core_foundation::{
    base::TCFType,
    runloop::{kCFRunLoopDefaultMode, CFRunLoop},
};
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, EventField};
use cua_driver_core::foreground_activity::{
    Activity, EpisodeLease, InputControl, PressedInputs, Snapshot, Source, State,
};
use cua_driver_core::tool::{ProtectedResourceOwnership, Tool, ToolDef};
use foreign_types::ForeignType;
use std::cell::{Cell, RefCell};

mod segment;
pub(crate) use segment::{
    begin_segment, end_segment, stop_runtime_segments, stop_session_segments,
};

fn foreground_writer() -> Arc<tokio::sync::Mutex<()>> {
    static WRITER: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(WRITER.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
}

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
            | "get_window_state"
    ) {
        let mut def = inner.def().clone();
        def.input_schema["properties"]["foreground_segment_id"] = serde_json::json!({
            "type": "string", "minLength": 1, "maxLength": 128,
            "description": "Private native foreground segment token; requires the same canonical transport, session, runtime and exact PID/window that began it."
        });
        Box::new(ActivityGuardedTool { inner, def })
    } else {
        inner
    }
}

struct ActivityGuardedTool {
    inner: Box<dyn Tool>,
    def: ToolDef,
}

struct InvocationContext {
    cancelled: AtomicBool,
    interrupted: AtomicBool,
    cleanup_unconfirmed: AtomicBool,
    session_id: Option<String>,
    runtime_scope: Option<String>,
    foreground_admission: Option<EpisodeLease>,
    background_leases: Mutex<Vec<Arc<tokio::sync::OwnedMutexGuard<()>>>>,
    transport_owner: Option<Arc<cua_driver_core::session::TransportOwner>>,
    segment_call: Option<Arc<segment::Call>>,
    workers: AtomicUsize,
    invocation_done: AtomicBool,
}

impl InvocationContext {
    fn check(&self) -> anyhow::Result<()> {
        if self.cleanup_unconfirmed.load(Ordering::Acquire) {
            anyhow::bail!("native_cleanup_unconfirmed: stop task input until cleanup is confirmed");
        }
        if self.interrupted.load(Ordering::Acquire)
            || self.cancelled.load(Ordering::Acquire)
            || self
                .transport_owner
                .as_ref()
                .is_some_and(|owner| !owner.is_live())
            || self
                .session_id
                .as_deref()
                .is_some_and(cua_driver_core::session::is_session_ending_or_ended)
            || self
                .runtime_scope
                .as_deref()
                .is_some_and(cua_driver_core::session::is_runtime_scope_suspended)
        {
            self.interrupted.store(true, Ordering::Release);
            if let Some(call) = &self.segment_call {
                call.revoke();
            }
            anyhow::bail!(
                "foreground_activity_interrupted: native request or session ended; stop task input"
            );
        }
        if self
            .foreground_admission
            .is_some_and(|lease| !lease.permits(clock_ms(), snapshot()))
        {
            self.interrupted.store(true, Ordering::Release);
            if let Some(call) = &self.segment_call {
                call.revoke();
            }
            anyhow::bail!("foreground_activity_interrupted: native activity changed after foreground admission; stop task input");
        }
        if let Some(call) = &self.segment_call {
            if let Err(error) = call.check() {
                if call.cleanup_is_unknown() {
                    self.cleanup_unconfirmed.store(true, Ordering::Release);
                }
                self.interrupted.store(true, Ordering::Release);
                call.revoke();
                return Err(error);
            }
        }
        Ok(())
    }

    fn finish_invocation(&self) {
        self.invocation_done.store(true, Ordering::Release);
        self.settle_if_finished();
    }

    fn settle_if_finished(&self) {
        if self.invocation_done.load(Ordering::Acquire) && self.workers.load(Ordering::Acquire) == 0
        {
            if let Some(call) = &self.segment_call {
                call.settle();
            }
        }
    }

    fn project_native_result(&self, result: &mut cua_driver_core::protocol::ToolResult) {
        if let Some(summary) = self
            .segment_call
            .as_ref()
            .and_then(|call| call.closed_summary())
        {
            let structured = result
                .structured_content
                .get_or_insert_with(|| serde_json::json!({}));
            if !structured.is_object() {
                *structured = serde_json::json!({});
            }
            structured["foreground_segment"] = summary;
        }
        let cleanup_unconfirmed = self.cleanup_unconfirmed.load(Ordering::Acquire);
        if !cleanup_unconfirmed && !self.interrupted.load(Ordering::Acquire) {
            return;
        }
        // A normal ToolResult is otherwise a transport settlement signal. Do
        // not let cancellation mask a helper whose exit could not be proven.
        let (code, reason, message) = if cleanup_unconfirmed {
            (
                "native_cleanup_unconfirmed",
                "native_cleanup_unconfirmed",
                "Native cleanup could not be confirmed; an effect is possible. Stop this Computer Use flow; do not replay or reconnect automatically.",
            )
        } else {
            (
                "foreground_activity_interrupted",
                "native_interruption",
                "Native input was interrupted; an effect is possible. Stop this Computer Use flow and observe before continuing; do not replay.",
            )
        };
        result.is_error = Some(true);
        let mut structured = result
            .structured_content
            .take()
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        structured["code"] = serde_json::json!(code);
        structured["effect"] = serde_json::json!("unverifiable");
        structured["verified"] = serde_json::json!(false);
        structured["retryable"] = serde_json::json!(false);
        structured["foreground_failure"] = serde_json::json!({"reason": reason});
        result.structured_content = Some(structured);
        result.content = vec![cua_driver_core::protocol::Content::text(message)];
        if let Some(record) = result.action_record.as_mut() {
            record.effect = cua_driver_core::action_record::ActionEffect::Unverifiable;
        }
    }
}

tokio::task_local! {
    static INVOCATION: Arc<InvocationContext>;
}

thread_local! {
    static BLOCKING_INVOCATION: RefCell<Option<Arc<InvocationContext>>> = const { RefCell::new(None) };
}

struct CancelInvocation {
    context: Arc<InvocationContext>,
    completed: bool,
}
impl Drop for CancelInvocation {
    fn drop(&mut self) {
        if !self.completed {
            self.context.cancelled.store(true, Ordering::Release);
            if let Some(call) = &self.context.segment_call {
                call.revoke();
            }
        }
        self.context.finish_invocation();
    }
}

/// Tokio does not inherit task-locals in spawn_blocking, and dropping its join
/// future does not stop the native thread. Explicitly carry the private request
/// cancellation/owner context so event guards can stop that still-running work.
/// Never reconstruct this identity from a PID or a caller-provided token.
pub(crate) fn spawn_blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let context = INVOCATION
        .try_with(Arc::clone)
        .ok()
        .or_else(|| BLOCKING_INVOCATION.with(|current| current.borrow().clone()));
    struct Worker(Arc<InvocationContext>);
    impl Drop for Worker {
        fn drop(&mut self) {
            self.0.workers.fetch_sub(1, Ordering::AcqRel);
            self.0.settle_if_finished();
        }
    }
    let worker = context.as_ref().map(|context| {
        context.workers.fetch_add(1, Ordering::AcqRel);
        Worker(Arc::clone(context))
    });
    tokio::task::spawn_blocking(move || {
        let _worker = worker;
        struct RestoreContext(Option<Arc<InvocationContext>>);
        impl Drop for RestoreContext {
            fn drop(&mut self) {
                BLOCKING_INVOCATION.with(|current| current.replace(self.0.take()));
            }
        }
        let previous = BLOCKING_INVOCATION.with(|current| current.replace(context));
        let _restore = RestoreContext(previous);
        work()
    })
}

fn current_invocation() -> Option<Arc<InvocationContext>> {
    BLOCKING_INVOCATION
        .with(|current| current.borrow().clone())
        .or_else(|| INVOCATION.try_with(Arc::clone).ok())
}

fn check_invocation(required: bool) -> anyhow::Result<()> {
    match current_invocation() {
        Some(context) => {
            if required
                && (!context
                    .session_id
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                    || !context
                        .runtime_scope
                        .as_deref()
                        .is_some_and(|value| !value.is_empty()))
            {
                anyhow::bail!(
                    "foreground_activity_unavailable: canonical native owner is unavailable"
                );
            }
            context.check()
        }
        None if required => anyhow::bail!(
            "foreground_activity_unavailable: trusted native request lifecycle is unavailable"
        ),
        None => Ok(()),
    }
}

fn record_interruption() {
    if let Some(context) = current_invocation() {
        context.interrupted.store(true, Ordering::Release);
        if let Some(call) = &context.segment_call {
            call.revoke();
        }
    }
}

/// A native-owned helper failed to prove it has stopped. Keep this sticky even
/// if a later Drop retry succeeds: callers must not infer safe settlement from
/// the operation's ordinary error response or automatically reconnect/replay.
pub(crate) fn mark_native_cleanup_unconfirmed() {
    if let Some(context) = current_invocation() {
        context.cleanup_unconfirmed.store(true, Ordering::Release);
        if let Some(call) = &context.segment_call {
            call.cleanup_unknown();
        }
    }
}

pub(crate) fn current_segment_background_lease(
    pid: i32,
) -> Option<Arc<tokio::sync::OwnedMutexGuard<()>>> {
    current_invocation().and_then(|context| {
        context
            .segment_call
            .as_ref()
            .and_then(|call| call.background_lease(pid))
    })
}

pub(crate) fn retain_background_lease(lease: Arc<tokio::sync::OwnedMutexGuard<()>>) {
    let context = INVOCATION
        .try_with(Arc::clone)
        .ok()
        .or_else(|| BLOCKING_INVOCATION.with(|current| current.borrow().clone()));
    if let Some(context) = context {
        context
            .background_leases
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(lease);
    }
}

/// Use immediately before non-CGEvent preparation writes (for example AX
/// focus). Ordinary background work is not stopped merely by user activity;
/// explicit foreground requests keep their original admitted generation.
pub(crate) fn check_request() -> anyhow::Result<()> {
    if LEASE.with(Cell::get).is_some() {
        check_input()
    } else {
        check_invocation(false)
    }
}

/// A segment's opaque token never authorizes selecting a different native
/// target, including trusted helper/delegation routes. Check after any target
/// resolution and before preparation or input; tokenless behavior is unchanged.
pub(crate) fn check_segment_target(
    pid: i32,
    window_id: Option<u32>,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    if let Some(call) = current_invocation().and_then(|context| context.segment_call.clone()) {
        let target = call.target();
        if target.pid != pid || Some(target.window_id) != window_id {
            return Err(cua_driver_core::protocol::ToolResult::error(
                "A foreground segment cannot redirect input or observation to another native target; no action was sent.")
                .with_structured(serde_json::json!({"code":"foreground_segment_target_mismatch", "effect":"refused", "retryable":false})));
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl Tool for ActivityGuardedTool {
    fn def(&self) -> &ToolDef {
        &self.def
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
        let segment_call = match segment::admit_call(&args, &self.def().name) {
            Ok(call) => call,
            Err(result) => return result,
        };
        if self.def().name == "get_window_state" && segment_call.is_none() {
            return self.inner.invoke(args).await;
        }
        if self.def().name == "move_cursor"
            && args.get("scope").and_then(serde_json::Value::as_str) != Some("desktop")
            && segment_call.is_none()
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
            if let Some(call) = &segment_call {
                call.revoke();
                call.settle();
            }
            return cua_driver_core::protocol::ToolResult::error(
                "Native activity coverage or a bounded foreground episode is unavailable; no input was dispatched.")
                .with_structured(serde_json::json!({
                    "code": "foreground_activity_unavailable", "effect": "refused", "retryable": false,
                }));
        }
        let foreground = crate::tools::DeliveryMode::parse(
            args.get("delivery_mode")
                .and_then(serde_json::Value::as_str),
        )
        .is_foreground();
        let session_id = args
            .get("_session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let runtime_scope = cua_driver_core::tool::current_dispatch_runtime_scope();
        let transport_owner = args
            .get("_transport_session_id")
            .and_then(serde_json::Value::as_str)
            .and_then(cua_driver_core::session::current_transport_owner_for);
        let foreground_pid = if foreground {
            let pid = args
                .get("pid")
                .and_then(serde_json::Value::as_i64)
                .and_then(|pid| i32::try_from(pid).ok())
                .filter(|pid| *pid > 0);
            if pid.is_none()
                || !session_id.as_deref().is_some_and(|value| !value.is_empty())
                || !runtime_scope
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
            {
                return cua_driver_core::protocol::ToolResult::error("Canonical foreground owner and exact process are required; no input was dispatched.")
                    .with_structured(serde_json::json!({"code": "foreground_activity_unavailable", "effect": "refused", "retryable": false}));
            }
            pid
        } else {
            None
        };
        // Serialize foreground with background work for this same process.
        // Other processes remain independent. Acquire before idle evidence so
        // a queued request cannot reuse evidence from before its wait.
        let foreground_guard = if let Some(pid) = foreground_pid.filter(|pid| {
            segment_call.is_none() && !crate::background_mutation::held_by_current_task(*pid)
        }) {
            Some(Arc::new(crate::background_mutation::acquire(pid).await))
        } else {
            None
        };
        let foreground_admission = if let Some(call) = &segment_call {
            Some(call.activity_lease())
        } else if foreground {
            let Some(lease) = EpisodeLease::begin(clock_ms(), snapshot()) else {
                return cua_driver_core::protocol::ToolResult::error(
                    "Foreground input requires five seconds of reliable native idle evidence; no input was dispatched.")
                    .with_structured(serde_json::json!({
                        "code": "foreground_activity_unavailable", "effect": "refused", "retryable": false,
                    }));
            };
            Some(lease)
        } else {
            None
        };
        let mut retained_leases = current_invocation()
            .map(|context| {
                context
                    .background_leases
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            })
            .unwrap_or_default();
        retained_leases.extend(foreground_guard);
        let context = Arc::new(InvocationContext {
            cancelled: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            cleanup_unconfirmed: AtomicBool::new(false),
            // _session_id is injected by the canonical registry, unlike the
            // caller's optional public `session` display label.
            session_id,
            runtime_scope,
            foreground_admission,
            background_leases: Mutex::new(retained_leases),
            transport_owner,
            segment_call,
            workers: AtomicUsize::new(0),
            invocation_done: AtomicBool::new(false),
        });
        let mut cancel_on_exit = CancelInvocation {
            context: Arc::clone(&context),
            completed: false,
        };
        let held_pid = context
            .segment_call
            .as_ref()
            .map(|call| call.target().pid)
            .or(foreground_pid);
        let mut result = INVOCATION
            .scope(Arc::clone(&context), async {
                if let Err(error) = context.check() {
                    return cua_driver_core::protocol::ToolResult::error(error.to_string());
                }
                if let Some(pid) = held_pid {
                    crate::background_mutation::with_held_lease(pid, self.inner.invoke(args)).await
                } else {
                    self.inner.invoke(args).await
                }
            })
            .await;
        let _ = context.check();
        cancel_on_exit.completed = true;
        context.finish_invocation();
        if context.workers.load(Ordering::Acquire) != 0 {
            context.cleanup_unconfirmed.store(true, Ordering::Release);
            if let Some(call) = &context.segment_call {
                call.cleanup_unknown();
            }
        }
        context.project_native_result(&mut result);
        result
    }
}

#[derive(Clone, Copy)]
struct Lease {
    evidence: EpisodeLease,
    pid: i32,
    window: u32,
}
thread_local! {
    static LEASE: Cell<Option<Lease>> = const { Cell::new(None) };
    static PRESSED: RefCell<PressedInputs<CGEvent>> = RefCell::new(PressedInputs::default());
}

/// One synchronous native action. Drop never changes focus; only normal
/// completion may restore the exact original window.
pub(crate) struct Episode {
    original: Option<(i32, u32)>,
    lease: Lease,
    _writer: Option<tokio::sync::OwnedMutexGuard<()>>,
    segment: Option<Arc<segment::Call>>,
}

impl Episode {
    pub(crate) fn begin(pid: i32, window: u32) -> anyhow::Result<Self> {
        check_invocation(true)?;
        let segment = current_invocation().and_then(|context| context.segment_call.clone());
        if let Some(call) = &segment {
            call.check()?;
            if call.target().pid != pid || call.target().window_id != window {
                anyhow::bail!("foreground segment exact target mismatch");
            }
        }
        let writer = if segment.is_none() {
            Some(
                foreground_writer()
                    .try_lock_owned()
                    .map_err(|_| anyhow::anyhow!("foreground input is already in progress"))?,
            )
        } else {
            None
        };
        let evidence = segment.as_ref().map(|call| call.activity_lease()).or_else(|| EpisodeLease::begin(clock_ms(), snapshot())).ok_or_else(|| {
            anyhow::anyhow!("foreground_activity_unavailable: five seconds of reliable native idle evidence required")
        })?;
        if !matches!(
            crate::windows::resolve_window_owner(pid, window),
            crate::windows::WindowOwner::SamePid
        ) {
            anyhow::bail!("foreground target ownership is unavailable");
        }
        let original = if segment.is_some() {
            None
        } else {
            crate::apps::frontmost_pid().and_then(|pid| {
                let window = crate::ax::bindings::focused_window_id_of_pid(pid)?;
                (crate::input::skylight::front_process_matches(pid, window) == Some(true)
                    && matches!(
                        crate::windows::resolve_window_owner(pid, window),
                        crate::windows::WindowOwner::SamePid
                    ))
                .then_some((pid, window))
            })
        };
        // An unknown original identity cannot support exact restoration. Do
        // not activate first and only discover this missing evidence later.
        if original.is_none() && segment.is_none() {
            anyhow::bail!("foreground_activity_unavailable: exact original window is unavailable");
        }
        let lease = Lease {
            evidence,
            pid,
            window,
        };
        check_activity(lease)?;
        if LEASE.with(Cell::get).is_some() || !PRESSED.with(|held| held.borrow().is_empty()) {
            anyhow::bail!(
                "foreground_activity_unavailable: nested or unsettled foreground episode"
            );
        }
        LEASE.with(|slot| slot.set(Some(lease)));
        // From this point an attempted activation owns the target expectation,
        // even if a native activation returns an error before input. Do not
        // revive the original pre-activation state for a later segment call.
        if let Some(call) = &segment {
            call.mark_activated();
        }
        Ok(Self {
            original,
            lease,
            _writer: writer,
            segment,
        })
    }

    pub(crate) fn check(&self) -> anyhow::Result<()> {
        check_activity(self.lease)
    }

    pub(crate) fn finish<T>(self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        let had_pressed_controls = release_owned_inputs();
        // An ordinary action error does not imply human intervention. Settle
        // owned controls and restore safely when evidence still permits it,
        // then preserve the original action error. Cancellation, a generation
        // change, and Drop/unwind never enter focus restoration.
        self.check()?;
        if let Some(call) = &self.segment {
            if exact_target_is_frontmost(self.lease) {
                call.mark_activated();
            } else {
                record_interruption();
                anyhow::bail!("foreground segment target changed after input; stop further calls");
            }
            if had_pressed_controls && result.is_ok() {
                anyhow::bail!(
                    "foreground segment call left held controls; owned cleanup completed"
                );
            }
            return result;
        }
        if let Some((pid, window)) = self.original {
            if (pid, window) != (self.lease.pid, self.lease.window)
                && exact_target_is_frontmost(self.lease)
                && matches!(
                    crate::windows::resolve_window_owner(pid, window),
                    crate::windows::WindowOwner::SamePid
                )
            {
                self.check()?;
                if !crate::input::skylight::restore_exact_window_guarded(pid, window, || {
                    self.check()
                }) {
                    self.check()?;
                    anyhow::bail!(
                        "foreground input settled but exact original-window restoration failed"
                    );
                }
            }
        }
        if had_pressed_controls && result.is_ok() {
            anyhow::bail!("foreground input left held controls; owned cleanup completed");
        }
        result
    }
}

impl Drop for Episode {
    fn drop(&mut self) {
        // Also runs when an operation returns an error or unwinds. Never move
        // focus here: a user's intervention must retain its resulting focus.
        release_owned_inputs();
        LEASE.with(|slot| slot.set(None));
    }
}

fn check_activity(lease: Lease) -> anyhow::Result<()> {
    check_invocation(true)?;
    if !lease.evidence.permits(clock_ms(), snapshot()) {
        record_interruption();
        anyhow::bail!(
            "foreground_activity_interrupted: stop task input; observe before continuing"
        );
    }
    Ok(())
}

fn exact_target_is_frontmost(lease: Lease) -> bool {
    crate::input::skylight::front_process_matches(lease.pid, lease.window) == Some(true)
        && crate::ax::bindings::focused_window_id_of_pid(lease.pid) == Some(lease.window)
}

pub(crate) fn check_input() -> anyhow::Result<()> {
    let lease = LEASE
        .with(Cell::get)
        .ok_or_else(|| anyhow::anyhow!("bounded foreground episode is required"))?;
    check_activity(lease)?;
    if !exact_target_is_frontmost(lease) {
        record_interruption();
        anyhow::bail!("foreground target changed; stop input and observe again");
    }
    Ok(())
}

pub(crate) fn check_if_foreground_episode() -> anyhow::Result<()> {
    check_invocation(false)?;
    if LEASE.with(Cell::get).is_some() {
        check_input()?;
    }
    Ok(())
}

pub(crate) fn check_targeted_input(pid: i32) -> anyhow::Result<()> {
    check_invocation(false)?;
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
    let transition = input_transition(event, release);
    if matches!(transition, InputTransition::Unsupported) {
        anyhow::bail!("unsupported native foreground event transition");
    }
    if let InputTransition::Up(control) = transition {
        if !release {
            anyhow::bail!("native release transition must use the cleanup path");
        }
        // A completed pair may also be encountered by an outer unwind guard.
        // Never post the release twice or release input this episode did not own.
        if !PRESSED.with(|held| held.borrow_mut().release(control).is_some()) {
            return Ok(());
        }
        mark_generated(event);
        event.post(CGEventTapLocation::HID);
        return Ok(());
    }
    if release {
        anyhow::bail!("native cleanup may only release an owned key or mouse button");
    }
    // Preallocate the release before dispatching a down or updating a drag
    // position. Allocation failure must never leave an unpaired transition.
    let cleanup = match transition {
        InputTransition::Down(control, kind) | InputTransition::Drag(control, kind) => {
            Some((control, copy_release(event, kind)?))
        }
        _ => None,
    };
    check_input()?;
    if let Some((control, cleanup)) = cleanup {
        match transition {
            InputTransition::Down(_, _) => PRESSED.with(|held| {
                held.borrow_mut().press(control, cleanup).map_err(|_| {
                    anyhow::anyhow!("native foreground control is already held by this episode")
                })
            })?,
            InputTransition::Drag(_, _) => {
                PRESSED.with(|held| held.borrow_mut().update_release(control, cleanup));
            }
            _ => unreachable!(),
        }
    }
    mark_generated(event);
    event.post(CGEventTapLocation::HID);
    Ok(())
}

#[derive(Clone, Copy)]
enum InputTransition {
    Down(InputControl, CGEventType),
    Up(InputControl),
    Drag(InputControl, CGEventType),
    Other,
    Unsupported,
}

fn input_transition(event: &CGEvent, release: bool) -> InputTransition {
    use CGEventType::*;
    use InputTransition::*;
    let key = || {
        InputControl::Key(event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16)
    };
    let other_button = || {
        InputControl::Mouse(
            event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) as u8,
        )
    };
    match event.get_type() {
        KeyDown => Down(key(), KeyUp),
        KeyUp => Up(key()),
        // Quartz represents momentary modifier transitions as FlagsChanged,
        // not KeyDown/KeyUp. Use the emitter's explicit direction: aggregate
        // flags alone cannot distinguish left/right keys of the same modifier.
        FlagsChanged
            if momentary_modifier_flag(
                event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
            )
            .is_some() =>
        {
            if release {
                Up(key())
            } else {
                Down(key(), FlagsChanged)
            }
        }
        LeftMouseDown => Down(InputControl::Mouse(0), LeftMouseUp),
        LeftMouseUp => Up(InputControl::Mouse(0)),
        LeftMouseDragged => Drag(InputControl::Mouse(0), LeftMouseUp),
        RightMouseDown => Down(InputControl::Mouse(1), RightMouseUp),
        RightMouseUp => Up(InputControl::Mouse(1)),
        RightMouseDragged => Drag(InputControl::Mouse(1), RightMouseUp),
        OtherMouseDown => Down(other_button(), OtherMouseUp),
        OtherMouseUp => Up(other_button()),
        OtherMouseDragged => Drag(other_button(), OtherMouseUp),
        MouseMoved | ScrollWheel => Other,
        _ => Unsupported,
    }
}

fn momentary_modifier_flag(key_code: i64) -> Option<core_graphics::event::CGEventFlags> {
    use core_graphics::event::CGEventFlags as Flags;
    match key_code {
        54 | 55 => Some(Flags::CGEventFlagCommand),
        56 | 60 => Some(Flags::CGEventFlagShift),
        58 | 61 => Some(Flags::CGEventFlagAlternate),
        59 | 62 => Some(Flags::CGEventFlagControl),
        63 => Some(Flags::CGEventFlagSecondaryFn),
        // Caps Lock is a toggle, not an owned momentary key press.
        _ => None,
    }
}

fn copy_release(event: &CGEvent, kind: CGEventType) -> anyhow::Result<CGEvent> {
    let raw = unsafe { CGEventCreateCopy(event.as_ptr()) };
    if raw.is_null() {
        anyhow::bail!("cannot allocate foreground cleanup event before input dispatch");
    }
    let release = unsafe { CGEvent::from_ptr(raw) };
    release.set_type(kind);
    if let Some(modifier) =
        momentary_modifier_flag(event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE))
            .filter(|_| matches!(kind, CGEventType::KeyUp | CGEventType::FlagsChanged))
    {
        release.set_flags(event.get_flags() & !modifier);
    }
    mark_generated(&release);
    Ok(release)
}

fn release_owned_inputs() -> bool {
    PRESSED.with(|held| {
        let mut held = held.borrow_mut();
        let had_pressed_controls = !held.is_empty();
        for event in held.drain_reversed() {
            event.post(CGEventTapLocation::HID);
        }
        had_pressed_controls
    })
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
    fn CGEventCreateCopy(event: core_graphics::sys::CGEventRef) -> core_graphics::sys::CGEventRef;
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
    diagnostic_state_from_snapshot(snapshot())
}

fn diagnostic_state_from_snapshot(current: Snapshot) -> serde_json::Value {
    serde_json::json!({
        "contract_version": 1,
        "monitor": if current.reliable { "ready" } else { "unknown" },
        "state": match current.state { State::Idle => "idle", State::Active => "active", State::Unknown => "unknown" },
        "idle_ms": current.idle_ms,
        // These flags declare implemented bounded exact-window contracts, not
        // current input admission or complete qualification of every boundary.
        // The live evidence above and native owner/activity/cleanup guards remain
        // authoritative; unknown cleanup never becomes permission to continue.
        "native_dispatch_guard": true,
        "native_cleanup": true,
        "exact_window_restore": true,
        "batch_foreground_segments": true,
    })
}

#[cfg(test)]
mod episode_lifecycle_tests {
    use super::*;
    use core_graphics::{
        event::CGEventFlags,
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    #[test]
    fn foreground_capabilities_preserve_runtime_admission_evidence() {
        for (reliable, state, idle_ms, admitted) in [
            (false, State::Unknown, 0, false),
            (true, State::Active, 0, false),
            (true, State::Idle, 4999, false),
            (true, State::Idle, 5000, true),
        ] {
            let current = Snapshot {
                reliable,
                state,
                idle_ms,
                generation: 7,
            };
            let value = diagnostic_state_from_snapshot(current);
            assert_eq!(value["contract_version"], 1);
            assert_eq!(value["monitor"], if reliable { "ready" } else { "unknown" });
            assert_eq!(
                value["state"],
                match state {
                    State::Idle => "idle",
                    State::Active => "active",
                    State::Unknown => "unknown",
                }
            );
            assert_eq!(value["idle_ms"], idle_ms);
            for capability in [
                "native_dispatch_guard",
                "native_cleanup",
                "exact_window_restore",
                "batch_foreground_segments",
            ] {
                assert_eq!(value[capability], true);
            }
            assert_eq!(EpisodeLease::begin(10000, current).is_some(), admitted);
        }
    }

    fn context() -> Arc<InvocationContext> {
        Arc::new(InvocationContext {
            cancelled: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            cleanup_unconfirmed: AtomicBool::new(false),
            session_id: Some("foreground-lifecycle-unit-session".into()),
            runtime_scope: Some("foreground-lifecycle-unit-runtime".into()),
            foreground_admission: None,
            background_leases: Mutex::new(Vec::new()),
            transport_owner: None,
            segment_call: None,
            workers: AtomicUsize::new(0),
            invocation_done: AtomicBool::new(false),
        })
    }

    #[test]
    fn cleanup_copy_is_independent_and_releases_only_its_modifier() {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
        let down = CGEvent::new_keyboard_event(source, 56, true).unwrap();
        down.set_flags(CGEventFlags::CGEventFlagCommand | CGEventFlags::CGEventFlagShift);
        assert!(matches!(down.get_type(), CGEventType::FlagsChanged));
        let original_type = down.get_type();
        let original_flags = down.get_flags();
        let original_marker = down.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA);
        let InputTransition::Down(InputControl::Key(56), cleanup_kind) =
            input_transition(&down, false)
        else {
            panic!("native Shift must enter the owned-input ledger");
        };
        let release = copy_release(&down, cleanup_kind).unwrap();
        assert_ne!(
            down.as_ptr(),
            release.as_ptr(),
            "cleanup must own an independent event"
        );
        assert_eq!(down.get_type() as u32, original_type as u32);
        assert_eq!(down.get_flags(), original_flags);
        assert_eq!(
            down.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA),
            original_marker
        );
        assert!(matches!(release.get_type(), CGEventType::FlagsChanged));
        assert_eq!(
            release.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
            56
        );
        assert!(matches!(
            input_transition(&release, true),
            InputTransition::Up(InputControl::Key(56))
        ));
        assert_eq!(release.get_flags(), CGEventFlags::CGEventFlagCommand);
    }

    #[test]
    fn momentary_modifiers_have_balanced_native_flags_changed_cleanup() {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
        for key_code in [54, 55, 56, 60, 58, 61, 59, 62, 63] {
            let modifier = momentary_modifier_flag(key_code).unwrap();
            let other = if modifier == CGEventFlags::CGEventFlagCommand {
                CGEventFlags::CGEventFlagShift
            } else {
                CGEventFlags::CGEventFlagCommand
            };
            let down = CGEvent::new_keyboard_event(source.clone(), key_code as u16, true).unwrap();
            down.set_flags(modifier | other);
            let InputTransition::Down(InputControl::Key(owned_key), kind) =
                input_transition(&down, false)
            else {
                panic!("modifier {key_code} must be an owned down transition");
            };
            assert_eq!(owned_key, key_code as u16);
            assert!(matches!(kind, CGEventType::FlagsChanged));
            let release = copy_release(&down, kind).unwrap();
            assert!(matches!(release.get_type(), CGEventType::FlagsChanged));
            assert_eq!(release.get_flags(), other);
            assert_eq!(down.get_flags(), modifier | other);
            assert!(
                matches!(input_transition(&release, true), InputTransition::Up(InputControl::Key(key)) if key == owned_key)
            );
            // A release remains a release even if aggregate flags still name
            // this modifier (for example, its other physical side is held).
            release.set_flags(modifier | other);
            assert!(
                matches!(input_transition(&release, true), InputTransition::Up(InputControl::Key(key)) if key == owned_key)
            );
            let mut held = PressedInputs::default();
            held.press(InputControl::Key(owned_key), release)
                .unwrap_or_else(|_| panic!("new modifier must not already be held"));
            assert!(held.release(InputControl::Key(36)).is_none());
            assert!(held.release(InputControl::Key(owned_key)).is_some());
            assert!(held.is_empty());
        }
        // Only in-memory events were constructed; no native input was posted.
    }

    #[test]
    fn flags_changed_does_not_admit_toggle_or_unknown_keys() {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
        for key_code in [57, 36, u16::MAX] {
            let event = CGEvent::new_keyboard_event(source.clone(), 36, true).unwrap();
            event.set_type(CGEventType::FlagsChanged);
            event.set_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE, i64::from(key_code));
            for release in [false, true] {
                assert!(matches!(
                    input_transition(&event, release),
                    InputTransition::Unsupported
                ));
            }
        }
    }

    #[test]
    fn cleanup_ledger_never_posts_or_releases_an_unowned_key() {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
        let down = CGEvent::new_keyboard_event(source, 36, true).unwrap();
        let original_flags = down.get_flags();
        let release = copy_release(&down, CGEventType::KeyUp).unwrap();
        assert_ne!(down.as_ptr(), release.as_ptr());
        assert!(matches!(down.get_type(), CGEventType::KeyDown));
        assert!(matches!(release.get_type(), CGEventType::KeyUp));
        assert_eq!(release.get_flags(), original_flags);
        assert_eq!(down.get_flags(), original_flags);
        assert!(matches!(
            input_transition(&down, false),
            InputTransition::Down(InputControl::Key(36), CGEventType::KeyUp)
        ));
        assert!(matches!(
            input_transition(&release, true),
            InputTransition::Up(InputControl::Key(36))
        ));
        let mut held = PressedInputs::default();
        assert!(held.press(InputControl::Key(36), release).is_ok());
        assert!(held.release(InputControl::Key(55)).is_none());
        let releases = held.drain_reversed().collect::<Vec<_>>();
        assert_eq!(releases.len(), 1);
        assert_eq!(
            releases[0].get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
            36
        );
        assert!(held.is_empty());
        // These tests allocate events but deliberately never post native input.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_async_request_revokes_its_still_running_native_worker() {
        let context = context();
        let child_context = Arc::clone(&context);
        let coordinator = Arc::new(tokio::sync::Mutex::new(()));
        let child_coordinator = Arc::clone(&coordinator);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let request = tokio::spawn(async move {
            let _cancel = CancelInvocation {
                context: Arc::clone(&child_context),
                completed: false,
            };
            INVOCATION
                .scope(child_context, async move {
                    let guard = Arc::new(child_coordinator.lock_owned().await);
                    retain_background_lease(Arc::clone(&guard));
                    drop(guard);
                    spawn_blocking(move || {
                        assert!(check_invocation(true).is_ok());
                        started_tx.send(()).unwrap();
                        continue_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                        result_tx.send(check_invocation(true).is_err()).unwrap();
                    })
                    .await
                    .unwrap();
                })
                .await;
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(context.cancelled.load(Ordering::Acquire));
        drop(context);
        assert!(
            coordinator.try_lock().is_err(),
            "detached native worker must retain the process coordinator"
        );
        continue_tx.send(()).unwrap();
        assert!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        let _settled = tokio::time::timeout(Duration::from_secs(2), coordinator.lock_owned())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn native_foreground_without_trusted_request_context_is_refused() {
        assert!(spawn_blocking(|| check_invocation(true).is_err())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn cleanup_unconfirmed_is_sticky_and_takes_precedence_over_interruption() {
        let context = context();
        INVOCATION
            .scope(Arc::clone(&context), async {
                spawn_blocking(mark_native_cleanup_unconfirmed)
                    .await
                    .unwrap();
                assert!(context.cleanup_unconfirmed.load(Ordering::Acquire));
                assert!(check_request()
                    .unwrap_err()
                    .to_string()
                    .contains("native_cleanup_unconfirmed"));
                context.interrupted.store(true, Ordering::Release);
                let mut result = cua_driver_core::protocol::ToolResult::text("ordinary result");
                context.project_native_result(&mut result);
                let structured = result.structured_content.as_ref().unwrap();
                assert_eq!(result.is_error, Some(true));
                assert_eq!(structured["code"], "native_cleanup_unconfirmed");
                assert_eq!(structured["effect"], "unverifiable");
                assert_eq!(structured["verified"], false);
                assert_eq!(structured["retryable"], false);
                assert_eq!(
                    structured["foreground_failure"]["reason"],
                    "native_cleanup_unconfirmed"
                );
                context.interrupted.store(false, Ordering::Release);
                let mut next = cua_driver_core::protocol::ToolResult::text("later result");
                context.project_native_result(&mut next);
                assert_eq!(
                    next.structured_content.as_ref().unwrap()["code"],
                    "native_cleanup_unconfirmed"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn async_preparation_checks_owner_and_sticky_cancellation() {
        let context = context();
        INVOCATION
            .scope(Arc::clone(&context), async {
                assert!(check_invocation(true).is_ok());
                context.cancelled.store(true, Ordering::Release);
                assert!(check_request().is_err());
                assert!(context.interrupted.load(Ordering::Acquire));
                context.cancelled.store(false, Ordering::Release);
                assert!(
                    check_request().is_err(),
                    "an interrupted request cannot resume"
                );
            })
            .await;
        let mut unowned = InvocationContext {
            cancelled: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            cleanup_unconfirmed: AtomicBool::new(false),
            session_id: None,
            runtime_scope: Some("unit-runtime".into()),
            foreground_admission: None,
            background_leases: Mutex::new(Vec::new()),
            transport_owner: None,
            segment_call: None,
            workers: AtomicUsize::new(0),
            invocation_done: AtomicBool::new(false),
        };
        INVOCATION
            .scope(Arc::new(unowned), async {
                assert!(check_invocation(true).is_err());
            })
            .await;
        unowned = InvocationContext {
            cancelled: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            cleanup_unconfirmed: AtomicBool::new(false),
            session_id: Some("unit-session".into()),
            runtime_scope: None,
            foreground_admission: None,
            background_leases: Mutex::new(Vec::new()),
            transport_owner: None,
            segment_call: None,
            workers: AtomicUsize::new(0),
            invocation_done: AtomicBool::new(false),
        };
        INVOCATION
            .scope(Arc::new(unowned), async {
                assert!(check_invocation(true).is_err());
            })
            .await;
    }
}
