//! MCP tool implementations for macOS.

mod bring_to_front;
mod click;
mod clipboard;
mod double_click;
mod drag;
mod get_window_state;
mod hotkey;
mod invoke_menu;
mod kill_app;
mod launch_app;
mod list_apps;
mod list_windows;
mod press_key;
mod right_click;
mod scroll;
mod set_value;
mod set_window_frame;
mod type_text;
// `screenshot` / `screenshot_compat` modules removed in PR #1692 —
// `get_window_state` capture_mode:"vision" is the canonical screenshot
// path. The capture functions they wrapped (ScreenCaptureKit, CGWindow,
// etc.) live elsewhere under CuaDriverCore::Capture and are reached
// through GetWindowStateTool.
mod check_permissions;
mod cursor_tools;
mod get_accessibility_tree;
mod get_config;
mod get_cursor_position;
mod get_desktop_state;
pub(crate) mod get_screen_size;
mod health_report;
mod move_cursor;
mod page;
pub(crate) mod px_frame;
mod set_config;
mod type_text_chars;
mod zoom;

use cua_driver_core::{
    tool::{Tool, ToolRegistry},
    window_target::{PidOnlyWindowTargetGuard, WindowTargetCandidate, WindowTargetCandidates},
};
use std::collections::HashMap;
use std::sync::Arc;

use crate::{ax::cache::ElementCache, cursor::state::CursorRegistry};

fn pid_window_target_candidates(pid: i64) -> Vec<WindowTargetCandidate> {
    let Ok(pid) = i32::try_from(pid) else {
        return Vec::new();
    };
    window_target_candidates_for_pid(crate::windows::all_automation_windows(), pid)
}

fn window_target_candidates_for_pid(
    windows: impl IntoIterator<Item = crate::windows::WindowInfo>,
    pid: i32,
) -> Vec<WindowTargetCandidate> {
    let candidates: Vec<_> = windows
        .into_iter()
        .filter(|window| window.pid == pid)
        .map(|window| WindowTargetCandidate {
            window_id: u64::from(window.window_id),
            title: window.title,
            app_name: Some(window.app_name),
            is_on_screen: window.is_on_screen,
        })
        .collect();

    // CGWindowList includes off-screen AppKit bookkeeping surfaces alongside
    // the application's real window (menu-bar and restoration helpers are
    // common examples). They are not actionable PID-only targets and must not
    // make an otherwise unique visible window look ambiguous. Keep the
    // off-screen set only as a fallback for apps whose windows are all hidden
    // or minimized, where an explicit ambiguity remains safer than guessing.
    let visible: Vec<_> = candidates
        .iter()
        .filter(|candidate| candidate.is_on_screen)
        .cloned()
        .collect();
    if visible.is_empty() {
        candidates
    } else {
        visible
    }
}

#[cfg(test)]
mod pid_window_target_tests {
    use super::*;
    use cua_driver_core::window_target::{resolve_pid_window_target, PidWindowTargetResolution};

    fn window(window_id: u32, pid: i32) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo {
            window_id,
            pid,
            app_name: "Editor".into(),
            title: format!("Document {window_id}"),
            bounds: crate::windows::WindowBounds {
                x: 0.0,
                y: 0.0,
                width: 640.0,
                height: 480.0,
            },
            layer: 0,
            z_index: 1,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    #[test]
    fn same_pid_sibling_windows_are_ambiguous() {
        let candidates =
            window_target_candidates_for_pid([window(7, 42), window(8, 42), window(9, 99)], 42);
        assert!(matches!(
            resolve_pid_window_target(candidates),
            PidWindowTargetResolution::Ambiguous(windows)
                if windows.iter().map(|window| window.window_id).collect::<Vec<_>>() == [7, 8]
        ));
    }

    #[test]
    fn visible_window_wins_over_offscreen_appkit_helpers() {
        let mut offscreen = window(7, 42);
        offscreen.title.clear();
        offscreen.is_on_screen = false;
        offscreen.bounds = crate::windows::WindowBounds {
            x: -192.0,
            y: -1080.0,
            width: 1920.0,
            height: 30.0,
        };
        let visible = window(8, 42);

        let candidates = window_target_candidates_for_pid([offscreen, visible], 42);
        assert!(matches!(
            resolve_pid_window_target(candidates),
            PidWindowTargetResolution::Resolved(window) if window.window_id == 8
        ));
    }

    #[test]
    fn all_offscreen_windows_remain_ambiguous_instead_of_guessing() {
        let mut first = window(7, 42);
        first.is_on_screen = false;
        let mut second = window(8, 42);
        second.is_on_screen = false;

        let candidates = window_target_candidates_for_pid([first, second], 42);
        assert!(matches!(
            resolve_pid_window_target(candidates),
            PidWindowTargetResolution::Ambiguous(windows)
                if windows.iter().map(|window| window.window_id).collect::<Vec<_>>() == [7, 8]
        ));
    }
}

#[cfg(test)]
mod background_input_regression_tests;

fn pid_window_guarded<T: Tool + 'static>(
    tool: T,
    candidates: &WindowTargetCandidates,
) -> Box<dyn Tool> {
    Box::new(PidOnlyWindowTargetGuard::new(
        Box::new(tool),
        candidates.clone(),
    ))
}

pub use check_permissions::{
    request_from_launchservices_host as request_permissions_from_launchservices_host,
    PERMISSIONS_HOST_REQUEST_ARG,
};

/// Per-process zoom context — stores the padded crop origin and resize scale
/// from the most recent `zoom` call, so `click(from_zoom=true)` can translate
/// zoom-image pixel coordinates back to full-window coordinates.
#[derive(Clone, Copy, Debug)]
pub struct ZoomContext {
    /// Padded crop X origin in full-window pixel space.
    pub origin_x: f64,
    /// Padded crop Y origin in full-window pixel space.
    pub origin_y: f64,
    /// Inverse resize scale: `cw / out_w` (1.0 = no downscale).
    pub scale_inv: f64,
}

impl ZoomContext {
    /// Translate a zoom-image coordinate `(px, py)` to full-window pixel coordinates.
    pub fn zoom_to_window(&self, px: f64, py: f64) -> (f64, f64) {
        (
            self.origin_x + px * self.scale_inv,
            self.origin_y + py * self.scale_inv,
        )
    }
}

/// Input delivery modality — the agent-selected rung of the best-effort-background
/// ladder, passed per call (never a stored/config setting).
///
/// - `Background` (default): post synthetic input to the pid without fronting.
/// - `Foreground`: briefly front the target window, act, then restore the prior
///   frontmost (see [`crate::input::skylight::with_foreground_assist`]). The
///   agent's vision-driven last resort — and the only way `click` reaches a
///   foreground rung. Orthogonal to addressing (`element_index` vs `x/y`, which
///   selects AX vs pixel).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DeliveryMode {
    #[default]
    Background,
    Foreground,
}

impl DeliveryMode {
    /// Parse the per-call `delivery_mode` argument. Anything other than an
    /// explicit case-insensitive `"foreground"` resolves to `Background` — the
    /// correct default, so an omitted/garbage value never silently fronts.
    pub fn parse(arg: Option<&str>) -> Self {
        match arg {
            Some(s) if s.eq_ignore_ascii_case("foreground") => Self::Foreground,
            _ => Self::Background,
        }
    }

    pub fn is_foreground(self) -> bool {
        matches!(self, Self::Foreground)
    }
}

/// Convert a pure background-input refusal into the structured refusal result
/// shape shared by exact-target tools: `code`, `effect: "refused"`, the
/// requested target, and the safe next route when one exists. No actuator ran.
pub(crate) fn background_refusal_result(
    pid: i32,
    window_id: u32,
    refusal: &cua_driver_core::background_input::BackgroundRefusal,
) -> cua_driver_core::protocol::ToolResult {
    let mut structured = serde_json::json!({
        "code": refusal.code,
        "effect": "refused",
        "pid": pid,
        "window_id": window_id,
        "reason": refusal.reason,
    });
    if let Some(advice) = refusal.advice {
        structured["escalation"] = serde_json::json!({
            "recommended": advice,
            "reason": refusal.reason,
        });
    }
    cua_driver_core::protocol::ToolResult::error(format!(
        "Background input refused ({}): {}",
        refusal.code, refusal.reason
    ))
    .with_structured(structured)
}

/// Exclusive per-process ownership of one background mutation. Callers must
/// keep this value alive through actuator dispatch, focus restoration, and
/// target-bound verification.
pub(crate) struct BackgroundMutationLease {
    pid: i32,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl BackgroundMutationLease {
    /// Revalidate another actuator class while retaining the same per-PID
    /// lease. This supports explicit ladders without deadlocking by attempting
    /// to reacquire the coordinator recursively.
    pub(crate) async fn gate_again(
        &self,
        window_id: u32,
        element_ptr: Option<usize>,
        action: cua_driver_core::background_input::BackgroundAction,
    ) -> Result<(), cua_driver_core::protocol::ToolResult> {
        decide_background_window_action(self.pid, window_id, element_ptr, action).await
    }
}

async fn decide_background_window_action(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
    action: cua_driver_core::background_input::BackgroundAction,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    use cua_driver_core::background_input::{
        decide_background_input, BackgroundInputDecision, ExactWindowTarget,
    };
    let facts = match tokio::task::spawn_blocking(move || {
        crate::ax::exact_target::gather_background_facts(pid, window_id, element_ptr)
    })
    .await
    {
        Ok(facts) => facts,
        Err(error) => {
            return Err(cua_driver_core::protocol::ToolResult::error(format!(
                "Could not gather exact-target facts for pid {pid} window {window_id}: {error}"
            )));
        }
    };
    match decide_background_input(ExactWindowTarget { pid, window_id }, &facts, action) {
        BackgroundInputDecision::Execute { .. } => Ok(()),
        BackgroundInputDecision::Refuse(refusal) => {
            Err(background_refusal_result(pid, window_id, &refusal))
        }
    }
}

/// Acquire the per-PID mutation coordinator, gather fresh exact-target facts,
/// and ask the pure core for one background decision. `element_ptr` must stay
/// retained by the caller until the returned lease is dropped.
pub(crate) async fn gate_background_window_action(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
    action: cua_driver_core::background_input::BackgroundAction,
) -> Result<BackgroundMutationLease, cua_driver_core::protocol::ToolResult> {
    let lease = acquire_background_mutation(pid).await;
    lease.gate_again(window_id, element_ptr, action).await?;
    Ok(lease)
}

pub(crate) async fn acquire_background_mutation(pid: i32) -> BackgroundMutationLease {
    BackgroundMutationLease {
        pid,
        _guard: crate::background_mutation::acquire(pid).await,
    }
}

/// Finish the post-action observation window. Embedded interactive clients
/// that already observe the target continuously may opt out through the
/// private registry argument to avoid adding a one-second acknowledgement
/// delay to every input event. Regular MCP callers retain the full observer.
pub(crate) async fn finish_window_observation(
    snapshot: crate::window_change_detector::Snapshot,
    args: &serde_json::Value,
) -> crate::window_change_detector::Changes {
    if args
        .get("_skip_window_change_detection")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        drop(snapshot);
        crate::window_change_detector::Changes::no_change()
    } else {
        snapshot.detect_async().await
    }
}

#[cfg(test)]
mod interactive_observation_tests {
    use super::*;

    #[tokio::test]
    async fn embedded_interactive_input_can_finish_without_polling() {
        let snapshot = crate::window_change_detector::WindowChangeDetector::snapshot(None);
        let changes = finish_window_observation(
            snapshot,
            &serde_json::json!({"_skip_window_change_detection": true}),
        )
        .await;
        assert!(!changes.needs_restore());
    }
}

/// px-focus for the keyboard family (type_text / press_key / hotkey): focus the
/// element at (x,y) before a keystroke — the *element px action* form of a
/// keyboard tool. Prefer non-destructive AX focus so an existing selection is
/// retained; the foreground rung falls back to a real pixel click when needed.
/// Reuses ClickTool's exact coordinate translation and delivery mode.
/// `Ok(())` on success; `Err(ToolResult)` short-circuits the caller.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn focus_by_pixel(
    state: &Arc<ToolState>,
    pid: i32,
    window_id: Option<u32>,
    x: f64,
    y: f64,
    foreground: bool,
    session: Option<String>,
    session_id: Option<String>,
    from_zoom: bool,
    mutation_lease: Option<&BackgroundMutationLease>,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    use cua_driver_core::tool::Tool;
    let mut click_args = serde_json::json!({
        "pid": pid, "x": x, "y": y,
        "delivery_mode": "background",
        "action": "focus",
    });
    if let Some(wid) = window_id {
        click_args["window_id"] = serde_json::json!(wid);
        if let Some(lease) = mutation_lease {
            lease
                .gate_again(
                    wid,
                    None,
                    cua_driver_core::background_input::BackgroundAction::WindowPointer,
                )
                .await?;
        }
    }
    if let Some(ref s) = session {
        click_args["session"] = serde_json::json!(s);
    }
    if let Some(ref s) = session_id {
        click_args["_session_id"] = serde_json::json!(s);
    }
    if from_zoom {
        click_args["from_zoom"] = serde_json::json!(true);
    }
    let click_tool = click::ClickTool::new(state.clone());
    let click = click_tool.invoke(click_args);
    let focus = if let Some(lease) = mutation_lease {
        crate::background_mutation::with_held_lease(lease.pid, click).await
    } else {
        click.await
    };
    if focus.is_error != Some(true) {
        // AXFocused is non-destructive: unlike a second real click, it keeps a
        // Cmd+A selection intact before a follow-up type_text or Cmd+V.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        // A background focus-click is `effect:"unverifiable"` by construction:
        // it cannot prove the renderer moved its first responder. Returning on
        // "it didn't error" therefore skipped the real-click fallback below
        // whenever the click was a silent no-op — advancing on transport
        // success alone, which is exactly what the ladder forbids. Confirm the
        // focus actually moved before claiming this rung worked.
        if !foreground || pixel_focus_landed(pid, window_id, x, y).await {
            return Ok(());
        }
    } else if !foreground {
        return Err(cua_driver_core::protocol::ToolResult::error(format!(
            "focus pixel-click at ({x:.0},{y:.0}) failed."
        )));
    }

    // Some renderer surfaces do not expose a usable AX focus action. The
    // explicit foreground rung retains its real-click fallback for them.
    let mut click_args = serde_json::json!({
        "pid": pid, "x": x, "y": y,
        "delivery_mode": "foreground",
        "action": "press",
    });
    if let Some(wid) = window_id {
        click_args["window_id"] = serde_json::json!(wid);
    }
    if let Some(ref s) = session {
        click_args["session"] = serde_json::json!(s);
    }
    if let Some(ref s) = session_id {
        click_args["_session_id"] = serde_json::json!(s);
    }
    if from_zoom {
        click_args["from_zoom"] = serde_json::json!(true);
    }
    let focus = click::ClickTool::new(state.clone())
        .invoke(click_args)
        .await;
    if focus.is_error == Some(true) {
        return Err(cua_driver_core::protocol::ToolResult::error(format!(
            "focus pixel-click at ({x:.0},{y:.0}) failed."
        )));
    }
    // Brief settle so the renderer registers focus before the keystrokes.
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    Ok(())
}

/// Confirm that a focus pixel-click actually moved the application's focused
/// element onto the clicked point.
///
/// The background focus-click reports `effect:"unverifiable"`, so this is the
/// read-back that lets [`focus_by_pixel`] decide whether the cheap rung worked
/// or the real-click fallback is still required. It reuses the same
/// window-local-pixels → screen translation the click itself used, so the
/// comparison is in one coordinate space.
///
/// Returns `false` whenever the answer cannot be established (no window id,
/// untranslatable frame, unreadable focused element or rect). That is the
/// conservative direction: an unprovable focus escalates to the stronger rung
/// rather than being reported as success.
async fn pixel_focus_landed(pid: i32, window_id: Option<u32>, x: f64, y: f64) -> bool {
    let Some(wid) = window_id else {
        return false;
    };
    tokio::task::spawn_blocking(move || {
        let Ok(frame) = px_frame::resolve_window_px_frame(wid) else {
            return false;
        };
        let (screen_x, screen_y, _, _) = frame.to_screen(x, y);
        unsafe {
            let Some(focused) = crate::ax::bindings::focused_element_of_pid(pid) else {
                return false;
            };
            let rect = crate::ax::bindings::element_screen_rect(focused);
            core_foundation::base::CFRelease(focused as core_foundation::base::CFTypeRef);
            let Some(rect) = rect else {
                return false;
            };
            point_within_rect(rect, screen_x, screen_y)
        }
    })
    .await
    .unwrap_or(false)
}

/// Whether `[x, y, width, height]` (screen coordinates, top-left origin)
/// contains the point. A degenerate rect never contains anything, so an app
/// reporting a zero-sized focused element escalates rather than false-confirms.
fn point_within_rect([rx, ry, rw, rh]: [f64; 4], x: f64, y: f64) -> bool {
    rw > 0.0 && rh > 0.0 && x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

/// Thread-safe per-pid zoom context registry.
pub struct ZoomRegistry {
    inner: std::sync::Mutex<HashMap<i32, ZoomContext>>,
}

impl Default for ZoomRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ZoomRegistry {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn set(&self, pid: i32, ctx: ZoomContext) {
        self.inner.lock().unwrap().insert(pid, ctx);
    }

    pub fn get(&self, pid: i32) -> Option<ZoomContext> {
        self.inner.lock().unwrap().get(&pid).copied()
    }
}

/// Tracks the per-(pid, window_id) ratio applied by `max_image_dimension`
/// downscaling.
///
/// `ratio = original_dim / resized_dim` — multiply resized image coordinates
/// by this to recover original (native) window-local pixel coordinates.
/// Mirrors Swift's `ImageResizeRegistry`.
///
/// Keyed per window, matching the element cache and the element-token
/// registry. A pid-only key leaked the ratio recorded while snapshotting
/// window A into pixel clicks aimed at window B of the same pid, sending them
/// off-target (issue #2237).
pub struct ResizeRegistry {
    inner: std::sync::Mutex<HashMap<(i32, u32), f64>>,
}

impl Default for ResizeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ResizeRegistry {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Record that (pid, window_id)'s screenshot was downscaled by `ratio`.
    pub fn set_ratio(&self, pid: i32, window_id: u32, ratio: f64) {
        self.inner.lock().unwrap().insert((pid, window_id), ratio);
    }

    /// Remove the ratio entry for one window (no active downscale).
    pub fn clear_ratio(&self, pid: i32, window_id: u32) {
        self.inner.lock().unwrap().remove(&(pid, window_id));
    }

    /// The ratio for a window, or `None` if no downscale happened.
    ///
    /// `window_id: None` is the screen-scope (legacy) path: it returns a ratio
    /// only when every window recorded for `pid` agrees on one, so a
    /// window-less caller can never inherit some other window's scale. That
    /// preserves today's behaviour for the single-window case without guessing
    /// across windows.
    pub fn ratio(&self, pid: i32, window_id: Option<u32>) -> Option<f64> {
        let inner = self.inner.lock().unwrap();
        match window_id {
            Some(wid) => inner.get(&(pid, wid)).copied(),
            None => {
                let mut agreed: Option<f64> = None;
                for (_, ratio) in inner.iter().filter(|((p, _), _)| *p == pid) {
                    match agreed {
                        None => agreed = Some(*ratio),
                        Some(seen) if (seen - *ratio).abs() < 1e-9 => {}
                        Some(_) => return None,
                    }
                }
                agreed
            }
        }
    }
}

/// Runtime-mutable driver configuration persisted across calls within a session.
pub struct DriverConfig {
    /// Max screenshot dimension (0 = no limit). Applied during screenshot/zoom.
    /// Default 1568 matches Swift's `CuaDriverConfig.defaultMaxImageDimension` —
    /// the long edge is downscaled to this before encoding.
    pub max_image_dimension: u32,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            max_image_dimension: 1568,
        }
    }
}

/// Path to the persistent JSON config file shared by the CLI and MCP session.
pub fn config_file_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(format!("{home}/.cua-driver/config.json"))
}

/// Load `DriverConfig` from `~/.cua-driver/config.json`, falling back to
/// defaults for any missing or unrecognised keys.  Called at MCP startup so
/// that `cua-driver config set capture_mode vision` (CLI) carries over into
/// the next MCP session without requiring a per-call `set_config`.
pub fn load_driver_config() -> DriverConfig {
    let mut cfg = DriverConfig::default();
    let path = config_file_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return cfg, // no file yet — use defaults
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return cfg, // malformed file — use defaults
    };
    // `capture_mode` is per-call now; old on-disk `capture_mode` and
    // `capture_scope` keys are intentionally inert.
    if let Some(v) = json.get("max_image_dimension").and_then(|v| v.as_u64()) {
        if let Ok(v32) = u32::try_from(v) {
            cfg.max_image_dimension = v32;
        }
    }
    cfg
}

/// Convert native pixels from `get_desktop_state` into the logical point space
/// used by CoreGraphics input APIs. Retina scaled modes cannot rely on the
/// nominal backing factor alone, so derive the ratio from the actual PNG.
pub async fn desktop_screenshot_point(x: f64, y: f64) -> (f64, f64) {
    let ratio = tokio::task::spawn_blocking(|| {
        let logical_w = get_screen_size::main_screen_size().map(|(w, _, _)| w as f64);
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
    (x / ratio, y / ratio)
}

/// Persist a single key/value pair to `~/.cua-driver/config.json`.
/// Merges with any existing file contents so other keys are preserved.
/// Returns `Err` if the directory cannot be created or the file cannot be written.
pub fn write_driver_config_key(key: &str, value: &serde_json::Value) -> Result<(), String> {
    let path = config_file_path();
    let mut json: serde_json::Value = path
        .exists()
        .then(|| std::fs::read_to_string(&path).ok())
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    json[key] = value.clone();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(())
}

/// Per-session config overrides layered over the global persisted `DriverConfig`.
///
/// The cua-driver daemon is one shared process: every `cua-driver mcp` proxy
/// connects to it and shares its `ToolState`. `DriverConfig` is therefore
/// multi-tenant AND persisted to disk — so without session scoping, session A's
/// `set_config capture_mode=vision` clobbers session B's value and flips the
/// on-disk default under everyone. These overrides fix that: a named MCP session
/// gets an in-memory, non-persisted override keyed by its `_session_id`; the
/// anonymous session (CLI / one-shot `call`) still writes the shared global +
/// disk. `None` fields mean "fall through to the global layer".
#[derive(Clone, Default)]
pub struct ConfigOverrides {
    pub max_image_dimension: Option<u32>,
}

/// Thread-safe map of `session_id` → `ConfigOverrides`, mirroring
/// `CursorRegistry`'s registry shape. Cleared per session on `session_end`.
pub struct SessionConfigRegistry {
    inner: std::sync::Mutex<HashMap<String, ConfigOverrides>>,
}

impl SessionConfigRegistry {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Merge `delta` into `session`'s overrides (only the `Some` fields of
    /// `delta` overwrite; existing overrides for unset fields are preserved).
    pub fn set(&self, session: &str, delta: ConfigOverrides) {
        // Write-boundary resurrection guard: keyed by session_id, so an
        // in-flight set_config that lands AFTER session_end (passed the dispatch
        // gate, then the proxy died and the reaper cleared this session's
        // overrides) must NOT re-create the entry — it would be invisible and
        // never reaped again. `fire_session_end` marks ENDED_SESSIONS *before*
        // running the config-clear hook, so this check is authoritative.
        if cua_driver_core::session::is_session_ended(session) {
            return;
        }
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(session.to_owned()).or_default();
        if delta.max_image_dimension.is_some() {
            entry.max_image_dimension = delta.max_image_dimension;
        }
    }

    /// Resolve the effective `max_image_dimension` for `session`, layering its
    /// override over the global `DriverConfig`. `session = None` (anonymous)
    /// returns the global value verbatim.
    pub fn effective_max_image_dimension(
        &self,
        session: Option<&str>,
        global: &DriverConfig,
    ) -> u32 {
        let ov = session.and_then(|s| self.inner.lock().unwrap().get(s).cloned());
        match ov {
            Some(ov) => ov.max_image_dimension.unwrap_or(global.max_image_dimension),
            None => global.max_image_dimension,
        }
    }

    /// Drop `session`'s overrides. No-op for an unknown id (so `session_end`
    /// for an anonymous / never-set session is harmless).
    pub fn clear(&self, session: &str) {
        self.inner.lock().unwrap().remove(session);
    }
}

impl Default for SessionConfigRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared state passed to all tools.
pub struct ToolState {
    pub element_cache: Arc<ElementCache>,
    pub cursor_registry: Arc<CursorRegistry>,
    pub zoom_registry: Arc<ZoomRegistry>,
    pub resize_registry: Arc<ResizeRegistry>,
    /// A full observation may discover that the host application's current
    /// modal UI lives in an out-of-process AppKit view service.  Keep that
    /// exact, revalidated alias so TBH-style wrappers can continue addressing
    /// the stable host pid/window while explicit foreground keyboard actions
    /// reach the transient that was actually shown to the model.
    pub(crate) transient_ui_registry: Arc<crate::transient_ui::TransientUiRegistry>,
    /// Global, disk-persisted config — the base layer and the only one the
    /// anonymous session / CLI writes.
    pub config: Arc<std::sync::RwLock<DriverConfig>>,
    /// Per-MCP-session in-memory config overrides layered over `config`.
    pub session_config: Arc<SessionConfigRegistry>,
    /// Open CDP connections, one per port, reused across `insert_text` /
    /// `type_keystrokes` calls instead of reconnecting fresh every time —
    /// see `CdpSessionCache` for why (Chrome's "allow remote debugging"
    /// popup fires on every new connection, not once per session).
    pub cdp_sessions: Arc<crate::browser::CdpSessionCache>,
    /// Whether the runtime owner installed the AppKit main-thread cursor
    /// overlay facility. Imported SDK runtimes deliberately leave this false;
    /// explicit cursor-overlay methods must refuse instead of reporting a
    /// successful no-op.
    pub cursor_overlay_available: bool,
    /// Direct and embedded hosts own TCC request UX. Their runtime may inspect
    /// permission state but must not raise Cua-owned prompts.
    pub host_owns_permission_ux: bool,
    /// Advisory host identity for permission diagnostics only.
    pub host_bundle_id: Option<String>,
}

impl Default for ToolState {
    fn default() -> Self {
        Self::new(false, false, None)
    }
}

impl ToolState {
    fn new(
        cursor_overlay_available: bool,
        host_owns_permission_ux: bool,
        host_bundle_id: Option<String>,
    ) -> Self {
        Self {
            element_cache: Arc::new(ElementCache::new()),
            cursor_registry: Arc::new(CursorRegistry::new()),
            zoom_registry: Arc::new(ZoomRegistry::new()),
            resize_registry: Arc::new(ResizeRegistry::new()),
            transient_ui_registry: Arc::new(crate::transient_ui::TransientUiRegistry::new()),
            // Load persisted config from ~/.cua-driver/config.json so that
            // `cua-driver config set` changes carry over into MCP sessions.
            config: Arc::new(std::sync::RwLock::new(load_driver_config())),
            session_config: Arc::new(SessionConfigRegistry::new()),
            cdp_sessions: Arc::new(crate::browser::CdpSessionCache::new()),
            cursor_overlay_available,
            host_owns_permission_ux,
            host_bundle_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ForegroundKeyboardTarget {
    pub(crate) pid: i32,
    pub(crate) window_id: Option<u32>,
    pub(crate) transient_route: Option<crate::transient_ui::TransientRoute>,
}

/// Resolve the exact keyboard target for an explicit foreground action.
///
/// Background calls never enter this function, preserving the normal
/// background-first contract.  A stale helper alias is a hard stop rather than
/// a fallback to the host window, because that fallback could type into a
/// previously focused host control (for example Shortcuts' Search Actions).
pub(crate) async fn resolve_foreground_keyboard_target(
    state: &ToolState,
    session: &crate::transient_ui::TransientSessionKey,
    pid: i32,
    window_id: Option<u32>,
    has_explicit_element_or_point: bool,
) -> Result<ForegroundKeyboardTarget, cua_driver_core::protocol::ToolResult> {
    if tokio::task::spawn_blocking(move || {
        crate::transient_ui::is_trusted_transient_helper_process(pid)
    })
    .await
    .unwrap_or(true)
    {
        return Err(transient_ui_direct_target_refusal(pid, window_id));
    }
    let Some(source_window_id) = window_id else {
        let detection = tokio::task::spawn_blocking(move || {
            crate::transient_ui::detect_any_visible_transient_helper_for_host(pid)
        })
        .await;
        return match detection {
            Ok(detection) if detection.helper_is_visible() => {
                Err(transient_ui_unobserved_refusal(pid, 0, detection))
            }
            Ok(_) => Ok(ForegroundKeyboardTarget {
                pid,
                window_id,
                transient_route: None,
            }),
            Err(error) => Err(cua_driver_core::protocol::ToolResult::error(format!(
                "Could not check for transient UI before foreground keyboard delivery: {error}"
            ))),
        };
    };
    let source = crate::transient_ui::WindowTarget {
        pid,
        window_id: source_window_id,
    };
    let registry = state.transient_ui_registry.clone();
    let session_for_lookup = session.clone();
    let resolution =
        tokio::task::spawn_blocking(move || registry.resolve_live(&session_for_lookup, source))
            .await;
    match resolution {
        Ok(crate::transient_ui::RouteResolution::None) => {
            let detection = tokio::task::spawn_blocking(move || {
                crate::transient_ui::detect_visible_transient_helper(source)
            })
            .await;
            match detection {
                Ok(detection) => foreground_keyboard_target_from_evidence(
                    pid,
                    source_window_id,
                    crate::transient_ui::RouteResolution::None,
                    Some(detection),
                    has_explicit_element_or_point,
                    cua_driver_core::tool::current_dispatch_allows_trusted_transient_target_rewrite(
                    ),
                ),
                Err(error) => Err(cua_driver_core::protocol::ToolResult::error(format!(
                    "Could not check for transient UI before foreground keyboard delivery: {error}"
                ))),
            }
        }
        Ok(resolution) => foreground_keyboard_target_from_evidence(
            pid,
            source_window_id,
            resolution,
            None,
            has_explicit_element_or_point,
            cua_driver_core::tool::current_dispatch_allows_trusted_transient_target_rewrite(),
        ),
        Err(error) => Err(cua_driver_core::protocol::ToolResult::error(format!(
            "Could not revalidate transient UI routing: {error}"
        ))),
    }
}

fn foreground_keyboard_target_from_evidence(
    pid: i32,
    window_id: u32,
    resolution: crate::transient_ui::RouteResolution,
    fresh_detection: Option<crate::transient_ui::TransientHelperDetection>,
    has_explicit_element_or_point: bool,
    target_rewrite_allowed: bool,
) -> Result<ForegroundKeyboardTarget, cua_driver_core::protocol::ToolResult> {
    match resolution {
        crate::transient_ui::RouteResolution::None => {
            let detection = fresh_detection
                .unwrap_or(crate::transient_ui::TransientHelperDetection::None);
            if detection.helper_is_visible() {
                return Err(transient_ui_unobserved_refusal(pid, window_id, detection));
            }
            Ok(ForegroundKeyboardTarget {
                pid,
                window_id: Some(window_id),
                transient_route: None,
            })
        }
        crate::transient_ui::RouteResolution::Live(route) => {
            if !target_rewrite_allowed {
                return Err(transient_ui_policy_refusal(
                    route.source.pid,
                    route.source.window_id,
                ));
            }
            if has_explicit_element_or_point {
                return Err(
                    cua_driver_core::protocol::ToolResult::error(
                        "The host currently presents an observed transient helper, so an element- or point-addressed foreground keyboard action cannot be safely redirected. Re-observe and use an unaddressed foreground type_text/press_key call; no input was sent.",
                    )
                    .with_structured(serde_json::json!({
                        "code": "transient_ui_explicit_target_unsupported",
                        "effect": "refused",
                        "pid": route.source.pid,
                        "window_id": route.source.window_id,
                        "observed_transient_pid": route.target.pid,
                        "observed_transient_window_id": route.target.window_id,
                        "retryable": true,
                    })),
                );
            }
            Ok(ForegroundKeyboardTarget {
                pid: route.target.pid,
                window_id: Some(route.target.window_id),
                transient_route: Some(route),
            })
        }
        crate::transient_ui::RouteResolution::Stale(route) => Err(
            cua_driver_core::protocol::ToolResult::error(
                "The observed transient UI closed or changed before foreground keyboard delivery. Re-observe the host app before retrying; no input was sent.",
            )
            .with_structured(serde_json::json!({
                "code": "transient_ui_stale",
                "effect": "refused",
                "pid": route.source.pid,
                "window_id": route.source.window_id,
                "observed_transient_pid": route.target.pid,
                "observed_transient_window_id": route.target.window_id,
                "retryable": true,
                "suggestion": "Call get_window_state for the host app again before retrying."
            })),
        ),
    }
}

pub(crate) fn transient_ui_policy_refusal(
    pid: i32,
    window_id: u32,
) -> cua_driver_core::protocol::ToolResult {
    cua_driver_core::protocol::ToolResult::error(
        "The requested host is authorized, but its transient helper is a different protected target. This authorization mode cannot safely inherit the host grant; no helper content was observed and no input was sent.",
    )
    .with_structured(serde_json::json!({
        "code": "transient_ui_target_reauthorization_required",
        "effect": "refused",
        "pid": pid,
        "window_id": window_id,
        "retryable": false,
        "reason": "bounded mode and capability manifests require exact protected-resource authorization before target substitution"
    }))
}

pub(crate) fn transient_ui_direct_target_refusal(
    pid: i32,
    window_id: Option<u32>,
) -> cua_driver_core::protocol::ToolResult {
    cua_driver_core::protocol::ToolResult::error(
        "A transient helper is not a public application target. Address the original host app/window and re-observe it before input; no action was sent.",
    )
    .with_structured(serde_json::json!({
        "code": "transient_ui_direct_target_unsupported",
        "effect": "refused",
        "pid": pid,
        "window_id": window_id,
        "retryable": true,
        "suggestion": "Use the original host pid/window_id from get_window_state, never transient_ui.visual_target directly."
    }))
}

pub(crate) fn same_pid_transient_refusal(
    proof: crate::transient_ui::SamePidTransientProof,
) -> cua_driver_core::protocol::ToolResult {
    let dialog_metadata = matches!(
        proof.classification,
        crate::transient_ui::SamePidTransientClassification::DialogMetadata
    );
    cua_driver_core::protocol::ToolResult::error(
        "A unique same-process transient window is focused above the requested window. Re-observe that exact transient before acting; no input was sent.",
    )
    .with_structured(serde_json::json!({
        "code": "same_pid_transient_in_front",
        "effect": "refused",
        "pid": proof.source.pid,
        "window_id": proof.source.window_id,
        "retryable": true,
        "redirect": {
            "kind": "same_pid_modal",
            "pid": proof.target.pid,
            "window_id": proof.target.window_id,
            "proof": {
                "same_pid": true,
                "unique": true,
                "layer_zero": true,
                "on_current_space": true,
                "above_source": true,
                "contained_by_source": true,
                "ax_window_live": true,
                "focused": true,
                "main": true,
                "dialog_metadata": dialog_metadata,
                "classification": proof.classification.as_str()
            }
        },
        "suggestion": "Call get_window_state with the redirect pid/window_id, then use only that fresh observation for input."
    }))
}

pub(crate) async fn guard_same_pid_transient_target(
    pid: i32,
    window_id: Option<u32>,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    let Some(window_id) = window_id else {
        return Ok(());
    };
    let source = crate::transient_ui::WindowTarget { pid, window_id };
    let detection = tokio::task::spawn_blocking(move || {
        crate::transient_ui::detect_same_pid_transient_in_front(source)
    })
    .await
    .map_err(|error| {
        cua_driver_core::protocol::ToolResult::error(format!(
            "Could not check for a same-process transient before input: {error}"
        ))
        .with_structured(serde_json::json!({
            "code": "same_pid_transient_resolution_failed",
            "effect": "refused",
            "pid": pid,
            "window_id": window_id,
            "retryable": true
        }))
    })?;
    match detection {
        crate::transient_ui::SamePidTransientDetection::None => Ok(()),
        crate::transient_ui::SamePidTransientDetection::Unique(proof) => {
            Err(same_pid_transient_refusal(proof))
        }
        crate::transient_ui::SamePidTransientDetection::Ambiguous => Err(
            cua_driver_core::protocol::ToolResult::error(
                "One or more same-process windows cover the requested target, but no unique trusted modal successor could be proven. No input was sent.",
            )
            .with_structured(serde_json::json!({
                "code": "same_pid_transient_ambiguous",
                "effect": "refused",
                "pid": pid,
                "window_id": window_id,
                "retryable": true,
                "suggestion": "Close extra transient windows and observe the app again."
            })),
        ),
        crate::transient_ui::SamePidTransientDetection::Indeterminate => Err(
            cua_driver_core::protocol::ToolResult::error(
                "A same-process window may cover the requested target, but its accessibility relationship could not be proven. No input was sent.",
            )
            .with_structured(serde_json::json!({
                "code": "same_pid_transient_resolution_failed",
                "effect": "refused",
                "pid": pid,
                "window_id": window_id,
                "retryable": true,
                "suggestion": "Re-observe the app after the transient settles or close the extra window."
            })),
        ),
    }
}

pub(crate) async fn guard_transient_pointer_target(
    state: &ToolState,
    session: &crate::transient_ui::TransientSessionKey,
    pid: i32,
    window_id: Option<u32>,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    if tokio::task::spawn_blocking(move || {
        crate::transient_ui::is_trusted_transient_helper_process(pid)
    })
    .await
    .unwrap_or(true)
    {
        return Err(transient_ui_pointer_refusal(pid, window_id, false));
    }
    let Some(window_id) = window_id else {
        let detection = tokio::task::spawn_blocking(move || {
            crate::transient_ui::detect_any_visible_transient_helper_for_host(pid)
        })
        .await
        .map_err(|error| {
            cua_driver_core::protocol::ToolResult::error(format!(
                "Could not check for transient UI before pointer delivery: {error}"
            ))
        })?;
        return if detection.helper_is_visible() {
            Err(transient_ui_pointer_refusal(pid, None, false))
        } else {
            Ok(())
        };
    };

    let source = crate::transient_ui::WindowTarget { pid, window_id };
    let registry = state.transient_ui_registry.clone();
    let session = session.clone();
    let resolution = tokio::task::spawn_blocking(move || registry.resolve_live(&session, source))
        .await
        .map_err(|error| {
            cua_driver_core::protocol::ToolResult::error(format!(
                "Could not revalidate transient UI before pointer delivery: {error}"
            ))
        })?;
    match resolution {
        crate::transient_ui::RouteResolution::None => {
            let detection = tokio::task::spawn_blocking(move || {
                crate::transient_ui::detect_visible_transient_helper(source)
            })
            .await
            .map_err(|error| {
                cua_driver_core::protocol::ToolResult::error(format!(
                    "Could not check for transient UI before pointer delivery: {error}"
                ))
            })?;
            transient_pointer_target_from_evidence(
                pid,
                Some(window_id),
                resolution,
                Some(detection),
            )
        }
        resolution => {
            transient_pointer_target_from_evidence(pid, Some(window_id), resolution, None)
        }
    }
}

fn transient_pointer_target_from_evidence(
    pid: i32,
    window_id: Option<u32>,
    resolution: crate::transient_ui::RouteResolution,
    fresh_detection: Option<crate::transient_ui::TransientHelperDetection>,
) -> Result<(), cua_driver_core::protocol::ToolResult> {
    match resolution {
        crate::transient_ui::RouteResolution::Live(_)
        | crate::transient_ui::RouteResolution::Stale(_) => {
            Err(transient_ui_pointer_refusal(pid, window_id, true))
        }
        crate::transient_ui::RouteResolution::None => {
            if fresh_detection.is_some_and(|detection| detection.helper_is_visible()) {
                Err(transient_ui_pointer_refusal(pid, window_id, false))
            } else {
                Ok(())
            }
        }
    }
}

fn transient_ui_pointer_refusal(
    pid: i32,
    window_id: Option<u32>,
    previously_observed: bool,
) -> cua_driver_core::protocol::ToolResult {
    cua_driver_core::protocol::ToolResult::error(
        "The requested host currently presents a transient helper whose screenshot coordinates and AX elements are not valid host pointer targets. No pointer input was sent.",
    )
    .with_structured(serde_json::json!({
        "code": "transient_ui_pointer_unsupported",
        "effect": "refused",
        "pid": pid,
        "window_id": window_id,
        "previously_observed": previously_observed,
        "retryable": true,
        "suggestion": "Use an unaddressed foreground keyboard action against the host, or close the transient helper and re-observe before using pointer input."
    }))
}

fn transient_ui_unobserved_refusal(
    pid: i32,
    window_id: u32,
    detection: crate::transient_ui::TransientHelperDetection,
) -> cua_driver_core::protocol::ToolResult {
    let ambiguous = matches!(
        detection,
        crate::transient_ui::TransientHelperDetection::Ambiguous
    );
    cua_driver_core::protocol::ToolResult::error(
        "A trusted transient helper is visible, but this session has not successfully observed it. Re-observe the host app before retrying; no input was sent.",
    )
    .with_structured(serde_json::json!({
        "code": "transient_ui_unobserved",
        "effect": "refused",
        "pid": pid,
        "window_id": window_id,
        "ambiguous": ambiguous,
        "retryable": true,
        "suggestion": "Call get_window_state for this host app/window in the same session before retrying."
    }))
}

/// Read the logical pointer remembered for the caller's own session without
/// creating cursor state. Foreground keyboard tools validate this point again
/// against the exact window's live frame immediately after activation.
pub(crate) fn remembered_agent_cursor_position(
    state: &ToolState,
    args: &serde_json::Value,
) -> Option<(f64, f64)> {
    let cursor_key = cursor_tools::resolve_cursor_key(args);
    state
        .cursor_registry
        .get(&cursor_key)
        .and_then(|cursor| cursor.position)
        .map(|position| (position.x, position.y))
}

pub(crate) fn cursor_overlay_unavailable() -> cua_driver_core::protocol::ToolResult {
    let message = "macOS agent cursor overlay is unavailable: this runtime owner has no certified \
                   AppKit main-thread host adapter or no Window Server graphic-session access; \
                   use a GUI private worker or standalone service for cursor-overlay controls";
    cua_driver_core::protocol::ToolResult::error(message).with_structured(serde_json::json!({
        "status": "refused",
        "refusal": {
            "code": "facility_unavailable",
            "facility": "macos_cursor_overlay",
            "message": message,
        }
    }))
}

/// Register all macOS tools into the registry. `compat=true` swaps the
/// regular `screenshot` tool for the Claude Code computer-use compat
/// variant — same name, stricter args, window-scoped JPEG @ 85% + a text
/// note telling the caller to use pixel-addressed tools.
pub fn register_all(
    registry: &mut ToolRegistry,
    compat: bool,
    cursor_overlay_available: bool,
    host_owns_permission_ux: bool,
    host_bundle_id: Option<String>,
) {
    let state = Arc::new(ToolState::new(
        cursor_overlay_available,
        host_owns_permission_ux,
        host_bundle_id,
    ));
    let cursor_outcome_reader = {
        let cursor_registry = state.cursor_registry.clone();
        cua_driver_core::session::register_scoped_cursor_outcome_reader(std::sync::Arc::new(
            move |session_id| {
                let state = cursor_registry.get(session_id);
                let motion_customized = state.is_some()
                    && crate::cursor::overlay::current_motion(session_id)
                        != cursor_overlay::MotionConfig::default();
                let active_cursor_count = cursor_registry
                    .all_states()
                    .iter()
                    .filter(|state| state.config.cursor_id != "default")
                    .count()
                    .max(1);
                match state {
                    Some(state) => cua_driver_core::session::bounded_cursor_outcome(
                        true,
                        state.config.enabled,
                        crate::cursor::overlay::is_visible_for_session(session_id),
                        Some(state.config.theme_id.as_str()),
                        motion_customized,
                        active_cursor_count,
                    ),
                    None => cua_driver_core::session::bounded_cursor_outcome(
                        false,
                        false,
                        false,
                        None,
                        false,
                        active_cursor_count,
                    ),
                }
            },
        ))
    };
    registry.retain_cursor_outcome_reader(cursor_outcome_reader);
    if let Some(runtime_scope) = cua_driver_core::tool::current_dispatch_runtime_scope() {
        let prefix = format!("__cua_runtime_{runtime_scope}:");
        let cursor_registry = state.cursor_registry.clone();
        registry.retain_runtime_cleanup(move || {
            for cursor in cursor_registry
                .all_states()
                .into_iter()
                .filter(|cursor| cursor.config.cursor_id.starts_with(&prefix))
            {
                cursor_registry.remove(&cursor.config.cursor_id);
                crate::cursor::overlay::remove_cursor(cursor.config.cursor_id);
            }
        });
    }
    // Share the element cache with the recording-hook layer so it can
    // resolve element_index → window-local screenshot coords for click.png.
    crate::recording_hooks::set_element_cache(state.element_cache.clone());

    // Drop a disconnecting session's config overrides + owned cursor on
    // `session_end`. The daemon fans the session id out to this hook;
    // recording ownership is handled separately on the core RecordingSession.
    {
        let session_config = state.session_config.clone();
        let cursor_registry = state.cursor_registry.clone();
        let transient_ui_registry = state.transient_ui_registry.clone();
        let registration =
            cua_driver_core::session::register_scoped_session_end_hook(move |session_id| {
                session_config.clear(session_id);
                transient_ui_registry.clear_session(session_id);
                // Per-session agent cursor: the session_id is the cursor key when
                // the caller gave no explicit cursor_id, so dropping it here both
                // prunes the metadata registry and stops the overlay painting that
                // session's cursor. Both paths guard "default" so the anonymous /
                // one-shot cursor survives. Anonymous sessions that never created a
                // cursor are a harmless no-op.
                cursor_registry.remove(session_id);
                crate::cursor::overlay::remove_cursor(session_id.to_owned());
            });
        registry.retain_session_end_hook(registration);
        let revive_registration =
            cua_driver_core::session::register_scoped_session_revive_hook(move |session_id| {
                crate::cursor::overlay::revive_cursor(session_id.to_owned());
            });
        registry.retain_session_revive_hook(revive_registration);
    }

    registry.register(Box::new(list_apps::ListAppsTool));
    registry.register(Box::new(list_windows::ListWindowsTool));
    registry.register(Box::new(get_window_state::GetWindowStateTool::new(
        state.clone(),
    )));
    registry.register(Box::new(
        cua_driver_core::expectation::VerifyStateTool::new(Arc::new(
            cua_driver_core::expectation::ToolObservationProvider::new(
                Arc::new(list_windows::ListWindowsTool),
                Arc::new(get_window_state::GetWindowStateTool::new(state.clone())),
            ),
        )),
    ));
    registry.register(Box::new(launch_app::LaunchAppTool));
    registry.register(Box::new(kill_app::KillAppTool));
    let pid_window_candidates: WindowTargetCandidates = Arc::new(pid_window_target_candidates);
    registry.register(pid_window_guarded(
        bring_to_front::BringToFrontTool,
        &pid_window_candidates,
    ));
    registry.register(Box::new(set_window_frame::SetWindowFrameTool));
    registry.register(Box::new(invoke_menu::InvokeMenuTool));
    registry.register(pid_window_guarded(
        click::ClickTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        double_click::DoubleClickTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        right_click::RightClickTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        drag::DragTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        type_text::TypeTextTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        press_key::PressKeyTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        hotkey::HotkeyTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        set_value::SetValueTool::new(state.clone()),
        &pid_window_candidates,
    ));
    registry.register(pid_window_guarded(
        scroll::ScrollTool::new(state.clone()),
        &pid_window_candidates,
    ));
    cua_driver_core::clipboard::register_clipboard_tools(
        registry,
        Arc::new(clipboard::MacosClipboard::new()),
    );
    // The standalone `screenshot` tool was removed (#1692). The pixel-grounding
    // screenshot the Claude Code computer-use compat loop relies on now comes
    // from `get_window_state` (which always returns BOTH the tree AND a
    // screenshot — perception is mode-agnostic; `capture_mode` is deprecated/
    // ignored) for a window, or `get_desktop_state` for the whole screen.
    // `compat` no longer gates a tool swap here — the flag's live purpose is to
    // register the MCP server under the `cua-computer-use` name, which is what
    // triggers Claude Code's computer-use beta-tool injection (see cli.rs).
    let _ = compat;
    registry.register(Box::new(get_screen_size::GetScreenSizeTool));
    registry.register(Box::new(get_desktop_state::GetDesktopStateTool));
    registry.register(Box::new(get_cursor_position::GetCursorPositionTool));
    registry.register(Box::new(move_cursor::MoveCursorTool::new(state.clone())));
    registry.register(Box::new(cursor_tools::SetAgentCursorEnabledTool::new(
        state.clone(),
    )));
    registry.register(Box::new(cursor_tools::SetAgentCursorMotionTool::new(
        state.clone(),
    )));
    registry.register(Box::new(cursor_tools::SetAgentCursorThemeTool::new(
        state.clone(),
    )));
    registry.register(Box::new(cursor_tools::GetAgentCursorStateTool::new(
        state.clone(),
    )));
    registry.register(Box::new(check_permissions::CheckPermissionsTool::new(
        state.clone(),
    )));
    // `health_report` — single-call end-to-end diagnostics. Stable
    // schema_version="1" contract aimed at downstream consumers who must
    // not have to know cua-driver internals. Provider is platform-specific; tool plumbing is in
    // `cua_driver_core::health_report`.
    registry.register(Box::new(
        cua_driver_core::health_report::HealthReportTool::new(Arc::new(
            health_report::MacosHealthProvider,
        )),
    ));
    registry.register(Box::new(get_config::GetConfigTool::new(state.clone())));
    registry.register(Box::new(set_config::SetConfigTool::new(state.clone())));
    registry.register(Box::new(
        get_accessibility_tree::GetAccessibilityTreeTool::new(state.clone()),
    ));
    registry.register(Box::new(zoom::ZoomTool {
        state: state.clone(),
    }));
    // `type_text_chars` is intentionally NOT registered — Swift treats it as
    // a deprecated alias for `type_text` resolved at invoke time in
    // mcp-server's `ToolRegistry::invoke`. Keeping it out of the registry
    // means it doesn't show up in `tools/list` either, matching Swift's
    // ToolRegistry.swift (`type_text_chars` not in `handlers`) and the
    // platform-windows::build_registry which uses the same convention.
    // Touch the struct so it stays in this crate for the alias resolver.
    let _: &type_text_chars::TypeTextCharsTool =
        &type_text_chars::TypeTextCharsTool::new(state.clone());
    // Cross-platform `page` tool definition lives in mcp-server; macOS plugs in
    // its Apple-Events / CDP / AX-tree backend here.
    registry.register(Box::new(cua_driver_core::page::PageTool::new(Arc::new(
        page::MacOsPageBackend::new(state.clone()),
    ))));
    let browser_engine = cua_driver_core::browser::BrowserEngine::new_with_runtime_services(
        Arc::new(crate::browser::MacOsBrowserPlatform::new(
            state.cursor_registry.clone(),
        )),
        registry.approval_broker(),
        registry.protected_resource_ownership(),
    );
    cua_driver_core::browser::register_browser_tools(&browser_engine, registry);
    // Recording / replay + session-lifecycle tools are platform-independent.
    registry.register_recording_tools();
    registry.register_session_tools();
}

#[cfg(test)]
mod session_config_guard_tests {
    use super::*;
    use cua_driver_core::session::fire_session_end;

    fn overrides(max_dim: u32) -> ConfigOverrides {
        ConfigOverrides {
            max_image_dimension: Some(max_dim),
        }
    }

    #[test]
    fn ended_session_config_set_is_noop() {
        // THE FIX (config side): an ended session id keys the overrides map, so
        // an in-flight set_config after session_end must not re-create the entry
        // the reaper's clear hook removed. effective then falls back to global.
        let reg = SessionConfigRegistry::new();
        let global = DriverConfig::default();
        let sid = "wb-config-ended-Q9R8S7";
        fire_session_end(sid);
        assert!(cua_driver_core::session::is_session_ended(sid));

        reg.set(sid, overrides(800));
        let dim = reg.effective_max_image_dimension(Some(sid), &global);
        assert_eq!(
            dim, global.max_image_dimension,
            "ended session must not get an override entry"
        );
    }

    #[test]
    fn live_session_config_set_takes_effect() {
        let reg = SessionConfigRegistry::new();
        let global = DriverConfig::default();
        let sid = "wb-config-live-T1U2V3";
        assert!(!cua_driver_core::session::is_session_ended(sid));
        reg.set(sid, overrides(800));
        let dim = reg.effective_max_image_dimension(Some(sid), &global);
        assert_eq!(dim, 800, "live session override must apply");
    }
}

#[cfg(test)]
mod resize_registry_tests {
    use super::ResizeRegistry;

    /// Issue #2237: the registry was keyed by pid alone, so the downscale
    /// ratio recorded while snapshotting one window was applied to pixel
    /// clicks aimed at another window of the same app.
    #[test]
    fn resize_ratio_is_keyed_per_window() {
        let reg = ResizeRegistry::new();
        reg.set_ratio(800, 11, 2.0);
        reg.set_ratio(800, 22, 1.25);
        assert_eq!(reg.ratio(800, Some(11)), Some(2.0));
        assert_eq!(reg.ratio(800, Some(22)), Some(1.25));
    }

    #[test]
    fn undownscaled_window_reports_no_ratio() {
        let reg = ResizeRegistry::new();
        reg.set_ratio(800, 11, 2.0);
        assert_eq!(
            reg.ratio(800, Some(22)),
            None,
            "window 22 was never downscaled; it must not inherit window 11's ratio"
        );
    }

    #[test]
    fn clearing_one_window_keeps_the_other() {
        let reg = ResizeRegistry::new();
        reg.set_ratio(800, 11, 2.0);
        reg.set_ratio(800, 22, 1.25);
        reg.clear_ratio(800, 11);
        assert_eq!(reg.ratio(800, Some(11)), None);
        assert_eq!(reg.ratio(800, Some(22)), Some(1.25));
    }

    #[test]
    fn distinct_pids_with_the_same_window_id_do_not_collide() {
        let reg = ResizeRegistry::new();
        reg.set_ratio(800, 11, 2.0);
        reg.set_ratio(900, 11, 3.0);
        assert_eq!(reg.ratio(800, Some(11)), Some(2.0));
        assert_eq!(reg.ratio(900, Some(11)), Some(3.0));
    }

    /// Screen-scope callers pass no window_id. One window (the common case)
    /// keeps working; disagreeing windows refuse to guess.
    #[test]
    fn screen_scope_lookup_only_answers_when_windows_agree() {
        let reg = ResizeRegistry::new();
        assert_eq!(reg.ratio(800, None), None, "nothing recorded yet");
        reg.set_ratio(800, 11, 2.0);
        assert_eq!(
            reg.ratio(800, None),
            Some(2.0),
            "single window is unambiguous"
        );
        reg.set_ratio(800, 22, 2.0);
        assert_eq!(reg.ratio(800, None), Some(2.0), "agreeing windows answer");
        reg.set_ratio(800, 33, 1.25);
        assert_eq!(
            reg.ratio(800, None),
            None,
            "disagreeing windows must not pick one arbitrarily"
        );
    }
}

#[cfg(test)]
mod transient_keyboard_routing_tests {
    use super::*;

    fn error_code(result: cua_driver_core::protocol::ToolResult) -> String {
        result
            .structured_content
            .and_then(|value| value.get("code").cloned())
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .expect("structured refusal code")
    }

    #[test]
    fn visible_helper_without_same_session_observation_refuses_host_fallback() {
        let helper = crate::transient_ui::WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let error = foreground_keyboard_target_from_evidence(
            10,
            100,
            crate::transient_ui::RouteResolution::None,
            Some(crate::transient_ui::TransientHelperDetection::Unique(
                helper,
            )),
            false,
            true,
        )
        .expect_err("unobserved helper must fail closed");
        assert_eq!(error_code(error), "transient_ui_unobserved");
    }

    #[test]
    fn no_visible_helper_keeps_the_normal_host_foreground_path() {
        let target = foreground_keyboard_target_from_evidence(
            10,
            100,
            crate::transient_ui::RouteResolution::None,
            Some(crate::transient_ui::TransientHelperDetection::None),
            false,
            true,
        )
        .expect("ordinary host target");
        assert_eq!(target.pid, 10);
        assert_eq!(target.window_id, Some(100));
        assert_eq!(target.transient_route, None);
    }

    #[test]
    fn live_helper_route_is_refused_when_exact_policy_cannot_be_reauthorized() {
        let source = crate::transient_ui::WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let helper = crate::transient_ui::WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let error = foreground_keyboard_target_from_evidence(
            source.pid,
            source.window_id,
            crate::transient_ui::RouteResolution::Live(crate::transient_ui::TransientRoute {
                source,
                target: helper,
            }),
            None,
            false,
            false,
        )
        .expect_err("manifest/bounded routing must fail closed");
        assert_eq!(
            error_code(error),
            "transient_ui_target_reauthorization_required"
        );
    }

    #[test]
    fn stale_then_removed_route_still_refuses_a_visible_unobserved_helper() {
        let source = crate::transient_ui::WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let helper = crate::transient_ui::WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let first = foreground_keyboard_target_from_evidence(
            source.pid,
            source.window_id,
            crate::transient_ui::RouteResolution::Stale(crate::transient_ui::TransientRoute {
                source,
                target: helper,
            }),
            None,
            false,
            true,
        )
        .expect_err("stale route must refuse");
        assert_eq!(error_code(first), "transient_ui_stale");

        let second = foreground_keyboard_target_from_evidence(
            source.pid,
            source.window_id,
            crate::transient_ui::RouteResolution::None,
            Some(crate::transient_ui::TransientHelperDetection::Unique(
                helper,
            )),
            false,
            true,
        )
        .expect_err("fresh detection must prevent host fallback after stale cleanup");
        assert_eq!(error_code(second), "transient_ui_unobserved");
    }

    #[test]
    fn transient_pointer_surface_is_never_exposed_as_host_coordinates() {
        let refusal = transient_ui_pointer_refusal(10, Some(100), true);
        let structured = refusal.structured_content.unwrap();
        assert_eq!(structured["code"], "transient_ui_pointer_unsupported");
        assert_eq!(structured["pid"], 10);
        assert_eq!(structured["window_id"], 100);
        assert_eq!(structured["previously_observed"], true);

        let source = crate::transient_ui::WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let target = crate::transient_ui::WindowTarget {
            pid: 20,
            window_id: 200,
        };
        assert!(transient_pointer_target_from_evidence(
            10,
            Some(100),
            crate::transient_ui::RouteResolution::Live(crate::transient_ui::TransientRoute {
                source,
                target,
            }),
            None,
        )
        .is_err());
        assert!(transient_pointer_target_from_evidence(
            10,
            Some(100),
            crate::transient_ui::RouteResolution::None,
            Some(crate::transient_ui::TransientHelperDetection::Unique(
                target,
            )),
        )
        .is_err());
        assert!(transient_pointer_target_from_evidence(
            10,
            Some(100),
            crate::transient_ui::RouteResolution::None,
            Some(crate::transient_ui::TransientHelperDetection::None),
        )
        .is_ok());

        let direct = transient_ui_direct_target_refusal(20, Some(200));
        assert_eq!(
            direct.structured_content.unwrap()["code"],
            "transient_ui_direct_target_unsupported"
        );
    }

    #[test]
    fn same_pid_transient_refusal_exposes_only_a_narrow_exact_redirect() {
        let refusal = same_pid_transient_refusal(crate::transient_ui::SamePidTransientProof {
            source: crate::transient_ui::WindowTarget {
                pid: 42,
                window_id: 100,
            },
            target: crate::transient_ui::WindowTarget {
                pid: 42,
                window_id: 200,
            },
            classification:
                crate::transient_ui::SamePidTransientClassification::TrustedBlenderFileView,
        });
        assert_eq!(refusal.is_error, Some(true));
        let structured = refusal.structured_content.expect("structured refusal");
        assert_eq!(structured["code"], "same_pid_transient_in_front");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["pid"], 42);
        assert_eq!(structured["window_id"], 100);
        assert_eq!(structured["redirect"]["kind"], "same_pid_modal");
        assert_eq!(structured["redirect"]["pid"], 42);
        assert_eq!(structured["redirect"]["window_id"], 200);
        for field in [
            "same_pid",
            "unique",
            "layer_zero",
            "on_current_space",
            "above_source",
            "contained_by_source",
            "ax_window_live",
            "focused",
            "main",
        ] {
            assert_eq!(structured["redirect"]["proof"][field], true, "{field}");
        }
        assert_eq!(structured["redirect"]["proof"]["dialog_metadata"], false);
        assert_eq!(
            structured["redirect"]["proof"]["classification"],
            "trusted_blender_file_view"
        );
    }
}

#[cfg(test)]
mod cursor_overlay_facility_tests {
    use super::*;
    use cua_driver_core::tool::Tool;

    fn assert_facility_unavailable(result: cua_driver_core::protocol::ToolResult) {
        assert_eq!(result.is_error, Some(true));
        let refusal = result
            .structured_content
            .and_then(|value| value.get("refusal").cloned())
            .expect("structured refusal");
        assert_eq!(refusal["code"], "facility_unavailable");
        assert_eq!(refusal["facility"], "macos_cursor_overlay");
    }

    #[tokio::test]
    async fn cursor_control_refuses_without_main_thread_host_facility() {
        let state = Arc::new(ToolState::new(false, true, None));
        let result = cursor_tools::SetAgentCursorEnabledTool::new(state)
            .invoke(serde_json::json!({"enabled": true, "session": "test"}))
            .await;
        assert_facility_unavailable(result);
    }

    #[tokio::test]
    async fn window_cursor_move_refuses_without_main_thread_host_facility() {
        let state = Arc::new(ToolState::new(false, true, None));
        let result = move_cursor::MoveCursorTool::new(state)
            .invoke(serde_json::json!({"x": 10, "y": 20, "session": "test"}))
            .await;
        assert_facility_unavailable(result);
    }
}

// RecordingSession lives in cua-driver-core, but its `start()` pulls in the
// macOS cursor sampler (CoreGraphics), so the start-guard test runs here in
// platform-macos where build.rs links the frameworks — the core crate's test
// binary has no CoreGraphics linkage.
#[cfg(test)]
mod pixel_focus_readback_tests {
    use super::point_within_rect;

    #[test]
    fn confirms_a_point_inside_the_focused_element() {
        let composer = [100.0, 800.0, 250.0, 24.0];
        assert!(point_within_rect(composer, 220.0, 812.0));
        assert!(
            point_within_rect(composer, 100.0, 800.0),
            "top-left is inside"
        );
    }

    #[test]
    fn rejects_a_point_outside_the_focused_element() {
        // The WhatsApp case: the click targeted the composer but focus stayed on
        // the transcript above it, so the clicked point is not inside the
        // focused element's rect and the caller must escalate.
        let transcript = [100.0, 100.0, 640.0, 690.0];
        assert!(!point_within_rect(transcript, 220.0, 812.0));
    }

    #[test]
    fn excludes_the_far_edges() {
        let rect = [0.0, 0.0, 10.0, 10.0];
        assert!(!point_within_rect(rect, 10.0, 5.0));
        assert!(!point_within_rect(rect, 5.0, 10.0));
    }

    #[test]
    fn a_degenerate_rect_never_confirms() {
        assert!(!point_within_rect([5.0, 5.0, 0.0, 0.0], 5.0, 5.0));
        assert!(!point_within_rect([5.0, 5.0, -3.0, 10.0], 5.0, 6.0));
    }
}

#[cfg(test)]
mod recording_start_guard_tests {
    use cua_driver_core::recording::RecordingSession;
    use cua_driver_core::session::fire_session_end;

    #[test]
    fn start_refuses_for_ended_session_owner() {
        // THE FIX (recording side): an in-flight start_recording owned by a
        // session that already ended would leak an ffmpeg/SCStream process owned
        // by a dead session that is never reaped. start() must refuse.
        let rec = RecordingSession::new();
        let sid = "wb-recording-ended-W4X5Y6";
        fire_session_end(sid);
        assert!(cua_driver_core::session::is_session_ended(sid));

        let dir = std::env::temp_dir().join("wb-rec-ended");
        let err = rec.start(dir.to_str().unwrap(), false, Some(sid));
        assert!(err.is_err(), "start for an ended session owner must error");
        assert!(
            !rec.current_state().enabled,
            "no recording may start for a dead session"
        );
    }

    #[test]
    fn start_succeeds_for_live_session_owner() {
        let rec = RecordingSession::new();
        let sid = "wb-recording-live-Z7A8B9";
        assert!(!cua_driver_core::session::is_session_ended(sid));
        let dir = std::env::temp_dir().join("wb-rec-live");
        // record_video=false avoids spawning ffmpeg in the test.
        let ok = rec.start(dir.to_str().unwrap(), false, Some(sid));
        assert!(ok.is_ok(), "start for a live session owner must succeed");
        assert!(rec.current_state().enabled);
        let _ = rec.stop_owner(Some(sid));
    }

    #[test]
    fn start_succeeds_for_anonymous_owner() {
        // owner = None (CLI one-shot / legacy shim) is never gated.
        let rec = RecordingSession::new();
        let dir = std::env::temp_dir().join("wb-rec-anon");
        let ok = rec.start(dir.to_str().unwrap(), false, None);
        assert!(ok.is_ok(), "anonymous start must never be gated");
        assert!(rec.current_state().enabled);
        let _ = rec.stop_owner(None);
    }
}
