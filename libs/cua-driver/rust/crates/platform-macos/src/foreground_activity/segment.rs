//! Cross-RPC ownership. No background timer activates or restores a window.
use super::*;
use cua_driver_core::{
    background_input::ExactWindowTarget,
    foreground_segment::{
        Binding, CallKind, Cleanup, Limits, Owner, Reservation, Segment, State as SegmentState,
    },
    protocol::ToolResult,
};
use serde_json::{json, Value};
use std::collections::VecDeque;

struct Resources {
    background: Arc<tokio::sync::OwnedMutexGuard<()>>,
    _foreground: tokio::sync::OwnedMutexGuard<()>,
}

struct Inner {
    policy: Segment,
    resources: Option<Resources>,
    ending: bool,
    restoring: bool,
    activated: bool,
    cleanup_unknown: bool,
    summary: Option<Value>,
}

struct NativeSegment {
    binding: Binding,
    owner: Arc<cua_driver_core::session::TransportOwner>,
    original: ExactWindowTarget,
    inner: Mutex<Inner>,
}

fn registry() -> &'static Mutex<VecDeque<Arc<NativeSegment>>> {
    static REGISTRY: OnceLock<Mutex<VecDeque<Arc<NativeSegment>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn cleanup_latch() -> &'static AtomicBool {
    static UNKNOWN: AtomicBool = AtomicBool::new(false);
    &UNKNOWN
}

fn all_segments() -> Vec<Arc<NativeSegment>> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

fn failure(code: &str, message: &str, refused: bool) -> ToolResult {
    ToolResult::error(message).with_structured(json!({
        "code": code, "effect": if refused { "refused" } else { "unverifiable" },
        "retryable": false,
    }))
}

fn owner_from_args(
    args: &Value,
) -> Result<(Owner, Arc<cua_driver_core::session::TransportOwner>), ToolResult> {
    let refuse = || {
        failure(
            "foreground_segment_owner_unavailable",
            "A live canonical native transport, session and runtime are required",
            true,
        )
    };
    let session_id = args
        .get("_session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(refuse)?;
    let transport_session_id = args
        .get("_transport_session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(refuse)?;
    let runtime_scope =
        cua_driver_core::tool::current_dispatch_runtime_scope().ok_or_else(refuse)?;
    let owner = cua_driver_core::session::current_transport_owner_for(transport_session_id)
        .ok_or_else(refuse)?;
    if cua_driver_core::session::is_session_ending_or_ended(session_id)
        || cua_driver_core::session::is_runtime_scope_suspended(&runtime_scope)
    {
        return Err(refuse());
    }
    Ok((
        Owner {
            runtime_scope,
            session_id: session_id.into(),
            transport_session_id: transport_session_id.into(),
        },
        owner,
    ))
}

fn target_from_args(args: &Value) -> Result<ExactWindowTarget, ToolResult> {
    let pid = args
        .get("pid")
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .filter(|v| *v > 0);
    let window_id = args
        .get("window_id")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0);
    match (pid, window_id) {
        (Some(pid), Some(window_id)) => Ok(ExactWindowTarget { pid, window_id }),
        _ => Err(failure(
            "foreground_segment_target_invalid",
            "An exact positive native PID and window ID are required",
            true,
        )),
    }
}

fn lookup(args: &Value) -> Result<Arc<NativeSegment>, ToolResult> {
    let (owner, transport) = owner_from_args(args)?;
    let target = target_from_args(args)?;
    let id = args
        .get("foreground_segment_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .ok_or_else(|| {
            failure(
                "foreground_segment_invalid",
                "A bounded segment token is required",
                true,
            )
        })?;
    let segment = all_segments()
        .into_iter()
        .find(|segment| segment.binding.id == id)
        .ok_or_else(|| {
            failure(
                "foreground_segment_invalid",
                "Segment is unknown or retired",
                true,
            )
        })?;
    if !binding_matches(&segment, &owner, target, &transport) {
        return Err(failure(
            "foreground_segment_owner_mismatch",
            "Segment owner or exact target does not match this native request",
            true,
        ));
    }
    Ok(segment)
}

fn binding_matches(
    segment: &NativeSegment,
    owner: &Owner,
    target: ExactWindowTarget,
    transport: &Arc<cua_driver_core::session::TransportOwner>,
) -> bool {
    segment.binding.owner == *owner
        && segment.binding.target == target
        && Arc::ptr_eq(&segment.owner, transport)
}

impl NativeSegment {
    fn expected_front(&self) -> ExactWindowTarget {
        if self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activated
        {
            self.binding.target
        } else {
            self.original
        }
    }

    fn cleanup_is_settled(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        !inner.policy.in_flight()
            && !inner.ending
            && !inner.restoring
            && !inner.cleanup_unknown
            && inner.resources.is_none()
    }

    fn owner_live(&self) -> bool {
        self.owner.is_live()
            && !cua_driver_core::session::is_session_ending_or_ended(&self.binding.owner.session_id)
            && !cua_driver_core::session::is_runtime_scope_suspended(
                &self.binding.owner.runtime_scope,
            )
    }

    fn check(&self) -> anyhow::Result<()> {
        let live = self.owner_live();
        let now = clock_ms();
        let evidence = snapshot();
        // In an Episode, check_input supplies the exact foreground proof after
        // this activity check. Until our first activation attempt, only the
        // captured original may be front; afterwards only the immutable target.
        // A programmatic focus change must not silently grant a new episode.
        let expected_front = self.expected_front();
        let focus_live = LEASE.with(Cell::get).is_some() || exact_front(expected_front);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.cleanup_unknown {
            anyhow::bail!("native_cleanup_unconfirmed");
        }
        if !live || !focus_live {
            inner.policy.revoke();
        }
        inner
            .policy
            .check(&self.binding, now, evidence)
            .map_err(|_| {
                anyhow::anyhow!(
                    "foreground_activity_interrupted: foreground segment is no longer live"
                )
            })
    }

    fn summary(&self, restoration: &str) -> Value {
        json!({"foreground_segment_id": self.binding.id, "phase": "closed",
               "pid": self.binding.target.pid, "window_id": self.binding.target.window_id,
               "cleanup_confirmed": true, "restoration": restoration})
    }

    fn revoke(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .revoke();
        self.settle_revoked();
    }

    fn mark_unknown(&self) {
        cleanup_latch().store(true, Ordering::Release);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cleanup_unknown = true;
        inner.policy.revoke();
        if !inner.policy.in_flight() {
            let _ = inner.policy.finish_cleanup(Cleanup::Unknown);
        }
    }

    fn settle_revoked(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.restoring
            || inner.policy.in_flight()
            || inner.policy.state() != SegmentState::Revoked
        {
            return;
        }
        if inner.cleanup_unknown {
            let _ = inner.policy.finish_cleanup(Cleanup::Unknown);
        } else {
            if inner.policy.finish_cleanup(Cleanup::Confirmed).is_err() {
                inner.cleanup_unknown = true;
                cleanup_latch().store(true, Ordering::Release);
                return;
            }
            inner.ending = false;
            inner.summary = Some(self.summary("skipped_interrupted"));
            inner.resources.take();
        }
    }

    fn finish_restore(&self, restoration: &str) -> Option<Value> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.ending = false;
        inner.restoring = false;
        if inner.cleanup_unknown {
            let _ = inner.policy.finish_cleanup(Cleanup::Unknown);
            return None;
        }
        if inner.policy.finish_cleanup(Cleanup::Confirmed).is_err() {
            return None;
        }
        let summary = self.summary(restoration);
        inner.summary = Some(summary.clone());
        inner.resources.take();
        Some(summary)
    }
}

/// An invocation ticket settles only after its async scope AND blocking workers.
pub(super) struct Call {
    segment: Arc<NativeSegment>,
    reservation: Mutex<Option<Reservation>>,
}

impl Call {
    pub(super) fn target(&self) -> ExactWindowTarget {
        self.segment.binding.target
    }
    pub(super) fn activity_lease(&self) -> EpisodeLease {
        self.segment
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .activity_lease()
    }
    pub(super) fn check(&self) -> anyhow::Result<()> {
        self.segment.check()
    }
    pub(super) fn revoke(&self) {
        self.segment.revoke();
    }
    pub(super) fn cleanup_unknown(&self) {
        self.segment.mark_unknown();
    }
    pub(super) fn cleanup_is_unknown(&self) -> bool {
        self.segment
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cleanup_unknown
    }
    pub(super) fn mark_activated(&self) {
        self.segment
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activated = true;
    }
    pub(super) fn background_lease(
        &self,
        pid: i32,
    ) -> Option<Arc<tokio::sync::OwnedMutexGuard<()>>> {
        if pid != self.target().pid {
            return None;
        }
        self.segment
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resources
            .as_ref()
            .map(|resources| Arc::clone(&resources.background))
    }
    pub(super) fn settle(&self) {
        if let Some(reservation) = self
            .reservation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let failed = {
                self.segment
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .policy
                    .settle(reservation)
                    .is_err()
            };
            if failed {
                self.segment.mark_unknown();
            }
        }
        self.segment.settle_revoked();
    }
    pub(super) fn closed_summary(&self) -> Option<Value> {
        self.segment
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .summary
            .clone()
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        // Unwinding/early refusal is not a licence to keep an unowned segment open.
        let reserved = self
            .reservation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if reserved {
            self.revoke();
            self.settle();
        }
    }
}

fn supported_tool(name: &str) -> bool {
    matches!(
        name,
        "click"
            | "double_click"
            | "right_click"
            | "drag"
            | "scroll"
            | "type_text"
            | "press_key"
            | "hotkey"
            | "set_value"
            | "perform_secondary_action"
            | "get_window_state"
    )
}

fn validate_element_target(
    args: &Value,
    target: ExactWindowTarget,
    tool: &str,
) -> Result<(), ToolResult> {
    if args
        .get("element_token")
        .is_some_and(|value| !value.is_string())
    {
        return Err(failure(
            "foreground_segment_target_invalid",
            "Segment element token must be a valid exact-window token",
            true,
        ));
    }
    let resolved = cua_driver_core::element_token::resolve_element_args(
        target.pid,
        args.get("element_index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok()),
        args.get("element_token").and_then(Value::as_str),
        args.get("snapshot_id").and_then(Value::as_str),
        Some(u64::from(target.window_id)),
        tool,
    )
    .map_err(|_| {
        failure(
            "foreground_segment_target_invalid",
            "Segment element target is invalid or stale; no input was sent",
            true,
        )
    })?;
    if matches!(resolved, cua_driver_core::element_token::ResolvedElement::Element { window_id: Some(window), .. } if window != target.window_id)
    {
        return Err(failure(
            "foreground_segment_target_mismatch",
            "Element token belongs to a different exact window; no input was sent",
            true,
        ));
    }
    Ok(())
}

pub(super) fn admit_call(args: &Value, tool: &str) -> Result<Option<Arc<Call>>, ToolResult> {
    if cleanup_latch().load(Ordering::Acquire) {
        return Err(failure(
            "native_cleanup_unconfirmed",
            "Native segment cleanup is unknown; stop all GUI work",
            false,
        ));
    }
    if args.get("foreground_segment_id").is_none() {
        if let Some(pid) = args.get("pid").and_then(Value::as_i64) {
            if all_segments().iter().any(|segment| {
                segment.binding.target.pid as i64 == pid
                    && segment
                        .inner
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .resources
                        .is_some()
            }) {
                return Err(failure(
                    "foreground_segment_required",
                    "This process belongs to an active foreground segment",
                    true,
                ));
            }
        }
        return Ok(None);
    }
    let segment = lookup(args)?;
    if !supported_tool(tool) {
        return Err(failure(
            "foreground_segment_action_unsupported",
            "This action is not supported within an exact-window foreground segment",
            true,
        ));
    }
    if tool == "get_window_state"
        && args
            .get("window_selection")
            .and_then(Value::as_str)
            .is_some_and(|selection| selection != "exact")
    {
        return Err(failure(
            "foreground_segment_target_invalid",
            "Segment observations must retain their exact window target",
            true,
        ));
    }
    validate_element_target(args, segment.binding.target, tool)?;
    segment.check().map_err(|_| {
        failure(
            "foreground_activity_interrupted",
            "Segment activity or ownership ended; no new call admitted",
            true,
        )
    })?;
    let reservation = segment
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .policy
        .reserve(
            &segment.binding,
            clock_ms(),
            snapshot(),
            if tool == "get_window_state" {
                CallKind::Observation
            } else {
                CallKind::Mutation
            },
        )
        .map_err(|_| {
            failure(
                "foreground_segment_not_available",
                "Segment is busy, ended or exhausted; no new call admitted",
                true,
            )
        })?;
    Ok(Some(Arc::new(Call {
        segment,
        reservation: Mutex::new(Some(reservation)),
    })))
}

fn exact_front(target: ExactWindowTarget) -> bool {
    crate::input::skylight::front_process_matches(target.pid, target.window_id) == Some(true)
        && bounded_focused_window(target.pid) == Some(target.window_id)
}

fn bounded_focused_window(pid: i32) -> Option<u32> {
    use crate::ax::bindings::*;
    struct Owned(AXUIElementRef);
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { core_foundation::base::CFRelease(self.0 as _) };
        }
    }
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let app = Owned(app);
        if AXUIElementSetMessagingTimeout(app.0, 0.1) != kAXErrorSuccess {
            return None;
        }
        let window = Owned(copy_element_attr(app.0, "AXFocusedWindow")?);
        if AXUIElementSetMessagingTimeout(window.0, 0.1) != kAXErrorSuccess {
            return None;
        }
        ax_get_window_id(window.0)
    }
}

pub(crate) async fn begin_segment(args: Value) -> ToolResult {
    let (owner, transport) = match owner_from_args(&args) {
        Ok(owner) => owner,
        Err(result) => return result,
    };
    let target = match target_from_args(&args) {
        Ok(target) => target,
        Err(result) => return result,
    };
    if cleanup_latch().load(Ordering::Acquire) {
        return failure(
            "native_cleanup_unconfirmed",
            "Native cleanup is unknown; no segment can begin",
            false,
        );
    }
    let background = match tokio::time::timeout(
        Duration::from_millis(100),
        crate::background_mutation::acquire(target.pid),
    )
    .await
    {
        Ok(guard) => Arc::new(guard),
        Err(_) => {
            return failure(
                "foreground_segment_busy",
                "Target mutation ownership is busy",
                true,
            )
        }
    };
    let foreground = match foreground_writer().try_lock_owned() {
        Ok(guard) => guard,
        Err(_) => {
            return failure(
                "foreground_segment_busy",
                "Native foreground ownership is busy",
                true,
            )
        }
    };
    let binding = Binding {
        owner,
        target,
        id: format!("fgs_{}", uuid::Uuid::new_v4().simple()),
    };
    let mut policy =
        match Segment::begin(binding.clone(), clock_ms(), snapshot(), Limits::default()) {
            Ok(policy) => policy,
            Err(_) => {
                return failure(
                    "foreground_activity_unavailable",
                    "Five seconds of reliable idle are required",
                    true,
                )
            }
        };
    // No activation or other write in begin. A cancelled capture cannot leave input.
    let original = match tokio::task::spawn_blocking(move || {
        if !matches!(
            crate::windows::resolve_window_owner(target.pid, target.window_id),
            crate::windows::WindowOwner::SamePid
        ) {
            return None;
        }
        let pid = crate::apps::frontmost_pid()?;
        let original = ExactWindowTarget {
            pid,
            window_id: bounded_focused_window(pid)?,
        };
        (exact_front(original)
            && matches!(
                crate::windows::resolve_window_owner(original.pid, original.window_id),
                crate::windows::WindowOwner::SamePid
            ))
        .then_some(original)
    })
    .await
    {
        Ok(Some(original)) => original,
        _ => {
            return failure(
                "foreground_activity_unavailable",
                "Exact original window or target ownership is unavailable",
                true,
            )
        }
    };
    if !transport.is_live()
        || cua_driver_core::session::is_session_ending_or_ended(&binding.owner.session_id)
        || cua_driver_core::session::is_runtime_scope_suspended(&binding.owner.runtime_scope)
        || policy.check(&binding, clock_ms(), snapshot()).is_err()
    {
        return failure(
            "foreground_activity_unavailable",
            "Foreground admission changed during preparation",
            true,
        );
    }
    let segment = Arc::new(NativeSegment {
        binding,
        owner: transport,
        original,
        inner: Mutex::new(Inner {
            policy,
            resources: Some(Resources {
                background,
                _foreground: foreground,
            }),
            ending: false,
            restoring: false,
            activated: false,
            cleanup_unknown: false,
            summary: None,
        }),
    });
    {
        let mut entries = registry().lock().unwrap_or_else(|e| e.into_inner());
        while entries.len() >= 64 {
            if let Some(index) = entries.iter().position(|entry| {
                entry
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .resources
                    .is_none()
            }) {
                entries.remove(index);
            } else {
                return failure(
                    "foreground_segment_busy",
                    "Native segment registry is full",
                    true,
                );
            }
        }
        entries.push_back(Arc::clone(&segment));
    }
    let watched = Arc::clone(&segment);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let (state, deadline, lease) = {
                let inner = watched.inner.lock().unwrap_or_else(|e| e.into_inner());
                (
                    inner.policy.state(),
                    inner.policy.deadline_ms(),
                    inner.policy.activity_lease(),
                )
            };
            if matches!(state, SegmentState::Closed | SegmentState::CleanupUnknown) {
                break;
            }
            // Closing after a call/step budget is exhausted is not a new lease:
            // a client that disappears without end must still expire. The timer
            // only revokes ownership and never moves focus or emits input.
            if !watched.owner_live()
                || clock_ms() >= deadline
                || !lease.permits(clock_ms(), snapshot())
            {
                watched.revoke();
            }
            watched.settle_revoked();
        }
    });
    ToolResult::text("Native foreground segment opened; no window activation or input dispatched.").with_structured(json!({
        "foreground_segment_id": segment.binding.id, "phase": "open", "pid": target.pid, "window_id": target.window_id,
    }))
}

fn claim_end(inner: &mut Inner, binding: &Binding, finish: bool) -> Result<(), ToolResult> {
    if inner.ending || inner.restoring {
        return Err(failure(
            "foreground_segment_busy",
            "Segment end is already running",
            false,
        ));
    }
    inner.policy.request_close(binding).map_err(|_| {
        failure(
            "foreground_segment_not_available",
            "Segment cannot close",
            false,
        )
    })?;
    // Claim exclusive end ownership under the same lock BEFORE any wait for
    // native workers. No second end may queue another restore behind us.
    inner.ending = true;
    if !finish {
        inner.policy.revoke();
    }
    Ok(())
}

pub(crate) async fn end_segment(args: Value) -> ToolResult {
    let segment = match lookup(&args) {
        Ok(segment) => segment,
        Err(result) => return result,
    };
    let finish = match args.get("mode").and_then(Value::as_str) {
        Some("finish") => true,
        Some("abort") => false,
        _ => {
            return failure(
                "foreground_segment_end_invalid",
                "End mode must be finish or abort",
                true,
            )
        }
    };
    {
        let mut inner = segment.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.cleanup_unknown {
            return failure(
                "native_cleanup_unconfirmed",
                "Segment cleanup is unknown",
                false,
            );
        }
        if let Some(summary) = &inner.summary {
            return ToolResult::text("Segment is already closed.").with_structured(summary.clone());
        }
        if let Err(refusal) = claim_end(&mut inner, &segment.binding, finish) {
            return refusal;
        }
    }
    struct EndGuard {
        segment: Arc<NativeSegment>,
        context: Option<Arc<InvocationContext>>,
        completed: bool,
    }
    impl Drop for EndGuard {
        fn drop(&mut self) {
            if !self.completed {
                if let Some(context) = &self.context {
                    context.cancelled.store(true, Ordering::Release);
                }
                {
                    let mut inner = self.segment.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if !inner.restoring {
                        inner.ending = false;
                    }
                }
                self.segment.revoke();
            }
        }
    }
    let mut guard = EndGuard {
        segment: Arc::clone(&segment),
        context: None,
        completed: false,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if !segment
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .in_flight()
        {
            break;
        }
        if Instant::now() >= deadline {
            segment.mark_unknown();
            return failure(
                "native_cleanup_unconfirmed",
                "Segment workers did not settle within the cleanup bound",
                false,
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let lease;
    {
        let mut inner = segment.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.cleanup_unknown {
            return failure(
                "native_cleanup_unconfirmed",
                "Segment cleanup is unknown",
                false,
            );
        }
        if let Some(summary) = &inner.summary {
            guard.completed = true;
            return ToolResult::text("Segment closed without focus reclamation.")
                .with_structured(summary.clone());
        }
        lease = inner.policy.activity_lease();
        inner.restoring = true;
    }
    let context = Arc::new(InvocationContext {
        cancelled: AtomicBool::new(false),
        interrupted: AtomicBool::new(false),
        cleanup_unconfirmed: AtomicBool::new(false),
        session_id: Some(segment.binding.owner.session_id.clone()),
        runtime_scope: Some(segment.binding.owner.runtime_scope.clone()),
        foreground_admission: Some(lease),
        background_leases: Mutex::new(Vec::new()),
        transport_owner: Some(Arc::clone(&segment.owner)),
        segment_call: None,
        workers: AtomicUsize::new(0),
        invocation_done: AtomicBool::new(false),
    });
    guard.context = Some(Arc::clone(&context));
    let worker_segment = Arc::clone(&segment);
    let worker_context = Arc::clone(&context);
    let work = INVOCATION
        .scope(Arc::clone(&context), async move {
            spawn_blocking(move || {
                struct FailedRestore(Arc<NativeSegment>, bool);
                impl Drop for FailedRestore {
                    fn drop(&mut self) {
                        if !self.1 {
                            self.0.mark_unknown();
                        }
                    }
                }
                let mut emergency = FailedRestore(Arc::clone(&worker_segment), false);
                let live = finish
                    && worker_segment.owner_live()
                    && lease.permits(clock_ms(), snapshot())
                    && worker_segment
                        .inner
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .policy
                        .state()
                        == SegmentState::Closing;
                let restoration = if !live {
                    "skipped_interrupted"
                } else if exact_front(worker_segment.original) {
                    "unchanged"
                } else if !exact_front(worker_segment.binding.target) {
                    worker_segment.revoke();
                    "skipped_interrupted"
                } else if crate::input::skylight::restore_exact_window_guarded(
                    worker_segment.original.pid,
                    worker_segment.original.window_id,
                    || {
                        worker_context.check()?;
                        if !worker_segment.owner_live() || !lease.permits(clock_ms(), snapshot()) {
                            anyhow::bail!("foreground segment interrupted during restoration");
                        }
                        Ok(())
                    },
                ) {
                    "restored"
                } else if !worker_segment.owner_live()
                    || !lease.permits(clock_ms(), snapshot())
                    || worker_context.cancelled.load(Ordering::Acquire)
                    || worker_context.interrupted.load(Ordering::Acquire)
                {
                    "skipped_interrupted"
                } else {
                    "failed"
                };
                if worker_context.cleanup_unconfirmed.load(Ordering::Acquire) {
                    worker_segment.mark_unknown();
                }
                let summary = worker_segment.finish_restore(restoration);
                emergency.1 = true;
                (summary, restoration == "failed")
            })
            .await
        })
        .await;
    guard.completed = true;
    match work {
        Ok((Some(summary), failed)) => {
            let result = if failed {
                ToolResult::error("Segment input settled but original-window restoration failed")
            } else {
                ToolResult::text("Native foreground segment closed and owned input settled")
            };
            result.with_structured(summary)
        }
        _ => {
            segment.mark_unknown();
            failure(
                "native_cleanup_unconfirmed",
                "Segment cleanup could not be confirmed",
                false,
            )
        }
    }
}

pub(crate) fn stop_session_segments(session_id: &str) -> Result<(), String> {
    let mut settled = true;
    for segment in all_segments() {
        if segment.binding.owner.session_id == session_id
            || segment.binding.owner.transport_session_id == session_id
        {
            segment.revoke();
            settled &= segment.cleanup_is_settled();
        }
    }
    if settled {
        Ok(())
    } else {
        Err("Foreground segment native cleanup is still pending or unconfirmed".into())
    }
}

pub(crate) fn stop_runtime_segments(runtime_scope: &str) {
    for segment in all_segments() {
        if segment.binding.owner.runtime_scope == runtime_scope {
            segment.revoke();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> Snapshot {
        Snapshot {
            reliable: true,
            state: State::Idle,
            idle_ms: 5_000,
            generation: 7,
        }
    }

    async fn fixture() -> (
        Arc<NativeSegment>,
        Arc<tokio::sync::Mutex<()>>,
        Arc<tokio::sync::Mutex<()>>,
    ) {
        let binding = Binding {
            owner: Owner {
                runtime_scope: "test-runtime".into(),
                session_id: "test-session".into(),
                transport_session_id: "test-owner".into(),
            },
            target: ExactWindowTarget {
                pid: 42,
                window_id: 71,
            },
            id: "fgs_offline_test".into(),
        };
        let background = Arc::new(tokio::sync::Mutex::new(()));
        let foreground = Arc::new(tokio::sync::Mutex::new(()));
        let segment = Arc::new(NativeSegment {
            owner: cua_driver_core::session::TransportOwner::new(
                binding.owner.transport_session_id.clone(),
            ),
            original: ExactWindowTarget {
                pid: 43,
                window_id: 72,
            },
            inner: Mutex::new(Inner {
                policy: Segment::begin(binding.clone(), 5_000, idle(), Limits::default()).unwrap(),
                resources: Some(Resources {
                    background: Arc::new(Arc::clone(&background).lock_owned().await),
                    _foreground: Arc::clone(&foreground).lock_owned().await,
                }),
                ending: false,
                restoring: false,
                activated: false,
                cleanup_unknown: false,
                summary: None,
            }),
            binding,
        });
        (segment, background, foreground)
    }

    fn call(segment: &Arc<NativeSegment>) -> Arc<Call> {
        let reservation = segment
            .inner
            .lock()
            .unwrap()
            .policy
            .reserve(&segment.binding, 5_000, idle(), CallKind::Mutation)
            .unwrap();
        Arc::new(Call {
            segment: Arc::clone(segment),
            reservation: Mutex::new(Some(reservation)),
        })
    }

    fn context(call: Arc<Call>) -> Arc<InvocationContext> {
        Arc::new(InvocationContext {
            cancelled: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            cleanup_unconfirmed: AtomicBool::new(false),
            session_id: None,
            runtime_scope: None,
            foreground_admission: None,
            background_leases: Mutex::new(Vec::new()),
            transport_owner: None,
            segment_call: Some(call),
            workers: AtomicUsize::new(0),
            invocation_done: AtomicBool::new(false),
        })
    }

    #[test]
    fn foreground_segment_whitelist_excludes_unbound_and_persistent_actions() {
        for tool in [
            "press_key",
            "type_text",
            "set_value",
            "get_window_state",
            "click",
            "hotkey",
        ] {
            assert!(supported_tool(tool), "{tool}");
        }
        for tool in [
            "launch_app",
            "bring_to_front",
            "set_window_frame",
            "move_cursor",
            "invoke_menu",
            "unknown",
        ] {
            assert!(!supported_tool(tool), "{tool}");
        }
    }

    #[test]
    fn foreground_segment_element_token_cannot_override_exact_window() {
        use cua_driver_core::element_token::{format_token, global};
        let target = ExactWindowTarget {
            pid: 2_147_470_501,
            window_id: 601,
        };
        let own_snapshot = global().register_snapshot(target.pid, target.window_id, 3);
        let sibling_snapshot = global().register_snapshot(target.pid, 602, 3);
        let own = format_token(own_snapshot, 1);
        let sibling = format_token(sibling_snapshot, 1);
        assert!(
            validate_element_target(&json!({"element_token": own}), target, "set_value").is_ok()
        );
        assert!(validate_element_target(
            &json!({"element_token": sibling, "window_id": 601}),
            target,
            "set_value"
        )
        .is_err());
        assert!(
            validate_element_target(&json!({"element_token": "stale"}), target, "set_value")
                .is_err()
        );
        assert!(
            validate_element_target(&json!({"element_token": null}), target, "set_value").is_err()
        );
        assert!(
            validate_element_target(&json!({"element_index": 1}), target, "set_value").is_err()
        );
        // Replacing that window's observation must revoke the previously valid
        // token rather than allowing it to bind to the replacement cache.
        global().register_snapshot(target.pid, target.window_id, 3);
        assert!(
            validate_element_target(&json!({"element_token": own}), target, "set_value").is_err()
        );
    }

    #[tokio::test]
    async fn foreground_segment_exact_target_rewrite_is_refused_before_dispatch() {
        let (segment, _, _) = fixture().await;
        let ticket = call(&segment);
        let context = context(Arc::clone(&ticket));
        INVOCATION
            .scope(context, async {
                assert!(check_segment_target(42, Some(71)).is_ok());
                for (pid, window) in [(42, Some(72)), (43, Some(71)), (42, None)] {
                    let result = check_segment_target(pid, window).unwrap_err();
                    let result = result.structured_content.unwrap();
                    assert_eq!(result["code"], "foreground_segment_target_mismatch");
                    assert_eq!(result["effect"], "refused");
                }
            })
            .await;
        ticket.revoke();
        ticket.settle();
        assert!(
            check_segment_target(99, None).is_ok(),
            "tokenless dispatch is unchanged"
        );
    }

    #[tokio::test]
    async fn foreground_segment_activation_attempt_never_revives_original_expectation() {
        let (segment, _, _) = fixture().await;
        assert_eq!(segment.expected_front(), segment.original);
        let ticket = call(&segment);
        ticket.mark_activated();
        assert_eq!(segment.expected_front(), segment.binding.target);
        ticket.settle();
        assert_eq!(segment.expected_front(), segment.binding.target);
        ticket.revoke();
        assert_eq!(segment.expected_front(), segment.binding.target);
    }

    #[tokio::test]
    async fn foreground_segment_session_cleanup_proof_requires_released_resources() {
        let (segment, _, _) = fixture().await;
        assert!(!segment.cleanup_is_settled());
        let ticket = call(&segment);
        ticket.revoke();
        assert!(
            !segment.cleanup_is_settled(),
            "in-flight worker has not settled"
        );
        ticket.settle();
        assert!(segment.cleanup_is_settled());
        segment.inner.lock().unwrap().cleanup_unknown = true;
        assert!(
            !segment.cleanup_is_settled(),
            "unknown cleanup cannot be reported complete"
        );
    }

    #[tokio::test]
    async fn foreground_segment_binding_requires_actual_transport_capability() {
        let (segment, _, _) = fixture().await;
        assert!(binding_matches(
            &segment,
            &segment.binding.owner,
            segment.binding.target,
            &segment.owner
        ));
        let same_name_new_capability =
            cua_driver_core::session::TransportOwner::new("test-owner".into());
        assert!(!binding_matches(
            &segment,
            &segment.binding.owner,
            segment.binding.target,
            &same_name_new_capability
        ));
        let mut foreign = segment.binding.owner.clone();
        foreign.session_id = "other-session".into();
        assert!(!binding_matches(
            &segment,
            &foreign,
            segment.binding.target,
            &segment.owner
        ));
        foreign = segment.binding.owner.clone();
        foreign.runtime_scope = "other-runtime".into();
        assert!(!binding_matches(
            &segment,
            &foreign,
            segment.binding.target,
            &segment.owner
        ));
        assert!(!binding_matches(
            &segment,
            &segment.binding.owner,
            ExactWindowTarget {
                pid: 42,
                window_id: 999
            },
            &segment.owner
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn foreground_segment_concurrent_end_claims_only_one_restore_owner_before_waiting() {
        let (segment, background, foreground) = fixture().await;
        let ticket = call(&segment);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut waiters = Vec::new();
        for _ in 0..2 {
            let segment = Arc::clone(&segment);
            let barrier = Arc::clone(&barrier);
            waiters.push(std::thread::spawn(move || {
                barrier.wait();
                let mut inner = segment.inner.lock().unwrap();
                claim_end(&mut inner, &segment.binding, true).is_ok()
            }));
        }
        barrier.wait();
        let winners = waiters
            .into_iter()
            .map(|thread| usize::from(thread.join().unwrap()))
            .sum::<usize>();
        assert_eq!(winners, 1, "only one end may proceed to its worker wait");
        assert!(segment.inner.lock().unwrap().ending);
        assert!(segment.inner.lock().unwrap().policy.in_flight());
        assert!(background.try_lock().is_err());
        assert!(foreground.try_lock().is_err());
        ticket.settle();
        {
            let mut inner = segment.inner.lock().unwrap();
            assert!(
                claim_end(&mut inner, &segment.binding, true).is_err(),
                "settlement does not transfer end ownership"
            );
            inner.restoring = true;
        }
        segment.finish_restore("unchanged").unwrap();
        assert!(segment.cleanup_is_settled());
    }

    #[tokio::test]
    async fn foreground_segment_normal_call_settles_without_releasing_segment_ownership() {
        let (segment, background, foreground) = fixture().await;
        let ticket = call(&segment);
        let context = context(Arc::clone(&ticket));
        context.finish_invocation();
        assert!(!segment.inner.lock().unwrap().policy.in_flight());
        assert_eq!(
            segment.inner.lock().unwrap().policy.state(),
            SegmentState::Open
        );
        assert!(background.try_lock().is_err());
        assert!(foreground.try_lock().is_err());
        assert!(ticket.closed_summary().is_none());
        ticket.revoke();
        assert!(background.try_lock().is_ok());
        assert!(foreground.try_lock().is_ok());
        assert_eq!(
            ticket.closed_summary().unwrap()["restoration"],
            "skipped_interrupted"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn foreground_segment_abort_waits_for_detached_native_worker_before_closed_proof() {
        let (segment, background, foreground) = fixture().await;
        let ticket = call(&segment);
        let context = context(Arc::clone(&ticket));
        let child_context = Arc::clone(&context);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            let _cancel = CancelInvocation {
                context: Arc::clone(&child_context),
                completed: false,
            };
            INVOCATION
                .scope(child_context, async {
                    spawn_blocking(move || {
                        started_tx.send(()).unwrap();
                        continue_rx.recv().unwrap();
                        // This worker never touches an AX object or posts input.
                    })
                    .await
                    .unwrap();
                })
                .await;
        });
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(context.workers.load(Ordering::Acquire), 1);
        assert!(segment.inner.lock().unwrap().policy.in_flight());
        assert_eq!(
            segment.inner.lock().unwrap().policy.state(),
            SegmentState::Revoked
        );
        assert!(ticket.closed_summary().is_none());
        assert!(background.try_lock().is_err());
        assert!(foreground.try_lock().is_err());
        continue_tx.send(()).unwrap();
        let released =
            tokio::time::timeout(Duration::from_secs(1), Arc::clone(&foreground).lock_owned())
                .await
                .unwrap();
        drop(released);
        assert!(!segment.inner.lock().unwrap().policy.in_flight());
        assert!(background.try_lock().is_ok());
        assert_eq!(
            ticket.closed_summary().unwrap(),
            json!({
                "foreground_segment_id": "fgs_offline_test", "phase": "closed", "pid": 42,
                "window_id": 71, "cleanup_confirmed": true, "restoration": "skipped_interrupted",
            })
        );
    }

    #[tokio::test]
    async fn foreground_segment_call_can_borrow_only_its_owned_pid_lock() {
        let (segment, background, _) = fixture().await;
        let ticket = call(&segment);
        assert!(ticket.background_lease(99).is_none());
        let borrowed = ticket.background_lease(42).unwrap();
        ticket.revoke();
        ticket.settle();
        assert!(
            background.try_lock().is_err(),
            "borrowed native work still owns coordinator"
        );
        drop(borrowed);
        assert!(background.try_lock().is_ok());
    }

    #[tokio::test]
    async fn foreground_segment_closed_proof_is_not_published_during_restore_worker() {
        let (segment, background, foreground) = fixture().await;
        segment.inner.lock().unwrap().restoring = true;
        segment.revoke();
        assert!(segment.inner.lock().unwrap().summary.is_none());
        assert!(background.try_lock().is_err());
        assert!(foreground.try_lock().is_err());
        let summary = segment.finish_restore("skipped_interrupted").unwrap();
        assert_eq!(summary["cleanup_confirmed"], true);
        assert!(foreground.try_lock().is_ok());
    }
}
