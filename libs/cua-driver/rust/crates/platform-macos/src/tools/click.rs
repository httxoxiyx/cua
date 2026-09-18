//! click tool — matches the Swift reference ClickTool.swift.
//!
//! Two addressing modes:
//!
//! * **Element path** (`element_index` + `window_id`): normally performs AXAction
//!   on the cached element. Plain text-input clicks without AXPress instead use
//!   one exact-window pointer route, selected before any actuator runs.
//!   Extra behaviors vs. the naive dispatch:
//!   - AXTextField / AXTextArea AXPress: at least 100 ms best-effort focus
//!     settle after dispatch completes, including window-report time, without
//!     claiming that the DOM is ready or the click effect is confirmed.
//!   - AXPopUpButton: appends the list of available options and redirects to set_value.
//!   - Advertised-action warning if the element didn't list the requested action.
//!
//! * **Pixel path** (`x`, `y`): synthesises CGEvent mouse clicks and posts them to
//!   the target pid.  `from_zoom=true` translates zoom-crop pixel coordinates back
//!   to full-window space using the most recent `zoom` context stored per-pid.

use async_trait::async_trait;
use cua_driver_contract::{ClickButton, ClickInput};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    tool_args::parse_typed_projection,
};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_action_names, copy_bool_attr, copy_children, copy_element_attr, copy_string_attr,
    element_at_screen_position, element_screen_rect, kAXErrorSuccess, AXUIElementPerformAction,
    AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::{CFRelease, TCFType};

use super::ToolState;

pub struct ClickTool {
    state: Arc<ToolState>,
}

impl ClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Focus posture for the raw pixel transport after AX hit-testing has failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PixelActivationPolicy {
    /// Standard background delivery: suppress activation of the target.
    SuppressTarget,
    /// Left-click with a concrete window: synthesize event-routing focus only
    /// inside the target. The real foreground application remains active/key.
    SyntheticTargetFocus,
    /// Explicit foreground rung owns its brief activation and restoration.
    ForegroundAssist,
}

#[derive(Clone, Copy, Debug)]
struct SelectionPixelTarget {
    screen_x: f64,
    screen_y: f64,
    window_x: f64,
    window_y: f64,
}

/// Choose one actuator before the exact-target gate. A pointer-selected text
/// input must never enter the generic AX click/fallback implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ElementClickRoute {
    AxSemantic,
    TextInputPointer,
}

fn element_click_route(
    action: &str,
    button: &str,
    has_modifiers: bool,
    role: &str,
    advertised_actions: &[String],
    selectable_ancestry: bool,
    auxiliary_surface: bool,
) -> ElementClickRoute {
    if (action.eq_ignore_ascii_case("press") || action.eq_ignore_ascii_case("click"))
        && button == "left"
        && !has_modifiers
        && matches!(role, "AXTextField" | "AXTextArea")
        && !advertised_actions.iter().any(|action| action == "AXPress")
        && !selectable_ancestry
        && !auxiliary_surface
    {
        ElementClickRoute::TextInputPointer
    } else {
        ElementClickRoute::AxSemantic
    }
}

/// The host-attached classifier deliberately excludes directly addressed
/// popover windows. Neither case is part of the text-input pointer route.
fn text_input_pointer_has_auxiliary_window(element_ptr: usize) -> bool {
    unsafe {
        let Some(window) = copy_element_attr(element_ptr as AXUIElementRef, "AXWindow") else {
            return false;
        };
        let role = copy_string_attr(window, "AXRole");
        CFRelease(window as _);
        matches!(role.as_deref(), Some("AXPopover" | "AXMenu" | "AXMenuBar"))
    }
}

/// AX and WindowServer geometry are both logical screen points, not capture
/// pixels. Refuse a missing/changed frame or a center outside that exact window;
/// never clamp it or reinterpret it as a desktop coordinate.
fn text_input_pointer_target(
    element_rect: Option<[f64; 4]>,
    captured_bounds: &crate::windows::WindowBounds,
    live_bounds: Option<&crate::windows::WindowBounds>,
) -> Option<SelectionPixelTarget> {
    let [x, y, width, height] = element_rect?;
    let live = live_bounds?;
    let bounds = captured_bounds;
    if ![
        x,
        y,
        width,
        height,
        bounds.x,
        bounds.y,
        bounds.width,
        bounds.height,
    ]
    .iter()
    .all(|value| value.is_finite())
        || width <= 0.0
        || height <= 0.0
        || bounds.width <= 0.0
        || bounds.height <= 0.0
        || [bounds.x, bounds.y, bounds.width, bounds.height]
            != [live.x, live.y, live.width, live.height]
    {
        return None;
    }
    let screen_x = x + width / 2.0;
    let screen_y = y + height / 2.0;
    let window_x = screen_x - bounds.x;
    let window_y = screen_y - bounds.y;
    if !screen_x.is_finite()
        || !screen_y.is_finite()
        || !(0.0..bounds.width).contains(&window_x)
        || !(0.0..bounds.height).contains(&window_y)
    {
        return None;
    }
    Some(SelectionPixelTarget {
        screen_x,
        screen_y,
        window_x,
        window_y,
    })
}

fn element_click_result(
    fronted: bool,
    used_pixel: bool,
    selection_verified: bool,
    suspected_noop: bool,
) -> Value {
    serde_json::json!({
        "path": match (used_pixel, fronted) {
            (true, true) => "cgevent_fg",
            (true, false) => "cgevent",
            (false, true) => "ax_fg",
            (false, false) => "ax",
        },
        "verified": selection_verified,
        "effect": if selection_verified {
            "confirmed"
        } else if suspected_noop {
            "suspected_noop"
        } else {
            "unverifiable"
        },
    })
}

fn selection_readback_confirms(
    before: bool,
    after: bool,
    has_modifiers: bool,
    prior_selected_peers_preserved: bool,
) -> bool {
    if has_modifiers {
        after != before && prior_selected_peers_preserved
    } else {
        after
    }
}

const TEXT_INPUT_FOCUS_SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

fn needs_text_input_focus_settle(role: &str, ax_action: &str) -> bool {
    ax_action == "AXPress" && matches!(role, "AXTextField" | "AXTextArea")
}

fn remaining_text_input_focus_settle(elapsed: std::time::Duration) -> std::time::Duration {
    TEXT_INPUT_FOCUS_SETTLE.saturating_sub(elapsed)
}

const SELECTION_READBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const SELECTION_READBACK_POLL: std::time::Duration = std::time::Duration::from_millis(25);
const SELECTION_READBACK_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);
const SELECTION_READBACK_STABILITY: std::time::Duration = std::time::Duration::from_millis(250);

fn pixel_activation_policy(
    button: &str,
    effective_foreground: bool,
    has_window: bool,
) -> PixelActivationPolicy {
    if effective_foreground {
        PixelActivationPolicy::ForegroundAssist
    } else if button == "left" && has_window {
        PixelActivationPolicy::SyntheticTargetFocus
    } else {
        PixelActivationPolicy::SuppressTarget
    }
}

/// A visually targeted background click may use AX as its delivery backend
/// only when the exact hit-tested element explicitly advertises AXPress.
/// macOS can return kAXErrorSuccess for AXPress on containers such as
/// AXWebArea even though no page pointer event is produced; treating that as
/// delivery would turn a coordinate click into a silent no-op.
fn background_pixel_ax_press_eligible(
    press_action: bool,
    role: &str,
    advertised_actions: &[String],
    enabled: Option<bool>,
) -> bool {
    let is_concrete_press_control = matches!(
        role,
        "AXButton" | "AXCheckBox" | "AXRadioButton" | "AXLink" | "AXDisclosureTriangle"
    );
    press_action
        && enabled == Some(true)
        && is_concrete_press_control
        && advertised_actions.iter().any(|action| action == "AXPress")
}
fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "click".into(),
        description:
            "Click against a target pid. **Prefer `element_token` over pixel \
             coordinates** — semantic actions can work on backgrounded / minimized / hidden / \
             off-Space windows. The token identifies one exact snapshot element and tells \
             you what you're clicking via the cached element's role + label. Reach for \
             `x, y` only when the target is a canvas / video / WebGL / custom-drawn surface \
             that doesn't appear in the AX tree.\n\n\
             Two addressing modes:\n\n\
             - element_token, or element_index + snapshot_id (from get_window_state): normally AX delivery. \
               A plain unmodified primary click on a text input without AXPress uses exact-window \
               pointer delivery instead and requires a live, visible window frame. Background delivery \
               does not authorize foreground activation. \
               The snapshot cache is scoped per (pid, window_id) and is replaced by the \
               next snapshot of the same window — re-snapshot every turn before clicking.\n\n\
             - x, y (window-local screenshot pixels, top-left origin of the PNG returned \
               by get_window_state): CGEvent path. Synthesizes mouse events and posts to \
               pid. Use modifier for cmd/shift/option/ctrl. Needs a visible on-screen \
               window to anchor the conversion.\n\n\
             button: \"left\" (default), \"right\", or \"middle\". Defaults to left so the \
             field is fully back-compat — omit it and you get the legacy left-click behaviour. \
             Pixel path: routes through the CGEvent left/right/middle mouse-button primitives. \
             AX path: \"right\" maps to AXShowMenu (same surface as the dedicated `right_click` \
             tool); \"middle\" has no AX equivalent and falls back to a pixel middle-click at the \
             element's center.\n\
             action: press (default), show_menu, pick, confirm, cancel, open.\n\
             from_zoom: set true after a zoom call to auto-translate zoom-image pixel \
             coordinates to full-window space."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            // `pid` is conditionally required — needed for window/element clicks
            // but omitted for windowless `scope:"desktop"` clicks — so it is NOT
            // in `required`; the code validates it with a clear error when needed.
            // (Keeps the contract consistent across platforms; see
            // cua_driver_core::tool_schema.)
            "required": [],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "pid":           { "type": "integer", "description": "Target process ID." },
                "window_id":     { "type": "integer", "description": "Target window ID. Required for element_index. Optional when element_token is supplied (the token carries it)." },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "x":             { "type": "number",  "description": "X in screenshot pixels. A window target uses the get_window_state PNG; a desktop target uses the native get_desktop_state PNG. The driver reverses Retina backing scale and any window-image downscale." },
                "y":             { "type": "number",  "description": "Y in screenshot pixels from the image selected by target." },
                "action":        { "type": "string",  "description": "AX action: press, show_menu, pick, confirm, cancel, open." },
                "button":        {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button. Default: \"left\" — omit for legacy left-click behaviour. Pixel path uses the matching CGEvent primitive; AX path maps \"right\" to AXShowMenu and falls back to a pixel middle-click at the element's center for \"middle\"."
                },
                "count":         { "type": "integer", "description": "Click count for pointer delivery, including text inputs without AXPress. Default 1." },
                "modifier": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Modifier keys: cmd, shift, option/alt, ctrl."
                },
                "from_zoom": {
                    "type": "boolean",
                    "description": "When true, x and y are in the last zoom image for this pid; driver translates back to full-window coordinates."
                },
                "debug_image_out": {
                    "type": "string",
                    "description": "Optional file path. When set on a pixel-addressed click, captures a fresh screenshot, draws a red crosshair at (x, y), and writes the PNG. Use to verify coordinate spaces. Requires window_id; incompatible with from_zoom."
                },
                "delivery_mode": {
                    "type": "string",
                    "enum": ["background", "foreground"],
                    "description": "Best-effort-background ladder rung (default \"background\"). \"background\": perform the AX action or post the CGEvent without fronting. \"foreground\": briefly front the window, act, let transient UI settle, then restore the prior frontmost app. Requires window_id. Modified clicks require \"foreground\" so macOS observes physical modifier-key state. A generic click has no independent postcondition read-back, except selection of list-like AX rows whose AXSelected state can be confirmed; otherwise confirm the effect from a fresh state snapshot. Use the agent loop: background AX (element_index) → snapshot → background pixel (x/y) → snapshot → delivery_mode:\"foreground\"."
                },
                "scope": {
                    "type": "string",
                    "enum": ["window", "desktop"],
                    "description": "Coordinate frame for a windowless screen-absolute click (default \"window\"). Pass \"desktop\" when sending x,y with NO pid/window_id — the coordinates are then true screen pixels (read from get_desktop_state with scope=\"desktop\"). Per-call; not a setting."
                }
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  false,
        open_world:  true,
    })
}

#[async_trait]
impl Tool for ClickTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;

        // ── Window-less screen-absolute branch (scope="desktop") ──────
        // x,y given with NO pid and NO window_id → the coordinates are TRUE
        // SCREEN pixels. This is the foreground, vision-driven desktop-scope
        // path, the macOS peer of the Windows WindowFromPoint click. Gate on the
        // effective scope: under "window" return a structured
        // `desktop_scope_disabled` error (same contract as Windows) rather than
        // silently treating window-local pixels as screen pixels.
        let has_pid = args.get("pid").map(|v| !v.is_null()).unwrap_or(false);
        let has_window_id = args.get("window_id").map(|v| !v.is_null()).unwrap_or(false);
        let has_xy = args.get("x").map(|v| v.is_number()).unwrap_or(false)
            && args.get("y").map(|v| v.is_number()).unwrap_or(false);
        if has_xy && !has_pid && !has_window_id {
            // `scope` is a per-call param now (default "window"); pass
            // scope="desktop" to enable screen-absolute clicks.
            let scope = args.str_or("scope", "window");
            if scope != "desktop" {
                return ToolResult::error(
                    "click: x,y given with no pid/window_id, but scope is \"window\". \
                     Screen-absolute clicks require desktop scope. Pass scope=\"desktop\" \
                     (and use get_desktop_state with scope=\"desktop\" to read true \
                     screen pixels) first."
                        .to_string(),
                )
                .with_structured(serde_json::json!({
                    "code": "desktop_scope_disabled",
                    "scope": scope,
                    "suggestion": "pass scope=\"desktop\"",
                }));
            }
            let input = match parse_typed_projection::<ClickInput>("click", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let sx_shot = input.x;
            let sy_shot = input.y;
            // ── Desktop-screenshot pixels → logical screen points ──────────────
            // The vision invariant: the pixel an agent reads off the screenshot it
            // was handed is the pixel that gets clicked. `get_desktop_state`
            // returns the display at NATIVE pixels (e.g. 3024×1964 on a 2× Retina
            // display whose logical size is 1512×982), but everything below — the
            // window-under-point hit test (logical CGWindow bounds), the cursor
            // warp, and the CGEvent post — operates in LOGICAL screen points. So
            // x,y arrive in desktop-SCREENSHOT space (what the agent reads off the
            // PNG) and must be divided by the screenshot↔logical ratio, or a
            // center-pixel pick warps to the corner (off by the backing scale).
            //
            // Derive the ratio the same way `get_desktop_state` reports it: native
            // screenshot width / logical screen width. This is robust even when
            // CGDisplayPixelsWide under-reports the backing scale (it returns the
            // scaled-mode point width on some Retina configs → a bogus 1.0).
            let desktop_ratio = tokio::task::spawn_blocking(|| {
                let logical_w =
                    super::get_screen_size::main_screen_size().map(|(w, _, _)| w as f64);
                let shot_w = crate::capture::screenshot_display_bytes()
                    .ok()
                    .and_then(|png| crate::capture::png_dimensions(&png).ok())
                    .map(|(w, _)| w as f64);
                match (shot_w, logical_w) {
                    (Some(sw), Some(lw)) if lw > 0.0 && sw > lw => sw / lw,
                    _ => 1.0,
                }
            })
            .await
            .unwrap_or(1.0);
            let sx = sx_shot / desktop_ratio;
            let sy = sy_shot / desktop_ratio;
            let button = match input.button.unwrap_or(ClickButton::Left) {
                ClickButton::Left => "left",
                ClickButton::Right => "right",
                ClickButton::Middle => "middle",
            }
            .to_owned();
            let count = input.count.unwrap_or(1) as usize;
            if count == 0 {
                return ToolResult::error("click.count must be at least 1.")
                    .with_structured(serde_json::json!({ "code": "invalid_arguments" }));
            }
            // Keep decorative cursor feedback off the input critical path when
            // the daemon opts into asynchronous click feedback.
            let cursor_key = super::cursor_tools::resolve_cursor_key(&args);
            crate::cursor::overlay::animate_click_feedback(cursor_key.clone(), sx, sy).await;
            self.state
                .cursor_registry
                .update_position(&cursor_key, sx, sy);

            let btn = button.clone();
            let desktop_modifiers: Vec<String> = args.str_array("modifier");
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                // Desktop scope is explicitly foreground and vision-driven: post
                // at the global HID tap so WindowServer delivers to the window
                // actually visible at this point. PID-posting here would silently
                // turn the foreground contract back into background delivery.
                let modifier_refs: Vec<&str> =
                    desktop_modifiers.iter().map(String::as_str).collect();
                crate::input::mouse::click_at_xy_desktop_with_modifiers(
                    sx,
                    sy,
                    count,
                    &btn,
                    &modifier_refs,
                )
            })
            .await;
            let button_label = match button.as_str() {
                "right" => "right-click",
                "middle" => "middle-click",
                _ => "click",
            };
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "✅ Sent screen-absolute {button_label} at desktop-pixel \
                     ({sx_shot:.0},{sy_shot:.0}) → screen-point ({sx:.0},{sy:.0}) \
                     (desktop scope; not driver-verified)."
                ))
                .with_structured(serde_json::json!({ "path": "cgevent_hid", "verified": false, "effect": "unverifiable" })),
                Ok(Err(e)) => ToolResult::error(format!("desktop-scope click failed: {e}")),
                Err(e) => ToolResult::error(format!("task error: {e}")),
            };
        }

        let pid = match args.require_i32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let app_context_route = crate::ax::app_context::delegation_route_from_args(&args);
        // Resolve this action's cursor key so its click-pulse / glide land on
        // the calling session's cursor, not the shared "default" one.
        let cursor_key = super::cursor_tools::resolve_cursor_key(&args);

        // Surface 6: resolve element_token / element_index precedence
        // BEFORE the pixel-path fallback. Token wins on disagreement; a
        // stale token returns an explicit error instead of silently
        // falling back to the integer (Surface 6 hard constraint).
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg_u64 = args.opt_u64("window_id");
        let window_id_arg = match args.opt_u32("window_id") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match cua_driver_core::element_token::resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg_u64,
            "click",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id, snapshot_id, _via_token) = match resolved {
            cua_driver_core::element_token::ResolvedElement::None => {
                (None, window_id_arg, None, false)
            }
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: wid,
                element_index: idx,
                snapshot_id,
                via_token,
            } => (Some(idx), wid, Some(snapshot_id), via_token),
        };
        let x = args
            .opt_f64("x")
            .or_else(|| args.opt_i64("x").map(|i| i as f64));
        let y = args
            .opt_f64("y")
            .or_else(|| args.opt_i64("y").map(|i| i as f64));
        let action = args.str_or("action", "press");
        // Surface 5: optional `button` arg, default "left" preserves legacy behaviour.
        // Pixel path: routes to left/right/middle CGEvent primitives.
        // AX path: "right" delegates to AXShowMenu (same surface as right_click);
        // "middle" has no AX equivalent and falls back to a pixel middle-click
        // at the element's screen-space center.
        let button_str = args.str_or("button", "left").to_lowercase();
        // delivery_mode: per-call ladder rung. Foreground briefly activates the
        // target for both AX and pixel paths, then restores the prior app.
        let delivery_mode = super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref());
        // Reject unknown buttons explicitly so silent left-click fall-through can't
        // mask a typo. Keep "" → default left for old clients that never sent the field.
        if !matches!(button_str.as_str(), "" | "left" | "right" | "middle") {
            return ToolResult::error(format!(
                "click: unknown button \"{button_str}\" — expected one of left, right, middle."
            ));
        }
        let button_str = if button_str.is_empty() {
            "left".to_string()
        } else {
            button_str
        };
        let count = args.u64_or("count", 1) as usize;
        let from_zoom = args.bool_or("from_zoom", false);
        let debug_image_out = args.opt_str("debug_image_out");
        let modifiers: Vec<String> = args.str_array("modifier");

        // PID-routed key transitions can look correct for one AX poll and then
        // collapse to a plain click once AppKit resolves the gesture. Refuse
        // that false-success path. The explicit foreground rung uses physical
        // HID modifier transitions under an exact-window activation guard.
        if !modifiers.is_empty() && (!delivery_mode.is_foreground() || window_id.is_none()) {
            return ToolResult::error(
                "click modifiers require delivery_mode:\"foreground\" and window_id on macOS; \
                 background PID-routed events cannot preserve live modifier-key state",
            )
            .with_structured(serde_json::json!({
                "code": "background_unavailable",
                "effect": "refused",
                "escalation": {
                    "recommended": "foreground",
                    "reason": "macOS modifier clicks require exact-window HID delivery so the target observes live modifier state"
                }
            }));
        }

        // A fresh same-pid modal may have appeared after the observation.  Do
        // not translate or dispatch the retained host coordinates/elements in
        // that state: the image can show the transient while this call still
        // names the covered host window.
        if let Err(refusal) = super::guard_same_pid_transient_target(pid, window_id).await {
            return refusal;
        }

        let transient_session = crate::transient_ui::TransientSessionKey::from_args(&args);
        if let Err(refusal) =
            super::guard_transient_pointer_target(&self.state, &transient_session, pid, window_id)
                .await
        {
            return refusal;
        }

        if let (Some(idx), Some(wid), Some(snapshot_id)) = (element_index, window_id, snapshot_id) {
            // ── AX element path ────────────────────────────────────────────
            // Retain the element out of the cache so it can't be freed by a
            // concurrent get_window_state on the same (pid, window_id) while
            // this click is mid-flight (use-after-free → daemon crash). The
            // guard lives to the end of this method, past the AX action below.
            let element_guard = match self.state.element_cache.get_element_retained_for_snapshot(
                pid,
                wid,
                snapshot_id,
                idx,
            ) {
                Some(e) => e,
                None => {
                    return cua_driver_core::element_token::stale_element_cache_result(
                        "click",
                        pid,
                        wid,
                        snapshot_id,
                    )
                }
            };
            let element_ptr = element_guard.as_ptr();
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

            // Right-click is still the semantic AXShowMenu request. Only a
            // plain primary click on a non-selectable text input without
            // AXPress chooses pointer delivery; AXConfirm/AXShowMenu are not
            // substitutes for a click, and a failed AX attempt is not retried.
            let effective_action = if button_str == "right" && action == "press" {
                "show_menu".to_string()
            } else {
                action.clone()
            };
            let foreground = delivery_mode.is_foreground();
            let primary_press = (effective_action.eq_ignore_ascii_case("press")
                || effective_action.eq_ignore_ascii_case("click"))
                && button_str == "left"
                && modifiers.is_empty();
            let inspect_background_surface = !foreground && button_str != "middle";
            let selection_action = effective_action == "press" && button_str != "middle";
            let (background_menu, background_popover, selectable_ancestry, element_route) =
                match tokio::task::spawn_blocking(move || unsafe {
                    let element = element_ptr as AXUIElementRef;
                    let role = if inspect_background_surface || primary_press {
                        copy_string_attr(element, "AXRole").unwrap_or_default()
                    } else {
                        String::new()
                    };
                    // These isolated semantic surfaces never gain pointer
                    // authority from being observed alongside a host window.
                    let menu = inspect_background_surface
                        && crate::ax::application_menu::is_actionable_menu_role(&role)
                        && crate::ax::exact_target::element_window_id(element).is_none();
                    let popover = inspect_background_surface
                        && !menu
                        && crate::ax::attached_popover::has_displaced_popover_window(element, wid);
                    let pointer_candidate = primary_press
                        && matches!(role.as_str(), "AXTextField" | "AXTextArea")
                        && !menu
                        && !popover;
                    // Preserve the old collection-selection lookup condition.
                    // Additional ancestry queries are only for text-pointer
                    // candidates, not confirm/show-menu or middle-click calls.
                    let selectable = ((selection_action && !menu && !popover) || pointer_candidate)
                        && crate::input::ax_actions::nearest_container_selection_state(element_ptr)
                            .is_some();
                    let route = if pointer_candidate && !selectable {
                        let actions = copy_action_names(element);
                        let auxiliary = !actions.iter().any(|action| action == "AXPress")
                            && (text_input_pointer_has_auxiliary_window(element_ptr)
                                || (foreground
                                    && crate::ax::attached_popover::has_displaced_popover_window(
                                        element, wid,
                                    )));
                        element_click_route(
                            "press", "left", false, &role, &actions, false, auxiliary,
                        )
                    } else {
                        ElementClickRoute::AxSemantic
                    };
                    (menu, popover, selectable, route)
                })
                .await
                {
                    Ok(facts) => facts,
                    Err(error) => {
                        return ToolResult::error(format!(
                            "Element click route lookup failed: {error}. No input was sent."
                        ))
                    }
                };

            // ── Exact-target background gate (macOS background input v1) ──
            // Text-input pointer delivery and button=middle both require the
            // stricter WindowPointer rung with the retained element proof. Gate
            // BEFORE any cursor/dispatch work so a stale or sibling-owned
            // target refuses instead of acting on the wrong window.
            let _mutation_lease = if !delivery_mode.is_foreground() {
                let gate_action = if button_str == "middle"
                    || element_route == ElementClickRoute::TextInputPointer
                {
                    cua_driver_core::background_input::BackgroundAction::WindowPointer
                } else if background_menu {
                    cua_driver_core::background_input::BackgroundAction::ApplicationMenuSemantic
                } else if background_popover {
                    cua_driver_core::background_input::BackgroundAction::AttachedPopoverSemantic
                } else {
                    cua_driver_core::background_input::BackgroundAction::AxSemantic
                };
                match super::gate_background_window_action(pid, wid, Some(element_ptr), gate_action)
                    .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal_result) => return refusal_result,
                }
            } else {
                None
            };

            // Animate cursor to element center BEFORE firing AX action,
            // mirroring Swift's `performElementClick` → `animateAndWait(to:)`.
            let center_ptr = element_ptr;
            let center = tokio::task::spawn_blocking(move || unsafe {
                crate::ax::bindings::element_screen_center(center_ptr as AXUIElementRef)
            })
            .await
            .ok()
            .flatten();

            // Surface 5: button=middle on the AX path has no AX equivalent.
            // Fall back to a pixel middle-click at the element's screen-space center
            // so the request still produces a real middle-button event (browser tab
            // close, autoscroll, etc.). If we can't resolve a center, error rather
            // than silently degrade to AXPress.
            if button_str == "middle" {
                let (cx, cy) = match center {
                    Some(c) => c,
                    None => {
                        return ToolResult::error(
                            "click(button=middle) on element_index: could not resolve element \
                         center for the pixel-middle-click fallback. Pass x, y directly.",
                        )
                    }
                };
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                crate::cursor::overlay::animate_click_feedback(cursor_key.clone(), cx, cy).await;
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, cx, cy);

                let mods_owned = modifiers.clone();
                let foreground = delivery_mode.is_foreground();
                let middle_app_context_route = app_context_route.clone();
                let result = tokio::task::spawn_blocking(move || {
                    super::ensure_app_context_delegation_live(
                        middle_app_context_route.as_ref(),
                    )?;
                    unsafe {
                        super::ensure_app_context_element_window(
                            middle_app_context_route.as_ref(),
                            element_ptr as AXUIElementRef,
                        )?;
                    }
                    let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                    if foreground {
                        crate::input::skylight::with_foreground_hid_activation_delegated(
                            pid as libc::pid_t,
                            wid,
                            middle_app_context_route,
                            || {
                                if m.is_empty() {
                                    crate::input::mouse::middle_click_at_xy(pid, cx, cy, &m)
                                } else {
                                    crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                                        cx, cy, 1, "middle", &m,
                                    )
                                }
                            },
                        )
                    } else {
                        crate::input::mouse::middle_click_at_xy(pid, cx, cy, &m)
                    }
                })
                .await;
                return match result {
                    Ok(Ok(())) => ToolResult::text(format!(
                        "✅ Posted middle-click to pid {pid} at element [{idx}] center \
                         (background CGEvent; not driver-verified — confirm via screenshot)."
                    ))
                    .with_structured(serde_json::json!({ "path": "cgevent", "verified": false, "effect": "unverifiable" })),
                    Ok(Err(e)) => ToolResult::error(format!("Middle-click failed: {e}")),
                    Err(e)     => ToolResult::error(format!("Task error: {e}")),
                };
            }

            let async_click_feedback = if let Some((cx, cy)) = center {
                // Pin overlay above target window first.
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
                let asynchronous =
                    crate::cursor::overlay::animate_click_feedback(cursor_key.clone(), cx, cy)
                        .await;
                // Keep the registry in sync with the overlay so
                // get_agent_cursor_state reports a truthful position even when
                // the click was dispatched via the AX path (no pixel coords).
                self.state
                    .cursor_registry
                    .update_position(&cursor_key, cx, cy);
                asynchronous
            } else {
                false
            };

            // Finder icon/list items can expose a readable AXSelected state
            // while refusing both AXSelected writes and AXPress. Resolve a
            // verified coordinate frame only for those collection-like
            // elements so perform_ax_click can cross that one failed semantic
            // rung internally and confirm the result by AX read-back.
            let selection_candidate = effective_action == "press"
                && !background_menu
                && !background_popover
                && selectable_ancestry;
            let mut selection_pixel = if selection_candidate {
                if let Some((cx, cy)) = center {
                    super::px_frame::resolve_or_refuse(wid)
                        .await
                        .ok()
                        .map(|frame| SelectionPixelTarget {
                            screen_x: cx,
                            screen_y: cy,
                            window_x: cx - frame.bounds.x,
                            window_y: cy - frame.bounds.y,
                        })
                } else {
                    None
                }
            } else {
                None
            };
            // The selection fallback delivers a routed window-local pixel
            // click — a stricter (WindowPointer) rung than the semantic gate
            // above. In background, drop the fallback rather than silently
            // escalate when the pointer rung would refuse (e.g. a
            // minimized/hidden target); the semantic path still runs.
            if selection_pixel.is_some()
                && !delivery_mode.is_foreground()
                && _mutation_lease
                    .as_ref()
                    .expect("background element actions hold the per-pid lease")
                    .gate_again(
                        wid,
                        Some(element_ptr),
                        cua_driver_core::background_input::BackgroundAction::WindowPointer,
                    )
                    .await
                    .is_err()
            {
                selection_pixel = None;
            }

            if background_menu
                || background_popover
                || element_route == ElementClickRoute::TextInputPointer
            {
                // Cursor feedback may have yielded while a dialog opened.
                // Keep the original modal/helper guards immediately before
                // the separate semantic menu dispatch as well.
                if let Err(refusal) = super::guard_same_pid_transient_target(pid, Some(wid)).await {
                    return refusal;
                }
                if let Err(refusal) = super::guard_transient_pointer_target(
                    &self.state,
                    &transient_session,
                    pid,
                    Some(wid),
                )
                .await
                {
                    return refusal;
                }
            }

            // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
            // Capture prior frontmost and arm one target-only suppression
            // lease for the complete background snapshot -> AX action ->
            // detect interval. A user switch to an unrelated app must not be
            // undone. After the action returns, detect any window / foreground
            // side-effects and append the usual result suffix.
            let prior_front = apps::frontmost_pid();
            let foreground = delivery_mode.is_foreground();
            let snapshot = if foreground {
                WindowChangeDetector::snapshot_without_suppression(prior_front)
            } else {
                WindowChangeDetector::snapshot_targeted(prior_front, pid)
            };

            // Run AX work on a blocking thread (can't block async executor).
            // Use `effective_action` so button=right rewrites press → show_menu.
            let action_clone = effective_action.clone();
            // Thread the resolved session cursor key into the blocking AX path
            // so its ShowFocusRect + ClickPulse land on THIS session's cursor,
            // not the shared "default" one (which would light the wrong cursor
            // and stomp default for a non-default session).
            let ck = cursor_key.clone();
            let selection_modifiers = modifiers.clone();
            let ax_app_context_route = app_context_route.clone();
            let result = focus_guard::with_focus_suppressed(
                // The observation snapshot owns the canonical target-only
                // lease; foreground delivery owns its activation.
                None,
                prior_front,
                "click.AXPress",
                || async move {
                    tokio::task::spawn_blocking(move || {
                        super::ensure_app_context_delegation_live(ax_app_context_route.as_ref())?;
                        unsafe {
                            super::ensure_app_context_element_window(
                                ax_app_context_route.as_ref(),
                                element_ptr as AXUIElementRef,
                            )?;
                        }
                        if element_route == ElementClickRoute::TextInputPointer {
                            perform_text_input_pointer_click(
                                element_ptr,
                                idx,
                                pid,
                                wid,
                                count,
                                foreground,
                                ax_app_context_route,
                            )
                        } else if foreground {
                            let mut outcome = None;
                            let has_modifiers = !selection_modifiers.is_empty();
                            let action = || {
                                outcome = Some(perform_ax_click(
                                    element_ptr,
                                    idx,
                                    pid,
                                    wid,
                                    &action_clone,
                                    &ck,
                                    selection_pixel,
                                    &selection_modifiers,
                                    foreground,
                                    !async_click_feedback,
                                )?);
                                std::thread::sleep(std::time::Duration::from_millis(150));
                                Ok(())
                            };
                            let fronted = if has_modifiers {
                                crate::input::skylight::with_foreground_hid_activation_delegated(
                                    pid as libc::pid_t,
                                    wid,
                                    ax_app_context_route,
                                    action,
                                )?;
                                true
                            } else {
                                crate::input::skylight::with_foreground_assist_delegated(
                                    pid as libc::pid_t,
                                    wid,
                                    ax_app_context_route,
                                    action,
                                )?
                            };
                            let outcome = outcome.ok_or_else(|| {
                                anyhow::anyhow!("foreground AX click did not execute")
                            })?;
                            Ok((outcome, fronted))
                        } else if background_menu {
                            perform_application_menu_click(
                                element_ptr,
                                idx,
                                pid,
                                wid,
                                &action_clone,
                            )
                            .map(|outcome| (outcome, false))
                        } else if background_popover {
                            perform_attached_popover_click(
                                element_ptr,
                                idx,
                                pid,
                                wid,
                                &action_clone,
                            )
                            .map(|outcome| (outcome, false))
                        } else {
                            perform_ax_click(
                                element_ptr,
                                idx,
                                pid,
                                wid,
                                &action_clone,
                                &ck,
                                selection_pixel,
                                &selection_modifiers,
                                false,
                                !async_click_feedback,
                            )
                            .map(|outcome| (outcome, false))
                        }
                    })
                    .await
                },
            )
            .await;

            // Start the settle clock only after dispatch has completed. Time
            // already spent reporting window changes counts toward the same
            // minimum interval; it must not add a second full settle budget.
            let click_completed_at = std::time::Instant::now();

            // Detect side-effects while preserving the configured protection tail.
            let changes = super::finish_window_observation(snapshot, &args).await;

            match result {
                Ok(Ok((
                    (
                        mut msg,
                        needs_text_input_settle,
                        suspected_noop,
                        selection_verified,
                        used_pixel,
                    ),
                    fronted,
                ))) => {
                    // Allow a small focus settle without treating it as a DOM
                    // readiness oracle or upgrading the action's evidence.
                    if needs_text_input_settle {
                        let remaining =
                            remaining_text_input_focus_settle(click_completed_at.elapsed());
                        if !remaining.is_zero() {
                            tokio::time::sleep(remaining).await;
                        }
                    }
                    msg.push_str(&changes.result_suffix());
                    // AX dispatch went through, but AXPerformAction returning
                    // success does not confirm the on-screen effect (many elements
                    // no-op silently). A click is never driver-verifiable (no
                    // read-back) → verified:false stays for back-compat. The
                    // tri-state `effect` is the richer signal:
                    //   * suspected_noop — the element didn't advertise the action,
                    //     so the press likely did nothing → cross to vision/pixel.
                    //   * unverifiable — dispatched fine, driver just can't confirm;
                    //     the caller verifies via screenshot.
                    let mut structured = element_click_result(
                        fronted,
                        used_pixel,
                        selection_verified,
                        suspected_noop,
                    );
                    if selection_verified {
                        structured["evidence"] = serde_json::json!([
                            { "kind": "accessibility_readback" }
                        ]);
                    }
                    if suspected_noop {
                        structured["escalation"] = serde_json::json!({
                            "recommended": "px",
                            "reason": "element does not advertise this action — the \
                                       AX press likely no-op'd. Do an element px \
                                       action: click by pixel (x,y) off the \
                                       screenshot from get_window_state."
                        });
                    }
                    ToolResult::text(msg).with_structured(structured)
                }
                Ok(Err(e)) => {
                    if let Some(refusal) = e.downcast_ref::<ApplicationMenuRefusal>() {
                        super::background_refusal_result(pid, wid, &refusal.0)
                    } else if let Some(refusal) = e.downcast_ref::<ElementPointerRefusal>() {
                        refusal.result(pid, wid)
                    } else if element_route == ElementClickRoute::TextInputPointer {
                        ToolResult::error(format!("Element pointer click failed: {e}"))
                    } else {
                        ToolResult::error(format!("AX action failed: {e}"))
                    }
                }
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            }
        } else if let (Some(mut cx), Some(mut cy)) = (x, y) {
            // ── Pixel path ─────────────────────────────────────────────────

            // debug_image_out: capture fresh screenshot, overlay crosshair BEFORE
            // any coordinate translation (so it shows received coords in the same
            // space the caller was reasoning in).
            if let Some(ref dbg_path) = debug_image_out {
                if from_zoom {
                    return ToolResult::error(
                        "debug_image_out is incompatible with from_zoom — \
                         received (x, y) would be in zoom-crop space, not window-local.",
                    );
                }
                match window_id {
                    None => return ToolResult::error("debug_image_out requires window_id."),
                    Some(wid) => {
                        // Session-effective max dimension so debug_image_out
                        // matches the resize the calling session sees in
                        // get_window_state (precedence: session override > global).
                        let max_dim = self.state.session_config.effective_max_image_dimension(
                            args.opt_str("_session_id").as_deref(),
                            &self.state.config.read().unwrap(),
                        );
                        let dbg_path_c = dbg_path.clone();
                        let dbg_result = tokio::task::spawn_blocking(move || {
                            let png = crate::capture::screenshot_window_bytes(wid)?;
                            let png = crate::capture::resize_png_if_needed(&png, max_dim)?;
                            crate::capture::write_crosshair_png(&png, cx, cy, &dbg_path_c)
                        })
                        .await;
                        match dbg_result {
                            Err(e) => {
                                return ToolResult::error(format!(
                                    "debug_image_out task failed: {e}. Not dispatching click."
                                ))
                            }
                            Ok(Err(e)) => {
                                return ToolResult::error(format!(
                                    "debug_image_out write failed: {e}. Not dispatching click."
                                ))
                            }
                            Ok(Ok(())) => {}
                        }
                    }
                }
            }

            if from_zoom {
                match self.state.zoom_registry.get(pid) {
                    Some(ctx) => {
                        let (wx, wy) = ctx.zoom_to_window(cx, cy);
                        cx = wx;
                        cy = wy;
                    }
                    None => {
                        return ToolResult::error(format!(
                            "from_zoom=true but no zoom context for pid {pid}. Call zoom first."
                        ))
                    }
                }
            } else if let Some(ratio) = self.state.resize_registry.ratio(pid, window_id) {
                // Coordinates are in the downscaled image space; scale back to native pixels.
                cx *= ratio;
                cy *= ratio;
            }

            // ── Window-local → screen coordinate translation ──────────────────
            // `click_at_xy` accepts screen-space coordinates (top-left origin).
            // Callers supply window-local screenshot pixels; `px_frame` adds the
            // window's screen-origin (and divides out the Retina backing scale)
            // to produce the final screen position, or refuses when the window
            // has no live frame — see px_frame's module docs for why there is
            // no screen-absolute fallback.
            //
            // win_local_x/y: window-local logical-pixel coords needed for
            // CGEventSetWindowLocation in the Chromium recipe.
            let (screen_x, screen_y, win_local_x, win_local_y) = if let Some(wid) = window_id {
                match super::px_frame::resolve_or_refuse(wid).await {
                    Ok(frame) => {
                        let (sx, sy, lx, ly) = frame.to_screen(cx, cy);
                        // A window-local point outside the live frame would
                        // dispatch onto whatever occupies that screen point —
                        // the same wrong-surface misclick class as #2237.
                        // Refuse in background, where the caller cannot see
                        // what is actually under the translated point.
                        if !delivery_mode.is_foreground()
                            && (lx < 0.0
                                || ly < 0.0
                                || lx > frame.bounds.width
                                || ly > frame.bounds.height)
                        {
                            return ToolResult::error(format!(
                                "click: window-local point ({lx:.1}, {ly:.1}) pt lies outside \
                                 window {wid}'s {:.0}×{:.0} pt frame; background delivery \
                                 refused. Re-read coordinates from a fresh get_window_state \
                                 screenshot.",
                                frame.bounds.width, frame.bounds.height
                            ));
                        }
                        (sx, sy, lx, ly)
                    }
                    Err(refusal) => return refusal,
                }
            } else {
                // No window_id → treat x,y as screen coordinates (legacy behaviour).
                (cx, cy, cx, cy)
            };

            // ── Exact-target background gate (macOS background input v1) ──
            // A window-addressed background pixel action targets coordinates,
            // which only mean something while the exact window is current and
            // not minimized/hidden: a stale target would let the pid-scoped
            // hit-test or routed events land on a same-process sibling. Gate
            // BEFORE the AX hit-test backend and any cursor/dispatch work.
            // delivery_mode:"foreground" stays the explicit last resort.
            let mutation_lease_held = crate::background_mutation::held_by_current_task(pid);
            let _mutation_lease = if !delivery_mode.is_foreground() && !mutation_lease_held {
                if let Some(wid) = window_id {
                    match super::gate_background_window_action(
                        pid,
                        wid,
                        None,
                        cua_driver_core::background_input::BackgroundAction::WindowPointer,
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

            // A background PX action can still use an accessibility delivery
            // backend after resolving the requested screen point. This keeps
            // targeting (PX) orthogonal to delivery (AX) and avoids making a
            // Chromium/AppKit window key merely to satisfy first-mouse rules.
            if !delivery_mode.is_foreground()
                && window_id.is_some()
                && button_str == "left"
                && count == 1
                && modifiers.is_empty()
            {
                let focus_only = action == "focus";
                let press_only = action == "press";
                let hit_test_wid = window_id.expect("guarded by window_id.is_some() above");
                let ax_result = tokio::task::spawn_blocking(move || unsafe {
                    let Some(element) = element_at_screen_position(pid, screen_x, screen_y) else {
                        return Ok::<bool, anyhow::Error>(false);
                    };
                    // The pid-scoped hit-test can resolve an element from a
                    // same-process sibling overlapping the requested point.
                    // Require proven ancestry in the requested window before
                    // acting; otherwise fall through to the routed pixel path
                    // (already gated for this exact window).
                    if crate::ax::exact_target::element_window_id(element) != Some(hit_test_wid) {
                        CFRelease(element as _);
                        return Ok(false);
                    }
                    let role = copy_string_attr(element, "AXRole").unwrap_or_default();
                    let enabled = copy_bool_attr(element, "AXEnabled");
                    let advertised_actions = copy_action_names(element);
                    let delivered = if focus_only {
                        crate::input::ax_actions::focus_element(element as usize).is_ok()
                    } else if !background_pixel_ax_press_eligible(
                        press_only,
                        &role,
                        &advertised_actions,
                        enabled,
                    ) {
                        false
                    } else {
                        let press = core_foundation::string::CFString::new("AXPress");
                        AXUIElementPerformAction(element, press.as_concrete_TypeRef())
                            == kAXErrorSuccess
                    };
                    CFRelease(element as _);
                    Ok(delivered)
                })
                .await;
                match ax_result {
                    Ok(Ok(true)) => {
                        let label = if focus_only { "focused" } else { "pressed" };
                        return ToolResult::text(format!(
                            "✅ PX hit-test {label} the background element via AX."
                        ))
                        .with_structured(serde_json::json!({
                            "path": "ax",
                            "verified": false,
                            "effect": "unverifiable"
                        }));
                    }
                    Ok(Ok(false)) if focus_only => {
                        return ToolResult::error(
                            "Background PX focus is unavailable at the requested point.".to_owned(),
                        )
                        .with_structured(serde_json::json!({
                            "code": "background_unavailable",
                            "effect": "refused"
                        }));
                    }
                    Ok(Err(error)) if focus_only => {
                        return ToolResult::error(format!("Background PX focus failed: {error}"))
                            .with_structured(serde_json::json!({
                                "code": "background_unavailable",
                                "effect": "refused"
                            }));
                    }
                    _ => {}
                }
            }

            // Resolve the effective delivery posture before observation. A
            // requested foreground click without a window id still degrades to
            // background, matching the existing contract and result label.
            let fg = delivery_mode.is_foreground() && window_id.is_some();
            let activation_policy = pixel_activation_policy(&button_str, fg, window_id.is_some());

            // Pin the overlay above the target window before any animation so
            // the cursor is already sandwiched correctly while it glides in.
            if let Some(wid) = window_id {
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                );
            }
            let async_click_feedback_requested =
                crate::cursor::overlay::async_click_feedback_enabled(&cursor_key);
            if !async_click_feedback_requested {
                crate::cursor::overlay::animate_cursor_to(cursor_key.clone(), screen_x, screen_y)
                    .await;
            }
            // Keep the registry in sync with the overlay (see AX path above).
            self.state
                .cursor_registry
                .update_position(&cursor_key, screen_x, screen_y);

            // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
            // A pixel click can land on a "Sign In" button that opens a sheet
            // or a Safari link that activates a new tab. Standard background
            // delivery and synthetic target focus suppress only real target
            // self-activation. Synthetic focus records that do not change the
            // real frontmost pid are ignored by the suppressor's compare step.
            // Foreground assist intentionally owns activation state, so the
            // detector observes changes without a competing lease.
            let prior_front = apps::frontmost_pid();
            let snapshot = match activation_policy {
                PixelActivationPolicy::SuppressTarget
                | PixelActivationPolicy::SyntheticTargetFocus => {
                    WindowChangeDetector::snapshot_targeted(prior_front, pid)
                }
                PixelActivationPolicy::ForegroundAssist => {
                    WindowChangeDetector::snapshot_without_suppression(prior_front)
                }
            };

            // Chromium needs its internal event-routing state to appear active
            // before it accepts a click for a fully covered window. Install that
            // state only on the target. The old focus-without-raise prologue also
            // sent `focused=false` to the real foreground app, which could fire
            // resignActive/resignKey and steal the user's keyboard destination
            // despite leaving window z-order unchanged.
            let synthetic_focus_context =
                if activation_policy == PixelActivationPolicy::SyntheticTargetFocus {
                    let wid = window_id.expect("activation policy requires window_id");
                    match tokio::task::spawn_blocking(move || {
                        crate::input::mouse::prepare_background_pixel_click(pid, wid)
                    })
                    .await
                    {
                        Ok(Ok(context)) => {
                            crate::cursor::overlay::send_command(
                                cursor_key.clone(),
                                cursor_overlay::OverlayCommand::PinAbove(wid as u64),
                            );
                            context
                        }
                        Ok(Err(error)) => {
                            return ToolResult::error(format!(
                                "Background click target-only focus failed: {error}"
                            ))
                            .with_structured(serde_json::json!({
                                "code": "background_unavailable",
                                "effect": "refused"
                            }));
                        }
                        Err(error) => {
                            return ToolResult::error(format!(
                                "Background click activation task failed: {error}"
                            ));
                        }
                    }
                } else {
                    None
                };
            let used_synthetic_target_focus = synthetic_focus_context.is_some();

            // In asynchronous mode, do not start cosmetic feedback until all
            // target-validation and Chromium focus preparation has completed.
            // This keeps a fast glide from pulsing before the actual click is
            // ready to dispatch. If the renderer queue is unavailable, retain
            // the existing immediate pulse as a best-effort fallback.
            let async_click_feedback = async_click_feedback_requested
                && crate::cursor::overlay::queue_async_click_feedback(
                    cursor_key.clone(),
                    screen_x,
                    screen_y,
                );
            if !async_click_feedback {
                // Synchronized mode preserves the existing timing: pulse only
                // after the private activation prelude has settled.
                crate::cursor::overlay::send_command(
                    cursor_key.clone(),
                    cursor_overlay::OverlayCommand::ClickPulse {
                        x: screen_x,
                        y: screen_y,
                    },
                );
            }

            let mods_owned = modifiers.clone();
            // Surface 5: route to the right/middle CGEvent primitives when
            // button != left. Left-button path stays on the existing Chromium-
            // routed `click_at_xy_with_window_local` for back-compat.
            let button_kind = button_str.clone();
            let pixel_app_context_route = app_context_route.clone();
            let result = focus_guard::with_focus_suppressed(
                // The observation snapshot owns the canonical target-only
                // lease, including synthetic target-focus delivery.
                None,
                prior_front,
                "click.pixel",
                || async move {
                    tokio::task::spawn_blocking(move || {
                        super::ensure_app_context_delegation_live(
                            pixel_app_context_route.as_ref(),
                        )?;
                        let do_click = move || -> anyhow::Result<()> {
                            let m: Vec<&str> = mods_owned.iter().map(String::as_str).collect();
                            if fg {
                                // Foreground pixel delivery must behave like a real
                                // user gesture. Custom canvases such as Blender's
                                // GHOST regions choose their input context from the
                                // global HID pointer stream and ignore PID-routed
                                // mouse events even while their window is frontmost.
                                return crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                                    screen_x,
                                    screen_y,
                                    count,
                                    &button_kind,
                                    &m,
                                );
                            }
                            match button_kind.as_str() {
                                "right" => {
                                    if let Some(wid) = window_id {
                                        return crate::input::mouse::right_click_at_xy_with_window_local(
                                            pid, screen_x, screen_y, win_local_x, win_local_y, wid, &m,
                                        );
                                    }
                                    crate::input::mouse::right_click_at_xy(pid, screen_x, screen_y, &m)
                                }
                                "middle" => {
                                    if let Some(_wid) = window_id {
                                        return crate::input::mouse::middle_click_at_xy_with_window_local(
                                            pid, screen_x, screen_y, win_local_x, win_local_y, &m,
                                        );
                                    }
                                    crate::input::mouse::middle_click_at_xy(pid, screen_x, screen_y, &m)
                                }
                                // "left" (default) or anything else — preserve legacy left-click path.
                                _ => {
                                    // When we know the window_id, pass the window-local coordinates so
                                    // `click_at_xy_with_window_local` can stamp `CGEventSetWindowLocation`
                                    // and Chromium-specific fields (f40, f51, f58, f91, f92) onto events
                                    // for better backgrounded-target delivery.
                                    if let Some(wid) = window_id {
                                        return crate::input::mouse::click_at_xy_with_window_local(
                                            pid, screen_x, screen_y,
                                            win_local_x, win_local_y,
                                            wid, count, &m,
                                            crate::input::mouse::WindowClickDelivery::from_foreground(fg),
                                            synthetic_focus_context,
                                        );
                                    }
                                    crate::input::mouse::click_at_xy(pid, screen_x, screen_y, count, &m)
                                }
                            }
                        };
                        // Foreground rung: brief front → click → restore.
                        // Returns whether the window was ACTUALLY fronted, so the
                        // reported `path` honestly reflects the rung that ran.
                        match (fg, window_id) {
                            (true, Some(wid)) => {
                                crate::input::skylight::with_foreground_hid_activation_delegated(
                                    pid as libc::pid_t,
                                    wid,
                                    pixel_app_context_route,
                                    do_click,
                                )
                                .map(|_| true)
                            }
                            _ => do_click().map(|_| false),
                        }
                    })
                    .await
                },
            )
            .await;

            let changes = super::finish_window_observation(snapshot, &args).await;

            let button_label = match button_str.as_str() {
                "right" => "right-click",
                "middle" => "middle-click",
                _ => "click",
            };
            match result {
                Ok(Ok(fronted)) => {
                    // `with_foreground_assist` returns `false` when the fronting SPIs
                    // were unavailable and it clicked WITHOUT activation — report the
                    // background path in that case so `path` reflects the rung that ran.
                    let (path, mode_label) = if fg && fronted {
                        ("cgevent_fg", "foreground CGEvent")
                    } else {
                        ("cgevent", "background CGEvent")
                    };
                    ToolResult::text(format!(
                        "✅ Posted {button_label} to pid {pid} ({mode_label}; \
                         not driver-verified — confirm via screenshot).{}",
                        changes.result_suffix()
                    ))
                    .with_structured(serde_json::json!({
                        "path": path,
                        "verified": false,
                        "effect": "unverifiable",
                        "focus_without_raise": used_synthetic_target_focus,
                        "synthetic_target_focus": used_synthetic_target_focus
                    }))
                }
                Ok(Err(e)) => ToolResult::error(format!("{button_label} failed: {e}")),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            }
        } else {
            ToolResult::error(
                "Provide either (element_index + window_id) or (x + y). pid is always required.",
            )
        }
    }
}

// ── AX click implementation (blocking) ───────────────────────────────────────

#[derive(Debug)]
enum ElementPointerRefusal {
    Frame(super::px_frame::PxFrameError),
    Target(&'static str),
}

impl ElementPointerRefusal {
    fn result(&self, pid: i32, window_id: u32) -> ToolResult {
        match self {
            Self::Frame(error) => {
                let mut result = super::px_frame::refusal(error);
                if let Some(structured) = result.structured_content.as_mut() {
                    structured["effect"] = serde_json::json!("refused");
                }
                result
            }
            Self::Target(reason) => ToolResult::error(format!(
                "Element pointer click refused: {reason}. No pointer input was sent."
            ))
            .with_structured(serde_json::json!({
                "code": "element_pointer_unavailable",
                "effect": "refused",
                "pid": pid,
                "window_id": window_id,
                "reason": reason,
            })),
        }
    }
}

impl std::fmt::Display for ElementPointerRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(error) => {
                write!(formatter, "text-input pointer frame unavailable: {error:?}")
            }
            Self::Target(reason) => formatter.write_str(reason),
        }
    }
}
impl std::error::Error for ElementPointerRefusal {}

/// Re-read the retained element and its exact window, not a cached coordinate
/// or hit-test substitute. The selected route may become unavailable but may
/// not change actuators after selection, focus preparation, or dispatch.
fn live_text_input_pointer_target(
    element_ptr: usize,
    pid: i32,
    window_id: u32,
    frame: &super::px_frame::WindowPxFrame,
    foreground: bool,
    delegation: Option<&crate::ax::app_context::AppContextDelegationRoute>,
) -> anyhow::Result<SelectionPixelTarget> {
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundAction, BackgroundInputDecision, ElementAncestry,
        ExactWindowTarget, WindowServerOwnership,
    };
    super::ensure_app_context_delegation_live(delegation)?;
    let element = element_ptr as AXUIElementRef;
    unsafe { super::ensure_app_context_element_window(delegation, element)? };
    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let advertised = unsafe { copy_action_names(element) };
    let selectable =
        crate::input::ax_actions::nearest_container_selection_state(element_ptr).is_some();
    if element_click_route(
        "press",
        "left",
        false,
        &role,
        &advertised,
        selectable,
        text_input_pointer_has_auxiliary_window(element_ptr),
    ) != ElementClickRoute::TextInputPointer
    {
        return Err(ElementPointerRefusal::Target(
            "the retained element no longer qualifies for the selected text-input route",
        )
        .into());
    }
    if unsafe { copy_bool_attr(element, "AXEnabled") } != Some(true) {
        return Err(ElementPointerRefusal::Target(
            "the text input is disabled or its enabled state is unproven",
        )
        .into());
    }
    let facts = crate::ax::exact_target::gather_background_facts(pid, window_id, Some(element_ptr));
    if foreground {
        if facts.window_server != WindowServerOwnership::SamePid
            || facts.element != ElementAncestry::ProvenDescendant
        {
            return Err(ElementPointerRefusal::Target(
                "live ownership and element ancestry do not prove the exact requested window",
            )
            .into());
        }
    } else if let BackgroundInputDecision::Refuse(refusal) = decide_background_input(
        ExactWindowTarget { pid, window_id },
        &facts,
        BackgroundAction::WindowPointer,
    ) {
        return Err(ApplicationMenuRefusal(refusal).into());
    }
    let live_bounds = crate::windows::window_bounds_by_id(window_id);
    text_input_pointer_target(
        unsafe { element_screen_rect(element) },
        &frame.bounds,
        live_bounds.as_ref(),
    )
    .ok_or_else(|| {
        ElementPointerRefusal::Target(
            "the element center is unavailable/outside its window or the window frame changed",
        )
        .into()
    })
}

/// Dispatch exactly one requested pointer gesture. This helper is reached only
/// after route selection and runs inside the shared focus-suppression lifetime.
/// Neither a preparation failure nor a native dispatch failure reaches AXPress.
fn perform_text_input_pointer_click(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    window_id: u32,
    count: usize,
    foreground: bool,
    delegation: Option<crate::ax::app_context::AppContextDelegationRoute>,
) -> anyhow::Result<((String, bool, bool, bool, bool), bool)> {
    let frame = super::px_frame::resolve_window_px_frame(window_id)
        .map_err(ElementPointerRefusal::Frame)?;
    // Refuse invalid/disabled targets before even preparing synthetic focus or
    // using the caller-authorized foreground activation helper.
    live_text_input_pointer_target(
        element_ptr,
        pid,
        window_id,
        &frame,
        foreground,
        delegation.as_ref(),
    )?;
    if foreground {
        crate::input::skylight::with_foreground_hid_activation_delegated(
            pid as libc::pid_t,
            window_id,
            delegation.clone(),
            || {
                let target = live_text_input_pointer_target(
                    element_ptr,
                    pid,
                    window_id,
                    &frame,
                    true,
                    delegation.as_ref(),
                )?;
                crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                    target.screen_x,
                    target.screen_y,
                    count,
                    "left",
                    &[],
                )
            },
        )?;
    } else {
        let focus_context = crate::input::mouse::prepare_background_pixel_click(pid, window_id)?;
        // Preparation may yield to the target application. Re-prove the exact
        // retained ancestry, enabled state, and frame immediately before input.
        let target = live_text_input_pointer_target(
            element_ptr,
            pid,
            window_id,
            &frame,
            false,
            delegation.as_ref(),
        )?;
        crate::input::mouse::click_at_xy_with_window_local(
            pid,
            target.screen_x,
            target.screen_y,
            target.window_x,
            target.window_y,
            window_id,
            count,
            &[],
            crate::input::mouse::WindowClickDelivery::Background,
            focus_context,
        )?;
    }
    Ok((
        (
            format!(
                "Posted primary pointer click (count {count}) to text input [{idx}] in window \
                 {window_id}; not driver-verified — confirm via a fresh state snapshot."
            ),
            true,
            false,
            false,
            true,
        ),
        foreground,
    ))
}

/// Preserve a late pre-dispatch refusal as a typed no-effect result.
#[derive(Debug)]
struct ApplicationMenuRefusal(cua_driver_core::background_input::BackgroundRefusal);

impl std::fmt::Display for ApplicationMenuRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0.reason)
    }
}
impl std::error::Error for ApplicationMenuRefusal {}

/// The menu route deliberately cannot reach perform_ax_click's selection,
/// pointer or ancestor-action fallbacks. Its single actuator is the requested
/// advertised AX action, checked against fresh app-local context after any
/// cursor animation and immediately before dispatch.
fn perform_application_menu_click(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    window_id: u32,
    action: &str,
) -> anyhow::Result<(String, bool, bool, bool, bool)> {
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundAction, BackgroundInputDecision, ExactWindowTarget,
    };
    let element = element_ptr as AXUIElementRef;
    let facts = crate::ax::exact_target::gather_background_facts(pid, window_id, Some(element_ptr));
    if let BackgroundInputDecision::Refuse(refusal) = decide_background_input(
        ExactWindowTarget { pid, window_id },
        &facts,
        BackgroundAction::ApplicationMenuSemantic,
    ) {
        return Err(ApplicationMenuRefusal(refusal).into());
    }
    let advertised = unsafe { copy_action_names(element) };
    let native = crate::ax::application_menu::advertised_menu_action(action, &advertised)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "application-menu action is not supported and advertised; take a fresh snapshot"
            )
        })?;
    crate::input::ax_actions::ensure_ax_action_enabled(element_ptr, native)?;
    let error = unsafe { crate::ax::bindings::perform_action(element, native) };
    if error != kAXErrorSuccess {
        anyhow::bail!(
            "AXUIElementPerformAction({native}) returned {error}; no fallback was attempted"
        );
    }
    Ok((format!("Performed {native} on observed application-menu element [{idx}]; verify the visible effect."),
        false, false, false, false))
}

/// No generic AX selection/ancestor fallback: re-prove the live attachment
/// after cursor feedback, then execute exactly the advertised semantic action.
fn perform_attached_popover_click(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    window_id: u32,
    action: &str,
) -> anyhow::Result<(String, bool, bool, bool, bool)> {
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundAction, BackgroundInputDecision, ExactWindowTarget,
    };
    let element = element_ptr as AXUIElementRef;
    let facts = crate::ax::exact_target::gather_background_facts(pid, window_id, Some(element_ptr));
    if let BackgroundInputDecision::Refuse(refusal) = decide_background_input(
        ExactWindowTarget { pid, window_id },
        &facts,
        BackgroundAction::AttachedPopoverSemantic,
    ) {
        return Err(ApplicationMenuRefusal(refusal).into());
    }
    let advertised = unsafe { copy_action_names(element) };
    let native =
        crate::ax::attached_popover::advertised_action(action, &advertised).ok_or_else(|| {
            anyhow::anyhow!(
                "attached-popover action is not supported and advertised; no input was sent"
            )
        })?;
    crate::input::ax_actions::ensure_ax_action_enabled(element_ptr, native)?;
    let error = unsafe { crate::ax::bindings::perform_action(element, native) };
    if error != kAXErrorSuccess {
        anyhow::bail!(
            "AXUIElementPerformAction({native}) returned {error}; no fallback was attempted"
        );
    }
    Ok((format!("Performed {native} on observed host-attached popover element [{idx}]; verify the visible effect."), false, false, false, false))
}

/// Returns `(summary_text, needs_text_input_settle, suspected_noop,
/// selection_verified, selection_via_pixel)`. An unadvertised action's AX
/// acceptance is only a suspected no-op, not a verified visible effect.
fn perform_ax_click(
    element_ptr: usize,
    idx: usize,
    pid: i32,
    window_id: u32,
    action_str: &str,
    cursor_key: &str,
    selection_pixel: Option<SelectionPixelTarget>,
    modifiers: &[String],
    foreground: bool,
    emit_click_pulse: bool,
) -> anyhow::Result<(String, bool, bool, bool, bool)> {
    let ax_action = map_action(action_str);
    let element = element_ptr as AXUIElementRef;

    // Check the live value immediately before dispatch. Foreground assist can
    // enable menu items that were disabled in the cached snapshot, while a
    // background transition can disable them after that snapshot. macOS may
    // otherwise return success for a disabled action that did nothing.
    crate::input::ax_actions::ensure_ax_action_enabled(element_ptr, ax_action)?;

    // Capture advertised actions BEFORE dispatching so we can detect silent no-ops
    // (AX returns success even when the element doesn't advertise the action).
    let advertised = unsafe { copy_action_names(element) };

    let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
    let title = unsafe { copy_string_attr(element, "AXTitle") }.unwrap_or_default();

    // A click on an AppKit collection item is frequently represented by a
    // label child or row that does not advertise AXPress. Prefer a bounded,
    // read-back-verified AXSelected write over dispatching a known hollow press
    // or forcing the caller onto a less stable pixel coordinate.
    if ax_action == "AXPress" && !advertised.iter().any(|action| action == ax_action) {
        if modifiers.is_empty() {
            if let Some(selected_role) =
                crate::input::ax_actions::select_nearest_container(element_ptr)
            {
                return Ok((
                    format!(
                        "✅ Selected nearest {selected_role} for [{idx}] {role} \"{title}\"; \
                         confirmed AXSelected=true."
                    ),
                    false,
                    false,
                    true,
                    false,
                ));
            }
        }

        if let (Some(target), Some(selection)) = (
            selection_pixel,
            crate::input::ax_actions::capture_nearest_container_selection(element_ptr),
        ) {
            let selected_role = selection.role().to_owned();
            let Some((before, _)) = selection.observe() else {
                anyhow::bail!("selection target stopped exposing AXSelected before delivery");
            };
            let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
            if foreground && !modifier_refs.is_empty() {
                crate::input::mouse::click_at_xy_desktop_with_modifiers_preserving_cursor(
                    target.screen_x,
                    target.screen_y,
                    1,
                    "left",
                    &modifier_refs,
                )?;
            } else {
                crate::input::mouse::click_at_xy_with_window_local(
                    pid,
                    target.screen_x,
                    target.screen_y,
                    target.window_x,
                    target.window_y,
                    window_id,
                    1,
                    &modifier_refs,
                    crate::input::mouse::WindowClickDelivery::from_foreground(foreground),
                    None,
                )?;
            }
            // AppKit may publish a transient AXSelected transition while the
            // event queue is still resolving the gesture. Let it settle before
            // accepting a candidate, then require the same state to survive a
            // second observation. A modified selection additionally preserves
            // every peer that was selected before delivery.
            std::thread::sleep(SELECTION_READBACK_SETTLE);
            let deadline = std::time::Instant::now() + SELECTION_READBACK_TIMEOUT;
            let mut last_observation = None;
            loop {
                if let Some((after, peers_preserved)) = selection.observe() {
                    last_observation = Some((after, peers_preserved));
                    let verified = selection_readback_confirms(
                        before,
                        after,
                        !modifiers.is_empty(),
                        peers_preserved,
                    );
                    if verified {
                        std::thread::sleep(SELECTION_READBACK_STABILITY);
                        if let Some((stable_after, stable_peers_preserved)) = selection.observe() {
                            last_observation = Some((stable_after, stable_peers_preserved));
                            if stable_after == after
                                && selection_readback_confirms(
                                    before,
                                    stable_after,
                                    !modifiers.is_empty(),
                                    stable_peers_preserved,
                                )
                            {
                                return Ok((
                                    format!(
                                        "✅ Selected nearest {selected_role} for [{idx}] {role} \
                                         \"{title}\"; AX selection write was unavailable, so a \
                                         coordinate click was delivered and confirmed by stable \
                                         AXSelected read-back."
                                    ),
                                    false,
                                    false,
                                    true,
                                    true,
                                ));
                            }
                        }
                    }
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(SELECTION_READBACK_POLL);
            }
            if modifiers.is_empty() {
                anyhow::bail!(
                    "coordinate click did not produce a stable AXSelected transition; \
                     last_readback={last_observation:?}; retry after a fresh snapshot"
                );
            }
            anyhow::bail!(
                "foreground modified coordinate click did not produce a stable AXSelected \
                 transition while preserving the prior selection; \
                 before_selected={before}, last_readback={last_observation:?}; \
                 take a fresh snapshot before retrying"
            );
        }
    }

    let err = unsafe { crate::ax::bindings::perform_action(element, ax_action) };
    if err != crate::ax::bindings::kAXErrorSuccess {
        // Some collection rows claim a click-like action but Finder returns
        // kAXErrorCannotComplete. Use the same verified selection fallback
        // before surfacing the dispatch error.
        if ax_action == "AXPress" && modifiers.is_empty() {
            if let Some(selected_role) =
                crate::input::ax_actions::select_nearest_container(element_ptr)
            {
                return Ok((
                    format!(
                        "✅ Selected nearest {selected_role} for [{idx}] {role} \"{title}\" \
                         after AXPress returned {err}; confirmed AXSelected=true."
                    ),
                    false,
                    false,
                    true,
                    false,
                ));
            }
        }
        anyhow::bail!("AXUIElementPerformAction({ax_action}) returned {err}");
    }

    let mut summary = format!("✅ Performed {ax_action} on [{idx}] {role} \"{title}\".");

    // AXPopUpButton: list available options, redirect to set_value.
    if role == "AXPopUpButton" {
        let children = unsafe { copy_children(element) };
        if !children.is_empty() {
            let options: Vec<String> = children
                .iter()
                .filter_map(|&child| {
                    let t = unsafe { copy_string_attr(child, "AXTitle") }.unwrap_or_default();
                    let v = unsafe { copy_string_attr(child, "AXValue") }.unwrap_or_default();
                    if t.is_empty() && v.is_empty() {
                        return None;
                    }
                    Some(if v.is_empty() || v == t {
                        format!("\"{t}\"")
                    } else {
                        format!("\"{t}\" (value: {v})")
                    })
                })
                .collect();
            for &child in &children {
                unsafe {
                    CFRelease(child as _);
                }
            }

            if !options.is_empty() {
                let opt_list = options.join(", ");
                summary.push_str(
                    "\n\n⚠️ This is a popup/select button. The native macOS menu closes \
                     immediately when the window is in the background. Do NOT use click \
                     again — instead, use:\n  set_value(pid, window_id, element_index, value)\n\
                     Available options: [",
                );
                summary.push_str(&opt_list);
                summary.push(']');
            }
        }
    }

    // Advertised-action warning: non-fatal but surfaces likely no-ops. Also the
    // machine-readable `suspected_noop` signal returned to the caller.
    let suspected_noop = !advertised.contains(&ax_action.to_string());
    if suspected_noop {
        let adv_list = if advertised.is_empty() {
            "none".into()
        } else {
            advertised.join(", ")
        };
        summary.push_str(&format!(
            "\n⚠️ Element does not advertise {ax_action} (actions: {adv_list}). \
             Action may have been a no-op."
        ));
    }

    // Only text-input AXPress requests the small settle in the async caller.
    let needs_text_input_settle = needs_text_input_focus_settle(&role, ax_action);

    // Show focus-rect highlight around the element (matches Swift showFocusRect).
    if let Some(rect) = unsafe { element_screen_rect(element) } {
        crate::cursor::overlay::send_command(
            cursor_key.to_owned(),
            cursor_overlay::OverlayCommand::ShowFocusRect(Some(rect)),
        );
        if emit_click_pulse {
            let cx = rect[0] + rect[2] / 2.0;
            let cy = rect[1] + rect[3] / 2.0;
            crate::cursor::overlay::send_command(
                cursor_key.to_owned(),
                cursor_overlay::OverlayCommand::ClickPulse { x: cx, y: cy },
            );
        }
    }
    let _ = pid;
    let _ = window_id; // used by caller context

    Ok((
        summary,
        needs_text_input_settle,
        suspected_noop,
        false,
        false,
    ))
}

#[cfg(test)]
mod selection_fallback_tests {
    use super::selection_readback_confirms;

    #[test]
    fn plain_click_requires_selected_readback() {
        assert!(selection_readback_confirms(false, true, false, true));
        assert!(selection_readback_confirms(true, true, false, true));
        assert!(!selection_readback_confirms(false, false, false, true));
    }

    #[test]
    fn modified_click_requires_a_transition_and_preserves_prior_selection() {
        assert!(selection_readback_confirms(false, true, true, true));
        assert!(selection_readback_confirms(true, false, true, true));
        assert!(!selection_readback_confirms(true, true, true, true));
        assert!(!selection_readback_confirms(false, true, true, false));
    }
}

fn map_action(action: &str) -> &'static str {
    match action.to_lowercase().as_str() {
        "press" | "click" => "AXPress",
        "show_menu" | "right_click" => "AXShowMenu",
        "pick" => "AXPick",
        "confirm" => "AXConfirm",
        "cancel" => "AXCancel",
        "open" => "AXOpen",
        _ => "AXPress",
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unadvertised_text_input_primary_click_selects_pointer_before_dispatch() {
        let actions = ["AXShowMenu".to_owned(), "AXConfirm".to_owned()];
        for role in ["AXTextField", "AXTextArea"] {
            for action in ["press", "click", "PRESS"] {
                assert_eq!(
                    element_click_route(action, "left", false, role, &actions, false, false),
                    ElementClickRoute::TextInputPointer,
                );
            }
        }
    }

    #[test]
    fn advertised_press_and_other_roles_keep_semantic_route() {
        assert_eq!(
            element_click_route(
                "press",
                "left",
                false,
                "AXTextField",
                &["AXPress".to_owned(), "AXConfirm".to_owned()],
                false,
                false,
            ),
            ElementClickRoute::AxSemantic,
        );
        for role in [
            "AXStaticText",
            "AXButton",
            "AXRow",
            "AXCell",
            "AXWebArea",
            "",
        ] {
            assert_eq!(
                element_click_route("press", "left", false, role, &[], false, false),
                ElementClickRoute::AxSemantic,
            );
        }
    }

    #[test]
    fn explicit_semantics_buttons_modifiers_and_selection_never_choose_text_pointer() {
        for action in [
            "confirm",
            "show_menu",
            "right_click",
            "pick",
            "cancel",
            "open",
            "unknown",
        ] {
            assert_eq!(
                element_click_route(action, "left", false, "AXTextField", &[], false, false),
                ElementClickRoute::AxSemantic,
            );
        }
        for (button, modifiers, selectable, auxiliary) in [
            ("right", false, false, false),
            ("middle", false, false, false),
            ("left", true, false, false),
            ("left", false, true, false),
            ("left", false, false, true),
        ] {
            assert_eq!(
                element_click_route(
                    "press",
                    button,
                    modifiers,
                    "AXTextField",
                    &[],
                    selectable,
                    auxiliary,
                ),
                ElementClickRoute::AxSemantic,
            );
        }
    }

    fn pointer_test_bounds() -> crate::windows::WindowBounds {
        crate::windows::WindowBounds {
            x: -300.0,
            y: 100.0,
            width: 200.0,
            height: 160.0,
        }
    }

    #[test]
    fn text_pointer_center_uses_logical_points_and_supports_negative_screen_origins() {
        let bounds = pointer_test_bounds();
        let target =
            text_input_pointer_target(Some([-280.0, 120.0, 100.0, 20.0]), &bounds, Some(&bounds))
                .expect("live in-window element");
        assert_eq!((target.screen_x, target.screen_y), (-230.0, 130.0));
        assert_eq!((target.window_x, target.window_y), (70.0, 30.0));
    }

    #[test]
    fn text_pointer_refuses_missing_changed_and_invalid_window_frames() {
        let bounds = pointer_test_bounds();
        let element = Some([-280.0, 120.0, 100.0, 20.0]);
        assert!(text_input_pointer_target(None, &bounds, Some(&bounds)).is_none());
        assert!(text_input_pointer_target(element, &bounds, None).is_none());
        for live in [
            crate::windows::WindowBounds {
                x: -299.0,
                ..bounds.clone()
            },
            crate::windows::WindowBounds {
                y: 101.0,
                ..bounds.clone()
            },
            crate::windows::WindowBounds {
                width: 201.0,
                ..bounds.clone()
            },
            crate::windows::WindowBounds {
                height: 161.0,
                ..bounds.clone()
            },
        ] {
            assert!(text_input_pointer_target(element, &bounds, Some(&live)).is_none());
        }
        for invalid in [
            crate::windows::WindowBounds {
                x: f64::NAN,
                ..bounds.clone()
            },
            crate::windows::WindowBounds {
                y: f64::INFINITY,
                ..bounds.clone()
            },
            crate::windows::WindowBounds {
                width: 0.0,
                ..bounds.clone()
            },
            crate::windows::WindowBounds {
                height: -1.0,
                ..bounds.clone()
            },
            crate::windows::WindowBounds {
                width: f64::INFINITY,
                ..bounds.clone()
            },
        ] {
            assert!(text_input_pointer_target(element, &invalid, Some(&invalid)).is_none());
        }
    }

    #[test]
    fn text_pointer_refuses_nonfinite_degenerate_and_outside_element_centers() {
        let bounds = pointer_test_bounds();
        for element in [
            [f64::NAN, 120.0, 100.0, 20.0],
            [-280.0, f64::INFINITY, 100.0, 20.0],
            [-280.0, 120.0, f64::INFINITY, 20.0],
            [-280.0, 120.0, 100.0, 0.0],
            [-280.0, 120.0, -1.0, 20.0],
            [-400.0, 120.0, 10.0, 20.0],
            [-280.0, 50.0, 100.0, 20.0],
            [-110.0, 120.0, 20.0, 20.0], // center on the exclusive right edge
            [-280.0, 250.0, 100.0, 20.0], // center on the exclusive bottom edge
        ] {
            assert!(text_input_pointer_target(Some(element), &bounds, Some(&bounds)).is_none());
        }
    }

    #[test]
    fn element_result_labels_actual_transport_without_inventing_verification() {
        for (fronted, used_pixel, expected_path) in [
            (false, false, "ax"),
            (true, false, "ax_fg"),
            (false, true, "cgevent"),
            (true, true, "cgevent_fg"),
        ] {
            let result = element_click_result(fronted, used_pixel, false, false);
            assert_eq!(result["path"], expected_path);
            assert_eq!(result["verified"], false);
            assert_eq!(result["effect"], "unverifiable");
            assert!(result.get("evidence").is_none());
        }
        let selection = element_click_result(false, true, true, false);
        assert_eq!(selection["verified"], true);
        assert_eq!(selection["effect"], "confirmed");
        assert_eq!(
            element_click_result(false, false, false, true)["effect"],
            "suspected_noop",
        );
    }

    #[test]
    fn text_pointer_preflight_refusals_are_not_reported_as_dispatch_success() {
        for refusal in [
            ElementPointerRefusal::Target("target no longer live"),
            ElementPointerRefusal::Frame(super::super::px_frame::PxFrameError::WindowNotFound {
                window_id: 42,
            }),
        ] {
            let result = refusal.result(7, 42);
            assert_eq!(result.is_error, Some(true));
            let structured = result.structured_content.expect("structured refusal");
            assert_eq!(structured["effect"], "refused");
            assert!(structured.get("path").is_none());
            assert!(structured.get("verified").is_none());
        }
    }

    #[test]
    fn text_input_focus_settle_applies_only_to_text_input_ax_press() {
        for role in ["AXTextField", "AXTextArea"] {
            assert!(needs_text_input_focus_settle(role, "AXPress"));
            for action in ["AXShowMenu", "AXPick", "AXConfirm", "AXCancel", "AXOpen"] {
                assert!(!needs_text_input_focus_settle(role, action));
            }
        }
        for role in ["AXButton", "AXPopUpButton", "AXWebArea", "AXGroup", ""] {
            assert!(!needs_text_input_focus_settle(role, "AXPress"));
        }
    }

    #[test]
    fn text_input_focus_settle_has_small_budget() {
        assert_eq!(
            TEXT_INPUT_FOCUS_SETTLE,
            std::time::Duration::from_millis(100)
        );
    }

    #[test]
    fn text_input_focus_settle_waits_full_budget_without_elapsed_report() {
        assert_eq!(
            remaining_text_input_focus_settle(std::time::Duration::ZERO),
            TEXT_INPUT_FOCUS_SETTLE
        );
    }

    #[test]
    fn text_input_focus_settle_credits_partial_report_time() {
        for (elapsed_ms, remaining_ms) in [(1, 99), (25, 75), (50, 50), (99, 1)] {
            assert_eq!(
                remaining_text_input_focus_settle(std::time::Duration::from_millis(elapsed_ms)),
                std::time::Duration::from_millis(remaining_ms)
            );
        }
    }

    #[test]
    fn text_input_focus_settle_saturates_after_report_covers_budget() {
        for elapsed in [
            TEXT_INPUT_FOCUS_SETTLE,
            std::time::Duration::from_millis(101),
            std::time::Duration::from_secs(1),
            std::time::Duration::MAX,
        ] {
            assert_eq!(
                remaining_text_input_focus_settle(elapsed),
                std::time::Duration::ZERO
            );
        }
    }

    #[test]
    fn text_input_focus_settle_preserves_exact_minimum_interval() {
        for elapsed_ns in [0, 1, 99_999_999, 100_000_000, 100_000_001, 1_000_000_000] {
            let elapsed = std::time::Duration::from_nanos(elapsed_ns);
            let remaining = remaining_text_input_focus_settle(elapsed);
            assert_eq!(elapsed + remaining, elapsed.max(TEXT_INPUT_FOCUS_SETTLE));
        }
    }

    /// Surface 5: schema must advertise the new `button` field with the three
    /// canonical values and default to "left". Hermes / Codex / Claude Code
    /// consumers branch on this enum being present.
    #[test]
    fn schema_advertises_button_enum() {
        let d = def();
        let props = d.input_schema.get("properties").expect("properties");
        let button = props.get("button").expect("button field present");
        let kind = button.get("type").and_then(|v| v.as_str());
        assert_eq!(kind, Some("string"));
        let enum_vals: Vec<&str> = button
            .get("enum")
            .and_then(|v| v.as_array())
            .expect("button.enum present")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(enum_vals.contains(&"left"));
        assert!(enum_vals.contains(&"right"));
        assert!(enum_vals.contains(&"middle"));
    }

    /// Surface 5 hard constraint: the tool description must mention the
    /// `button` argument and the "left" default so MCP introspection (which
    /// pipes description into LLM prompts) carries the back-compat note.
    #[test]
    fn description_mentions_button_default() {
        let d = def();
        let desc = d.description.to_ascii_lowercase();
        assert!(
            desc.contains("button"),
            "description should mention button arg"
        );
        assert!(
            desc.contains("left"),
            "description should mention left default"
        );
        assert!(
            desc.contains("middle"),
            "description should mention middle button"
        );
    }

    /// Existing default behaviour preserved: no `button` field on the call →
    /// resolves to "left" inside invoke. We can't drive the AX path without a
    /// live macOS Window Server, but we CAN check the same arg-parsing logic
    /// the invoke uses produces "left" for empty / absent input.
    #[test]
    fn button_defaults_to_left_when_absent() {
        use cua_driver_core::tool_args::ArgsExt;
        let args = serde_json::json!({ "pid": 1234 });
        let button_str_raw = args.str_or("button", "left").to_lowercase();
        let resolved = if button_str_raw.is_empty() {
            "left".to_string()
        } else {
            button_str_raw
        };
        assert_eq!(resolved, "left");
    }

    /// Round-trip the three canonical values through the same parse the invoke
    /// uses, so any future refactor that changes str_or semantics breaks here
    /// before it breaks consumers.
    #[test]
    fn button_round_trips_right_and_middle() {
        use cua_driver_core::tool_args::ArgsExt;
        for v in ["left", "right", "middle"] {
            let args = serde_json::json!({ "pid": 1234, "button": v });
            let s = args.str_or("button", "left").to_lowercase();
            assert_eq!(s, v);
        }
    }

    /// A raw background left click with an exact window may install target-only
    /// synthetic routing focus. Other background buttons retain strict
    /// suppression, and the explicit foreground rung owns real activation.
    #[test]
    fn raw_background_left_click_uses_target_only_synthetic_focus() {
        assert_eq!(
            pixel_activation_policy("left", false, true),
            PixelActivationPolicy::SyntheticTargetFocus
        );
        assert_eq!(
            pixel_activation_policy("left", false, false),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("right", false, true),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("middle", false, true),
            PixelActivationPolicy::SuppressTarget
        );
        assert_eq!(
            pixel_activation_policy("left", true, true),
            PixelActivationPolicy::ForegroundAssist
        );
    }
    #[test]
    fn background_pixel_ax_bridge_requires_advertised_press() {
        assert!(background_pixel_ax_press_eligible(
            true,
            "AXButton",
            &["AXPress".to_owned(), "AXShowMenu".to_owned()],
            Some(true),
        ));
        assert!(background_pixel_ax_press_eligible(
            true,
            "AXLink",
            &["AXPress".to_owned()],
            Some(true),
        ));
        assert!(!background_pixel_ax_press_eligible(
            true,
            "AXLink",
            &["AXPress".to_owned()],
            None,
        ));
        assert!(!background_pixel_ax_press_eligible(
            false,
            "AXButton",
            &["AXPress".to_owned()],
            Some(true),
        ));
        assert!(!background_pixel_ax_press_eligible(
            true,
            "AXButton",
            &[],
            Some(true),
        ));
        assert!(!background_pixel_ax_press_eligible(
            true,
            "AXButton",
            &["AXShowMenu".to_owned(), "AXScrollToVisible".to_owned()],
            Some(true),
        ));
        assert!(!background_pixel_ax_press_eligible(
            true,
            "AXButton",
            &["AXPress".to_owned()],
            Some(false),
        ));
        assert!(!background_pixel_ax_press_eligible(
            true,
            "AXWebArea",
            &["AXPress".to_owned()],
            Some(true),
        ));
        assert!(!background_pixel_ax_press_eligible(
            true,
            "AXGroup",
            &["AXPress".to_owned()],
            Some(true),
        ));
        assert!(!background_pixel_ax_press_eligible(
            true,
            "AXPopUpButton",
            &["AXPress".to_owned()],
            Some(true),
        ));
    }
}
