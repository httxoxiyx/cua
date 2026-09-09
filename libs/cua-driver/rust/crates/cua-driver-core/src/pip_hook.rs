//! PiP frame-push hook — registered once by `main.rs` when the
//! `--experimental-pip` flag is on argv.
//!
//! The trait + factory live in the `pip-preview` crate so the platform
//! backends can implement them without depending on `cua-driver-core`.
//! What lives here is just the per-process callback that the tool
//! dispatcher uses to seed frames from successful observations — a thin shim
//! so `tool.rs` doesn't need to know about `pip-preview` directly and we keep
//! the dependency graph one-directional. Platform live capture owns updates
//! after that seed; mutations do not trigger another synchronous screenshot.
//!
//! The PNG bytes pushed through here are reused from the exact image content
//! already returned by `get_window_state`, so PiP and the model begin from the
//! same frame without paying for a duplicate capture.

use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipHookDelegation {
    pub kind: String,
    pub host_pid: i64,
    pub panel_kind: String,
    pub expected_bundle_id: Option<String>,
    pub expected_app_name: Option<String>,
}

#[derive(Clone)]
pub struct PipHookTarget {
    pub logical_pid: Option<i64>,
    pub delegation: Option<PipHookDelegation>,
    pub pid: i64,
    pub window_id: u64,
    pub session_id: Option<String>,
}

/// Synthesized per-call frame payload. Kept structurally identical
/// to `pip_preview::PipFrame` — duplicated here to keep `cua-driver-core`
/// from importing `pip-preview` (the dependency would be circular once
/// platform backends pull both crates in).
pub struct PipHookFrame {
    pub target: PipHookTarget,
    pub png_bytes: Vec<u8>,
    pub timestamp_ms: u64,
}

pub enum PipHookEvent {
    Upsert(PipHookFrame),
    Ensure(PipHookTarget),
    EndSession(String),
    SetInputPassthrough { passthrough: bool },
}

/// Restores PiP interactivity even when a physical action returns early or
/// unwinds. Physical desktop actions are serialized by the dispatcher, so a
/// process-global passthrough scope is sufficient.
pub struct PipInputPassthroughGuard;

impl Drop for PipInputPassthroughGuard {
    fn drop(&mut self) {
        if let Some(f) = PIP_EVENT_FN.get() {
            if let Err(error) = f(PipHookEvent::SetInputPassthrough { passthrough: false }) {
                tracing::error!(%error, "failed to restore PiP input handling");
            }
        }
    }
}

type PipEventFnBox = Box<dyn Fn(PipHookEvent) -> Result<(), String> + Send + Sync>;
static PIP_EVENT_FN: OnceLock<PipEventFnBox> = OnceLock::new();

/// Register the platform-side push callback. `main.rs` calls this
/// once after starting the PiP backend.
pub fn set_pip_event_fn(f: impl Fn(PipHookEvent) -> Result<(), String> + Send + Sync + 'static) {
    if PIP_EVENT_FN.set(Box::new(f)).is_ok() {
        crate::session::register_session_end_hook(end_pip_session);
    }
}

/// True when a PiP backend is wired up. Tool dispatcher uses this to
/// skip the screenshot-bytes path when nothing would consume the
/// frame (avoiding wasted capture work in the common --pip-off case).
pub fn pip_enabled() -> bool {
    PIP_EVENT_FN.get().is_some()
}

/// Make the PiP stack input-transparent before a physical desktop action.
/// The platform callback applies the native state synchronously.
pub fn begin_pip_input_passthrough() -> Result<Option<PipInputPassthroughGuard>, String> {
    let Some(f) = PIP_EVENT_FN.get() else {
        return Ok(None);
    };
    f(PipHookEvent::SetInputPassthrough { passthrough: true })?;
    Ok(Some(PipInputPassthroughGuard))
}

/// Push a frame to the PiP window. No-op when no backend is registered.
pub fn push_pip_frame(frame: PipHookFrame) {
    if let Some(f) = PIP_EVENT_FN.get() {
        let _ = f(PipHookEvent::Upsert(frame));
    }
}

/// Ensure the exact target has a PiP card/live stream without synchronously
/// capturing on the tool-dispatch path. No-op when no backend is registered.
pub fn ensure_pip_target(target: PipHookTarget) {
    if let Some(f) = PIP_EVENT_FN.get() {
        let _ = f(PipHookEvent::Ensure(target));
    }
}

/// Remove frames associated with an ended runtime session.
pub fn end_pip_session(session_id: &str) {
    if let Some(f) = PIP_EVENT_FN.get() {
        let _ = f(PipHookEvent::EndSession(session_id.to_owned()));
    }
}
