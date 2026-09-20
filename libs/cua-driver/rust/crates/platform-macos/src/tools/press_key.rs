use async_trait::async_trait;
use core_foundation::base::CFRelease;
use cua_driver_contract::PressKeyInput;
use cua_driver_core::{
    action_record::{
        ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
        EvidenceKind, RequestedDelivery,
    },
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use libc;
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_bool_attr, copy_string_attr, focused_element_of_pid, AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;

use super::ToolState;

pub struct PressKeyTool {
    state: Arc<ToolState>,
}

impl PressKeyTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
struct AxKeyState {
    value: Option<String>,
    selected: Option<bool>,
}

#[derive(Debug)]
enum PressKeyDeliveryOutcome {
    Confirmed,
    Unverifiable,
    Failed(anyhow::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PressKeyFocusSuppressionPolicy {
    suppress_window_changes: bool,
}

fn focus_suppression_policy(foreground: bool) -> PressKeyFocusSuppressionPolicy {
    if foreground {
        PressKeyFocusSuppressionPolicy {
            suppress_window_changes: false,
        }
    } else {
        PressKeyFocusSuppressionPolicy {
            suppress_window_changes: true,
        }
    }
}

fn map_delivery_outcome(result: anyhow::Result<bool>) -> PressKeyDeliveryOutcome {
    match result {
        Ok(true) => PressKeyDeliveryOutcome::Confirmed,
        Ok(false) => PressKeyDeliveryOutcome::Unverifiable,
        Err(error) => PressKeyDeliveryOutcome::Failed(error),
    }
}

fn display_key_chord(key: &str, modifiers: &[String]) -> String {
    if modifiers.is_empty() {
        key.to_owned()
    } else {
        format!("{}+{key}", modifiers.join("+"))
    }
}

fn validate_post_target(pid: i32) -> anyhow::Result<()> {
    if pid <= 0 {
        anyhow::bail!("target pid {pid} is invalid");
    }
    // Both SLEventPostToPid and CGEventPostToPid are void APIs. A successful
    // call proves only that the request was accepted for posting, not that the
    // target consumed it. Reject the one positive pre-post failure oracle macOS
    // exposes: a process that no longer exists. EPERM still proves liveness.
    let status = unsafe { libc::kill(pid, 0) };
    if status == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EPERM) {
        Ok(())
    } else {
        anyhow::bail!("target pid {pid} is not available for event posting: {error}")
    }
}

fn read_ax_key_state(pid: i32, window_id: Option<u32>, element_ptr: usize) -> Option<AxKeyState> {
    if super::type_text::target_in_web_area(pid, Some((element_ptr, None)), window_id) {
        return None;
    }
    let element = element_ptr as AXUIElementRef;
    let state = AxKeyState {
        value: unsafe { copy_string_attr(element, "AXValue") },
        selected: unsafe { copy_bool_attr(element, "AXSelected") },
    };
    (state.value.is_some() || state.selected.is_some()).then_some(state)
}

fn dispatch_with_ax_oracle(
    pid: i32,
    window_id: Option<u32>,
    explicit_element_ptr: Option<usize>,
    dispatch: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    let (element_ptr, owns_element) = match explicit_element_ptr {
        Some(ptr) => (Some(ptr), false),
        None => unsafe {
            match window_id {
                Some(wid) => crate::ax::exact_target::focused_element_in_window(pid, wid),
                None => focused_element_of_pid(pid),
            }
        }
        .map(|element| (Some(element as usize), true))
        .unwrap_or((None, false)),
    };
    let before = element_ptr.and_then(|ptr| read_ax_key_state(pid, window_id, ptr));
    let result = dispatch();
    // Native controls normally publish their new value/selection on the next
    // run-loop turn. Keep this bounded and reuse the exact retained element so
    // a focus move cannot become false confirmation from a different control.
    if before.is_some() {
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    let after = element_ptr.and_then(|ptr| read_ax_key_state(pid, window_id, ptr));
    if owns_element {
        if let Some(ptr) = element_ptr {
            unsafe { CFRelease(ptr as _) };
        }
    }
    result?;
    Ok(matches!((before, after), (Some(before), Some(after)) if ax_state_changed(&before, &after)))
}

fn ax_state_changed(before: &AxKeyState, after: &AxKeyState) -> bool {
    matches!((&before.value, &after.value), (Some(before), Some(after)) if before != after)
        || matches!((before.selected, after.selected), (Some(before), Some(after)) if before != after)
}

fn action_record(confirmed: bool, foreground: bool) -> ActionExecutionRecord {
    let effect = if confirmed {
        ActionEffect::Confirmed
    } else {
        ActionEffect::Unverifiable
    };
    let transport = if foreground {
        ActionTransport::MacosCgEventHid
    } else {
        ActionTransport::MacosCgEventPid
    };
    let requested = if foreground {
        RequestedDelivery::Foreground
    } else {
        RequestedDelivery::Background
    };
    let actual = if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    };
    let mut record =
        ActionExecutionRecord::builder(effect, transport, requested).actual_delivery(actual);
    if confirmed {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::AccessibilityReadback,
            detail: "the same native AX element changed value or selection after the key post"
                .into(),
        });
    } else {
        record = record.evidence(ActionEvidence {
            kind: EvidenceKind::NativeApiResult,
            detail: if foreground {
                "the key events were constructed and the foreground HID post was attempted".into()
            } else {
                "the key events were constructed and the PID-routed post was attempted".into()
            },
        });
    }
    record.build().expect("press_key record is valid")
}

fn delivery_failed(error: impl std::fmt::Display) -> ToolResult {
    let message = format!("press_key delivery failed: {error}");
    ToolResult::error(&message).with_structured(serde_json::json!({
        "code": "delivery_failed",
        "message": message,
    }))
}

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "press_key".into(),
        description: "Press and release a single key. Follows the same `delivery_mode` ladder as click/type_text \
            — it does NOT raise the window by default:\n\
            • `background` (default): post to the pid WITHOUT fronting/raising — the \
              auth-message path (Chromium-safe). With element_index it focuses that AX \
              element first. `window_id` only targets; it does not raise.\n\
            • `foreground`: guard and briefly front the exact window, focus an addressed AX \
              element when supplied, send a genuine HID key transition so Chromium content, \
              inline editors, and native menu equivalents receive it, then restore prior \
              frontmost. Requires window_id.\n\n\
            A key press is confirmed only when a bounded native AX value/selection read-back \
            changes on the same control. Otherwise a successfully attempted post remains \
            effect:\"unverifiable\" without implying delivery failure or recommending foreground. \
            Key names: return, tab, escape, up/down/left/right, space, delete, \
            home, end, pageup, pagedown, f1-f12, plus any letter or digit. \
            Modifiers array: cmd, shift, option/alt, ctrl, fn.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["key"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid": { "type": "integer" },
                "key": { "type": "string", "description": "Key name: return, tab, escape, up, down, etc." },
                "modifiers": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys: cmd, shift, option/alt, ctrl, fn."
                },
                "window_id": { "type": "integer", "description": "Target window. Required for delivery_mode:\"foreground\". Does NOT itself raise the window — raising is gated on delivery_mode." },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "x": { "type": "number", "description": "Screenshot-pixel X — the element px action form: pixel-click there to focus, then send the key. Use when the key must go to a Chromium/Electron surface the AX path can't focus. Pass with y, no element_index." },
                "y": { "type": "number", "description": "Screenshot-pixel Y (see x)." },
                "scope": { "type": "string", "enum": ["window", "desktop"], "default": "window", "description": "Use desktop with no pid/window_id to send the key to the frontmost application." },
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema()
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

#[async_trait]
impl Tool for PressKeyTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<PressKeyInput>("press_key", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let key = input.key;
            let modifiers = input.modifiers.unwrap_or_default();
            let key_for_input = key.clone();
            let result = crate::foreground_activity::spawn_blocking(move || {
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                crate::input::keyboard::press_key_bare_global(&key_for_input, &modifier_refs)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Pressed '{key}' on the desktop."))
                    .with_structured(serde_json::json!({
                        "scope": "desktop",
                        "path": "hid",
                        "effect": "unverifiable"
                    })),
                Ok(Err(error)) => ToolResult::error(format!("desktop press_key failed: {error}")),
                Err(error) => ToolResult::error(format!("desktop press_key task failed: {error}")),
            };
        }
        let requested_pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let app_context_route = crate::ax::app_context::delegation_route_from_args(&args);
        let key_raw = match args.require_str("key") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let mut modifiers: Vec<String> = args.str_array("modifiers");
        // Surface 6: element_token / element_index precedence resolution.
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg_u64 = args.opt_u64("window_id");
        let window_id_arg = match args.opt_u32("window_id") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match cua_driver_core::element_token::resolve_element_args(
            requested_pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg_u64,
            "press_key",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, snapshot_id) = match resolved {
            cua_driver_core::element_token::ResolvedElement::None => (None, window_id_arg, None),
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: wid,
                element_index: idx,
                snapshot_id,
                via_token: _,
            } => (Some(idx), wid, Some(snapshot_id)),
        };

        // Remap "+" / "plus" → "=" + Shift (same physical key on US layout).
        let key = if key_raw == "+" || key_raw == "plus" {
            if !modifiers.iter().any(|m| m.eq_ignore_ascii_case("shift")) {
                modifiers.push("shift".to_string());
            }
            "=".to_string()
        } else {
            key_raw.clone()
        };
        let display_key = display_key_chord(&key_raw, &modifiers);
        // delivery_mode gates the raise: background (default) never fronts the
        // window (auth-envelope post, even with window_id); foreground is the
        // explicit NSMenu-activation rung. Matches click/type_text/hotkey.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        let fg = delivery_mode.is_foreground();
        let remembered_cursor = if fg {
            super::remembered_agent_cursor_position(&self.state, &args)
        } else {
            None
        };
        // Argument-shape errors are reported before any gating or retained
        // lookups: a malformed call must fail the same way regardless of
        // background-target state.
        let px = args.get("x").and_then(|v| v.as_f64());
        let py = args.get("y").and_then(|v| v.as_f64());
        if px.is_some() && py.is_some() && element_index.is_some() {
            return ToolResult::error(
                "Pass either element_index (ax) or x,y (px) to press_key, not both.",
            );
        }

        let window_return_candidate = !fg
            && std::env::var("CUA_EXPERIMENTAL_CHROME_WINDOW_RETURN").as_deref() == Ok("1")
            && key.eq_ignore_ascii_case("return")
            && modifiers.is_empty()
            && px.is_none()
            && py.is_none();
        // Diagnostics own only bounded in-memory phase records. This owner
        // flushes after the request/worker outcome, never between key down/up.
        let trace_request = window_return_candidate.then(|| {
            crate::input::return_trace::TraceRequest::new(
                "return",
                requested_pid,
                window_id.unwrap_or(0),
            )
        });
        let trace = trace_request
            .as_ref()
            .map(|request| request.trace())
            .unwrap_or_else(crate::input::return_trace::Trace::disabled);

        // Revalidate before either background gating or foreground activation.
        // A same-process modal can appear after observation, so a retained host
        // target must redirect before any key transition is sent.
        let transient_span = trace.begin("transient_guard");
        let transient_result =
            super::guard_same_pid_transient_target(requested_pid, window_id).await;
        transient_span.finish(transient_result.is_ok());
        if let Err(refusal) = transient_result {
            return refusal;
        }

        let foreground_target = if fg {
            let transient_session = crate::transient_ui::TransientSessionKey::from_args(&args);
            match super::resolve_foreground_keyboard_target(
                &self.state,
                &transient_session,
                requested_pid,
                window_id,
                element_index.is_some() || px.is_some() || py.is_some(),
                app_context_route.clone(),
            )
            .await
            {
                Ok(target) => target,
                Err(error) => return error,
            }
        } else {
            super::ForegroundKeyboardTarget {
                pid: requested_pid,
                window_id,
                transient_route: None,
                app_context_route: app_context_route.clone(),
            }
        };
        let pid = foreground_target.pid;
        let window_id = foreground_target.window_id;
        let transient_route = foreground_target.transient_route;
        let app_context_route = foreground_target.app_context_route;

        if let Err(error) = validate_post_target(pid) {
            return delivery_failed(error);
        }

        // Resolve the pre-focus element pointer (if requested) outside
        // the suppression closure — only the focus_element() write itself
        // needs to run under suppression, the cache lookup does not.
        // Retain out of the cache so a concurrent get_window_state can't free
        // the element before the suppressed focus below dereferences it
        // (use-after-free → daemon crash). Each blocking worker owns a guard:
        // cancelling this async request must not free a detached worker's target.
        let pre_focus_guard = if let (Some(idx), Some(wid), Some(snapshot_id)) =
            (element_index, window_id, snapshot_id)
        {
            match self.state.element_cache.get_element_retained_for_snapshot(
                pid,
                wid,
                snapshot_id,
                idx,
            ) {
                Some(guard) => Some(guard),
                None => {
                    return cua_driver_core::element_token::stale_element_cache_result(
                        "press_key",
                        pid,
                        wid,
                        snapshot_id,
                    );
                }
            }
        } else {
            None
        };
        let pre_focus_ptr: Option<usize> = pre_focus_guard.as_ref().map(|g| g.as_ptr());
        trace.event("retained_target_resolved");
        if let Some(element_ptr) = pre_focus_ptr {
            if unsafe {
                super::ensure_app_context_element_window(
                    app_context_route.as_ref(),
                    element_ptr as AXUIElementRef,
                )
            }
            .is_err()
            {
                return super::app_context_delegation_stale_refusal();
            }
        }

        // Independent candidate: keep the ordinary GenericKey singleton gate,
        // but deliver a window-tagged Return without preparing or restoring
        // any app-local focus. Failure never falls through to another route.
        if window_return_candidate {
            let (Some(wid), Some(idx), Some(snapshot), Some(field)) = (
                window_id,
                element_index,
                snapshot_id,
                pre_focus_guard.as_ref(),
            ) else {
                return ToolResult::error(
                    "window-tagged Return requires an explicit fresh field token and exact window",
                )
                .with_structured(serde_json::json!({
                    "code": "experimental_window_return_target_required", "effect": "refused",
                    "key_attempted": false, "cleanup_confirmed": true, "retryable": false,
                }));
            };
            let gate_span = trace.begin("generic_key_gate");
            let gate_result = super::gate_background_window_action(
                pid,
                wid,
                pre_focus_ptr,
                cua_driver_core::background_input::BackgroundAction::GenericKey,
            )
            .await;
            gate_span.finish(gate_result.is_ok());
            let lease = match gate_result {
                Ok(lease) => lease,
                Err(refusal) => return refusal,
            };
            let state = Arc::clone(&self.state);
            let field = field.clone();
            let field_ptr = field.as_ptr();
            let worker_trace = trace.clone();
            let outcome = crate::foreground_activity::spawn_blocking(move || {
                // A cancelled async caller may drop its stack while this
                // worker still owes the paired key-up. Keep the flush owner
                // here so log I/O cannot land inside that owned key interval.
                let _trace_request = trace_request;
                let _lease = lease;
                worker_trace.event("worker_entered");
                let outcome = crate::input::keyboard::experimental_chrome_window_return(
                    pid,
                    wid,
                    &field,
                    &worker_trace,
                    || {
                        crate::foreground_activity::check_request()?;
                        let live = state
                            .element_cache
                            .get_element_retained_for_snapshot(pid, wid, snapshot, idx)
                            .ok_or_else(|| anyhow::anyhow!("window-tagged Return token expired"))?;
                        if live.as_ptr() != field_ptr {
                            anyhow::bail!("window-tagged Return field identity changed");
                        }
                        super::ensure_app_context_delegation_live(app_context_route.as_ref())?;
                        unsafe {
                            super::ensure_app_context_element_window(
                                app_context_route.as_ref(),
                                field_ptr as AXUIElementRef,
                            )?;
                        }
                        Ok(())
                    },
                );
                worker_trace.event(match &outcome {
                    Ok(_) => "worker_posted_pair",
                    Err(failure) if failure.key_attempted => "worker_posted_outcome_unconfirmed",
                    Err(_) => "worker_refused_before_post",
                });
                outcome
            })
            .await;
            return match outcome {
                Ok(Ok(proof)) => ToolResult::text(
                    "Window-tagged background Return posted once without focus preparation; verify the exact page effect.",
                ).with_structured(serde_json::json!({
                    "path": "experimental_chrome_window_return",
                    "pid": proof.pid, "window_id": proof.window_id,
                    "effect": "unverifiable", "verified": false,
                    "key_attempted": proof.key_attempted,
                    "cleanup_confirmed": proof.cleanup_confirmed,
                    "focus_preparation": "none",
                })).with_action_record(action_record(false, false)),
                Ok(Err(failure)) => ToolResult::error(format!(
                    "Window-tagged background Return stopped at {}: {}. No replay or focus fallback.",
                    failure.stage, failure.error,
                )).with_structured(serde_json::json!({
                    "code": "experimental_window_return_stopped",
                    "stage": failure.stage, "reason": failure.error,
                    "effect": if failure.key_attempted { "unverifiable" } else { "refused" },
                    "key_attempted": failure.key_attempted,
                    "cleanup_confirmed": failure.cleanup_confirmed,
                    "retryable": false, "replay_safe": false,
                })),
                Err(error) => ToolResult::error(format!(
                    "Window-tagged background Return worker failed: {error}; no replay is safe.",
                )).with_structured(serde_json::json!({
                    "code": "experimental_window_return_worker_failed",
                    "effect": "unverifiable", "cleanup_confirmed": false,
                    "retryable": false, "replay_safe": false,
                })),
            };
        }

        // Candidate-only feasibility route. Production retains its existing
        // singleton gate until exact multi-window Chrome delivery is qualified.
        // This opt-in does not admit generic keys, implicit fields, or HID.
        if !fg
            && std::env::var("CUA_EXPERIMENTAL_CHROME_BACKGROUND_RETURN").as_deref() == Ok("1")
            && key.eq_ignore_ascii_case("return")
            && modifiers.is_empty()
            && px.is_none()
            && py.is_none()
        {
            if let (Some(wid), Some(idx), Some(snapshot), Some(field)) = (
                window_id,
                element_index,
                snapshot_id,
                pre_focus_guard.as_ref(),
            ) {
                // This gate authorizes only exact AX preparation. The helper
                // independently checks its narrower target-only key capability
                // before posting, under the same retained mutation lease.
                let lease = match super::gate_background_window_action(
                    pid,
                    wid,
                    pre_focus_ptr,
                    cua_driver_core::background_input::BackgroundAction::AxSemantic,
                )
                .await
                {
                    Ok(lease) => lease,
                    Err(refusal) => return refusal,
                };
                let state = Arc::clone(&self.state);
                let field = field.clone();
                let field_ptr = field.as_ptr();
                let outcome = crate::foreground_activity::spawn_blocking(move || {
                    let _lease = lease;
                    crate::input::keyboard::experimental_chrome_background_return(
                        pid,
                        wid,
                        &field,
                        || {
                            crate::foreground_activity::check_request()?;
                            let live = state
                                .element_cache
                                .get_element_retained_for_snapshot(pid, wid, snapshot, idx)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("background Return element token expired")
                                })?;
                            if live.as_ptr() != field_ptr {
                                anyhow::bail!("background Return element identity changed");
                            }
                            super::ensure_app_context_delegation_live(app_context_route.as_ref())?;
                            unsafe {
                                super::ensure_app_context_element_window(
                                    app_context_route.as_ref(),
                                    field_ptr as AXUIElementRef,
                                )?;
                            }
                            Ok(())
                        },
                    )
                })
                .await;
                return match outcome {
                    Ok(Ok(proof)) => ToolResult::text(
                        "Experimental background Return attempted once; inspect the exact target to verify its effect.",
                    ).with_structured(serde_json::json!({
                        "path": "experimental_chrome_background_return",
                        "pid": proof.pid, "window_id": proof.window_id,
                        "effect": "unverifiable", "verified": false,
                        "key_attempted": proof.key_attempted,
                        "cleanup_confirmed": proof.cleanup_confirmed,
                        "ax_context_restored": proof.ax_context_restored,
                    })).with_action_record(action_record(false, false)),
                    Ok(Err(failure)) => ToolResult::error(format!(
                        "Experimental background Return stopped at {}: {}. Do not replay; inspect the target.",
                        failure.stage, failure.error,
                    )).with_structured(serde_json::json!({
                        "code": "experimental_background_return_stopped",
                        "stage": failure.stage, "reason": failure.error,
                        "effect": "unverifiable", "key_attempted": failure.key_attempted,
                        "cleanup_confirmed": failure.cleanup_confirmed,
                        "cleanup_error": failure.cleanup_error, "replay_safe": false,
                    })),
                    Err(error) => ToolResult::error(format!(
                        "Experimental background Return worker failed: {error}; no replay is safe.",
                    )).with_structured(serde_json::json!({
                        "code": "experimental_background_return_worker_failed",
                        "effect": "unverifiable", "cleanup_confirmed": false,
                        "replay_safe": false,
                    })),
                };
            }
        }

        // ── Exact-target background gate (macOS background input v1) ──
        // A window-addressed background key is process-scoped transport: it
        // must prove exact delivery to the requested window (fresh AXWindows
        // membership, not minimized/hidden, no competing same-pid keyboard
        // destination, proven element ancestry) BEFORE anything is sent —
        // including the px focus click. delivery_mode:"foreground" stays the
        // caller's explicit last resort and is not gated here.
        let _mutation_lease = if !fg {
            if let Some(wid) = window_id {
                match super::gate_background_window_action(
                    pid,
                    wid,
                    pre_focus_ptr,
                    cua_driver_core::background_input::BackgroundAction::GenericKey,
                )
                .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                None
            }
        } else {
            None
        };

        // Foreground coordinates are only prepared here. Focus and key delivery
        // run together inside the one activation Episode below.
        let mut foreground_pixel_focus = None;
        let coordinate_focus = if let (Some(cx), Some(cy)) = (px, py) {
            let from_zoom = args
                .get("from_zoom")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if fg {
                match super::prepare_foreground_pixel_focus(
                    &self.state,
                    pid,
                    window_id,
                    cx,
                    cy,
                    from_zoom,
                )
                .await
                {
                    Ok(plan) => foreground_pixel_focus = Some(plan),
                    Err(error) => return error,
                }
            } else if let Err(e) = super::focus_by_pixel(
                &self.state,
                pid,
                window_id,
                cx,
                cy,
                false,
                args.opt_str("session"),
                args.opt_str("_session_id"),
                from_zoom,
                _mutation_lease.as_ref(),
                app_context_route.as_ref(),
            )
            .await
            {
                return e;
            }
            true
        } else {
            false
        };

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // Single-key presses can fire autocomplete (Return on a search box
        // opens a results popover) or trigger menu shortcuts that open windows.
        // Background delivery suppresses only reflex activation of the target;
        // a user switch to an unrelated app must remain untouched. Foreground
        // delivery owns an exact-window activation guard below, so suppressing
        // the target here would race and undo that activation before the HID
        // transition reaches custom canvases such as Blender/GHOST.
        //
        // The AX focus_element() pre-write also runs inside the closure while
        // the observation snapshot's canonical lease is active.
        let prior_front = apps::frontmost_pid();
        let suppression_policy = focus_suppression_policy(fg);
        let snapshot = if suppression_policy.suppress_window_changes {
            WindowChangeDetector::snapshot_targeted(prior_front, pid)
        } else {
            WindowChangeDetector::snapshot_without_suppression(prior_front)
        };

        let result = focus_guard::with_focus_suppressed(
            None,
            prior_front,
            "press_key.CGEvent",
            || async move {
                let pre_focus_app_context_route = app_context_route.clone();
                // Foreground focus belongs inside the bounded activation below;
                // never write focus first and only then check the idle lease.
                if let Some(element) = pre_focus_guard.as_ref().filter(|_| !fg).cloned() {
                    let focus_result = crate::foreground_activity::spawn_blocking(move || {
                        let element_ptr = element.as_ptr();
                        super::ensure_app_context_delegation_live(
                            pre_focus_app_context_route.as_ref(),
                        )?;
                        unsafe {
                            super::ensure_app_context_element_window(
                                pre_focus_app_context_route.as_ref(),
                                element_ptr as AXUIElementRef,
                            )?;
                        }
                        crate::input::ax_actions::focus_element(element_ptr)
                    })
                    .await;
                    match focus_result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => return Ok(Err(error)),
                        Err(error) => return Err(error),
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }

                crate::foreground_activity::spawn_blocking(move || {
                    // Keep the retained target in the worker until all native
                    // dispatch and read-back have settled, even if JoinHandle
                    // ownership disappears when the caller is cancelled.
                    let _pre_focus_guard = pre_focus_guard;
                    let pre_focus_ptr = _pre_focus_guard.as_ref().map(|element| element.as_ptr());
                    super::ensure_app_context_delegation_live(app_context_route.as_ref())?;
                    if let Some(element_ptr) = pre_focus_ptr {
                        unsafe {
                            super::ensure_app_context_element_window(
                                app_context_route.as_ref(),
                                element_ptr as AXUIElementRef,
                            )?;
                        }
                    }
                    let m: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    // Foreground rung: keep the exact target frontmost through a genuine
                    // physical HID key down/up pair, then restore. PID-routed events without the
                    // authentication envelope reach NSMenu, but Chromium/Electron may
                    // silently discard them even while frontmost; the guarded HID route
                    // is accepted by both. Pixel focus may itself have briefly
                    // activated the window, but that helper restores before
                    // returning; always establish a fresh exact foreground HID
                    // guard for the key transition itself.
                    if fg {
                        let wid = window_id.ok_or_else(|| {
                            anyhow::anyhow!(
                                "delivery_mode=foreground requires window_id for press_key"
                            )
                        })?;
                        return dispatch_with_ax_oracle(pid, window_id, pre_focus_ptr, || {
                            let key_action = || super::with_prepared_foreground_focus(foreground_pixel_focus, || {
                                // Activation can change the first responder, so
                                // perform the best-effort AX focus write inside
                                // the guarded foreground interval immediately
                                // before the physical key transition.
                                if let Some(element_ptr) = pre_focus_ptr {
                                    crate::input::ax_actions::focus_element(element_ptr)?;
                                }
                                crate::input::keyboard::press_key_global(&key, &m)
                            });
                            crate::input::skylight::with_foreground_keyboard_target_activation_routed(
                                pid as libc::pid_t,
                                wid,
                                remembered_cursor,
                                coordinate_focus || pre_focus_ptr.is_some(),
                                transient_route,
                                app_context_route,
                                key_action,
                            )
                        });
                    }
                    // Preserve the exact target's local responder context
                    // through the existing AX-oracle settle, without fronting.
                    crate::input::keyboard::with_background_window_context(
                        pid,
                        window_id,
                        || {
                            // AppKit can restore a remembered responder when
                            // the window becomes key. Reapply the explicit
                            // field target inside that context, as in the
                            // foreground path, before reading its AX oracle.
                            if let Some(element_ptr) = pre_focus_ptr {
                                crate::input::ax_actions::focus_element(element_ptr)?;
                            }
                            dispatch_with_ax_oracle(pid, window_id, pre_focus_ptr, || {
                                crate::input::keyboard::press_key(pid, &key, &m)
                            })
                        },
                    )
                })
                .await
            },
        )
        .await;

        let changes = super::finish_window_observation(snapshot, &args).await;

        let delivery_outcome = match result {
            Ok(result) => map_delivery_outcome(result),
            Err(error) => {
                PressKeyDeliveryOutcome::Failed(anyhow::anyhow!("posting task failed: {error}"))
            }
        };

        match delivery_outcome {
            outcome @ (PressKeyDeliveryOutcome::Confirmed
            | PressKeyDeliveryOutcome::Unverifiable) => {
                let confirmed = matches!(outcome, PressKeyDeliveryOutcome::Confirmed);
                let label = if fg {
                    " (delivery_mode:foreground)"
                } else {
                    ""
                };
                let mut structured = serde_json::json!({
                    "path": if fg { "key_events_fg" } else { "key_events" },
                    "verified": confirmed,
                    "effect": if confirmed { "confirmed" } else { "unverifiable" },
                });
                if let Some(route) = transient_route {
                    structured["transient_ui"] = serde_json::json!({
                        "routed": true,
                        "host_pid": route.source.pid,
                        "host_window_id": route.source.window_id,
                        "pid": route.target.pid,
                        "window_id": route.target.window_id,
                    });
                }
                ToolResult::text(format!(
                    "✅ Pressed {display_key} on pid {pid}{label}.{}",
                    changes.result_suffix()
                ))
                .with_structured(structured)
                .with_action_record(action_record(confirmed, fg))
            }
            PressKeyDeliveryOutcome::Failed(error) => delivery_failed(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_outcome_mapper_distinguishes_confirmed_unverifiable_and_failed() {
        assert!(matches!(
            map_delivery_outcome(Ok(true)),
            PressKeyDeliveryOutcome::Confirmed
        ));
        assert!(matches!(
            map_delivery_outcome(Ok(false)),
            PressKeyDeliveryOutcome::Unverifiable
        ));
        let failed = map_delivery_outcome(Err(anyhow::anyhow!("post rejected")));
        assert!(matches!(failed, PressKeyDeliveryOutcome::Failed(_)));
    }

    #[test]
    fn receipt_displays_explicit_modifier_chord() {
        assert_eq!(display_key_chord("f4", &["shift".to_string()]), "shift+f4");
        assert_eq!(display_key_chord("return", &[]), "return");
    }

    #[test]
    fn foreground_exact_window_guard_is_not_raced_by_focus_suppression() {
        assert_eq!(
            focus_suppression_policy(true),
            PressKeyFocusSuppressionPolicy {
                suppress_window_changes: false,
            }
        );
        assert_eq!(
            focus_suppression_policy(false),
            PressKeyFocusSuppressionPolicy {
                suppress_window_changes: true,
            }
        );
    }

    #[test]
    fn accepted_without_oracle_has_no_escalation_but_ax_change_confirms() {
        let unverifiable = action_record(false, false).public_result().unwrap();
        assert_eq!(
            unverifiable.effect,
            cua_driver_contract::ActionEffect::Unverifiable
        );
        assert_eq!(
            unverifiable.delivery.unwrap().mode,
            cua_driver_contract::ActionDeliveryMode::Background
        );
        assert!(unverifiable.escalation.is_none());

        let confirmed = action_record(true, false).public_result().unwrap();
        assert_eq!(
            confirmed.effect,
            cua_driver_contract::ActionEffect::Confirmed
        );
        assert_eq!(confirmed.evidence.unwrap().len(), 1);
        assert!(confirmed.escalation.is_none());
    }

    #[test]
    fn definitely_dead_pid_is_a_typed_delivery_failure() {
        let child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id() as i32;
        let mut child = child;
        child.wait().expect("wait for child exit");
        let failure = delivery_failed(validate_post_target(pid).unwrap_err());
        assert_eq!(failure.is_error, Some(true));
        assert_eq!(
            failure.structured_content.unwrap()["code"],
            "delivery_failed"
        );
    }
}
