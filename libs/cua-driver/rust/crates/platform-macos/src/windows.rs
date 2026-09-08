//! macOS window enumeration via CGWindowList APIs.
//!
//! Uses the C-level CGWindowListCopyWindowInfo API which returns a CFArray
//! of CFDictionary objects describing each window.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window_id: u32,
    pub pid: i32,
    pub app_name: String,
    pub title: String,
    pub bounds: WindowBounds,
    pub layer: i32,
    pub z_index: usize,
    pub is_on_screen: bool,
    /// Active Space on the display WindowServer associates with this window.
    /// This can differ between windows when displays use independent Spaces.
    pub current_space_id: Option<u64>,
    pub on_current_space: Option<bool>,
    pub space_ids: Option<Vec<u64>>,
}

pub(crate) struct WindowEnumeration {
    pub(crate) windows: Vec<WindowInfo>,
    pub(crate) current_space_id: Option<u64>,
}

// ── CGWindow option flags ─────────────────────────────────────────────────────
// Apple-canonical kCG* naming preserved to match the public Apple headers — the
// upper-case-globals lint would rename them to KCG_..., which would silently
// shadow the Apple-namespaced constant references in any future code that
// re-introduces them. Mirrors platform-windows::uia/windows_enum.rs which uses
// the same allow for UIA_* constants.
#[allow(non_upper_case_globals)]
const kCGWindowListExcludeDesktopElements: u32 = 16;
#[allow(non_upper_case_globals)]
const kCGWindowListOptionOnScreenOnly: u32 = 1;
#[allow(non_upper_case_globals)]
const kCGNullWindowID: u32 = 0;

// ── Internal CGWindowInfo parsing ─────────────────────────────────────────────
//
// We use `system_profiler` workaround via `CGWindowListCopyWindowInfo` which
// returns a plist-like structure. The simplest cross-compile-safe approach
// is to dump via `osascript` or use the Objective-C runtime.
//
// For the initial version we use the `core-foundation` crate + direct C linkage.

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(
        option: u32,
        relativeToWindow: u32,
    ) -> core_foundation::array::CFArrayRef;
}

/// Enumerate all windows (including off-screen).
pub fn all_windows() -> Vec<WindowInfo> {
    all_windows_with_space_snapshot().windows
}

pub(crate) fn all_windows_with_space_snapshot() -> WindowEnumeration {
    enumerate_windows(kCGWindowListExcludeDesktopElements, LayerFilter::ZeroOnly)
}

/// Enumerate layer-0 windows that are meaningful automation targets.
///
/// Unlike [`all_windows`], this performs a narrowly-gated AX check for macOS
/// capture-indicator helpers. Keep hot compositor/PiP polling on the raw
/// enumeration functions above.
pub(crate) fn all_automation_windows() -> Vec<WindowInfo> {
    all_automation_windows_with_space_snapshot().windows
}

pub(crate) fn all_automation_windows_with_space_snapshot() -> WindowEnumeration {
    let enumeration = all_windows_with_space_snapshot();
    let evidence = enumeration.windows.clone();
    filter_automation_enumeration_with_evidence(enumeration, &evidence)
}

/// Enumerate only on-screen windows.
pub fn visible_windows() -> Vec<WindowInfo> {
    visible_windows_with_space_snapshot().windows
}

pub(crate) fn visible_windows_with_space_snapshot() -> WindowEnumeration {
    enumerate_windows(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        LayerFilter::ZeroOnly,
    )
}

pub(crate) fn visible_automation_windows() -> Vec<WindowInfo> {
    visible_automation_windows_with_space_snapshot().windows
}

pub(crate) fn visible_automation_windows_with_space_snapshot() -> WindowEnumeration {
    let enumeration = visible_windows_with_space_snapshot();

    // `kCGWindowListOptionOnScreenOnly` includes the app-owned half of
    // WindowSharingSessionButton, while its ThemeWidgetControlViewService twin
    // is only present in the full WindowServer snapshot. Keep that off-screen
    // twin as classification evidence without returning it in the visible
    // projection.
    let evidence = all_windows();
    filter_automation_enumeration_with_evidence(enumeration, &evidence)
}

/// Enumerate windows on every CGWindow layer, including the accessory layers
/// (`layer != 0`) that [`all_windows`] hides.
///
/// Only used to answer "does this CGWindowID exist, and who owns it?" — the
/// question `list_windows` must NOT answer, because surfacing tooltips,
/// popovers, the Dock and every NSMenu window would swamp callers. Keeping the
/// layer filter on enumeration and off identity lookup is what lets
/// `get_window_state` tell "no such window" apart from "exists, but is not a
/// layer-0 window" (issue #2237).
pub(crate) fn all_windows_including_accessory_layers() -> Vec<WindowInfo> {
    enumerate_windows(kCGWindowListExcludeDesktopElements, LayerFilter::AnyLayer).windows
}

/// Which CGWindow layers an enumeration admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerFilter {
    /// Normal application windows only — what `list_windows` reports.
    ZeroOnly,
    /// Every layer, accessory windows included.
    AnyLayer,
}

fn enumerate_windows(options: u32, layers: LayerFilter) -> WindowEnumeration {
    use core_foundation::{
        array::CFArray,
        base::{CFGetTypeID, CFTypeRef, TCFType},
        boolean::CFBoolean,
        dictionary::CFDictionary,
        number::CFNumber,
        string::CFString,
    };
    use std::os::raw::c_void;

    let space_query = (layers == LayerFilter::ZeroOnly)
        .then(crate::input::skylight::SpaceQuery::new)
        .flatten();
    let current_space_id = space_query
        .as_ref()
        .and_then(|query| query.current_space_id());

    let raw_ref = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
    if raw_ref.is_null() {
        return WindowEnumeration {
            windows: vec![],
            current_space_id,
        };
    }

    let raw: CFArray<CFTypeRef> = unsafe { CFArray::wrap_under_create_rule(raw_ref as _) };
    let total = raw.len() as usize;
    let mut results = Vec::new();

    for (idx, item) in raw.iter().enumerate() {
        let item = *item;
        // Each item should be a CFDictionary.
        let dict_type = CFDictionary::<*const c_void, *const c_void>::type_id();
        if unsafe { CFGetTypeID(item) } != dict_type {
            continue;
        }

        let dict: CFDictionary<*const c_void, *const c_void> =
            unsafe { CFDictionary::wrap_under_get_rule(item as _) };

        // Helper: get number from dict by key string.
        let get_num = |key: &str| -> i64 {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFNumber::type_id() {
                        CFNumber::wrap_under_get_rule(v as _).to_i64()
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
        };

        let get_str = |key: &str| -> String {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFString::type_id() {
                        Some(CFString::wrap_under_get_rule(v as _).to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        };

        let get_bool = |key: &str| -> bool {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .map(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFBoolean::type_id() {
                        bool::from(CFBoolean::wrap_under_get_rule(v as _))
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
        };

        let window_id = get_num("kCGWindowNumber") as u32;
        let pid = get_num("kCGWindowOwnerPID") as i32;
        let app_name = get_str("kCGWindowOwnerName");
        let title = get_str("kCGWindowName");
        let layer = get_num("kCGWindowLayer") as i32;
        let is_on_screen = get_bool("kCGWindowIsOnscreen");

        // Only include layer-0 windows, unless the caller asked for every layer.
        if layer != 0 && layers == LayerFilter::ZeroOnly {
            continue;
        }

        // Parse bounds dict.
        let bounds = {
            let bk = CFString::new("kCGWindowBounds");
            dict.find(bk.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFDictionary::<*const c_void, *const c_void>::type_id() {
                        let bd: CFDictionary<*const c_void, *const c_void> =
                            CFDictionary::wrap_under_get_rule(v as _);
                        let x = get_bounds_num(&bd, "X");
                        let y = get_bounds_num(&bd, "Y");
                        let w = get_bounds_num(&bd, "Width");
                        let h = get_bounds_num(&bd, "Height");
                        Some(WindowBounds {
                            x,
                            y,
                            width: w,
                            height: h,
                        })
                    } else {
                        None
                    }
                })
                .unwrap_or(WindowBounds {
                    x: 0.,
                    y: 0.,
                    width: 0.,
                    height: 0.,
                })
        };

        // z_index: CGWindowList front-to-back → assign reverse index.
        let z_index = z_index_from_front_to_back(total, idx);

        results.push(WindowInfo {
            window_id,
            pid,
            app_name,
            title,
            bounds,
            layer,
            z_index,
            is_on_screen,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        });
    }

    if layers == LayerFilter::ZeroOnly {
        let Some(query) = &space_query else {
            return WindowEnumeration {
                windows: results,
                current_space_id,
            };
        };
        for window in &mut results {
            let space_ids = query.window_space_ids(window.window_id);
            let display_space_id = space_ids
                .as_ref()
                .and_then(|_| query.current_space_for_window(window.window_id));
            apply_window_space_metadata(window, space_ids, display_space_id);
        }
    }

    WindowEnumeration {
        windows: results,
        current_space_id,
    }
}

fn filter_automation_enumeration_with_evidence(
    mut enumeration: WindowEnumeration,
    evidence: &[WindowInfo],
) -> WindowEnumeration {
    enumeration.windows =
        filter_automation_windows_with_evidence(enumeration.windows, evidence, |window| {
            use crate::ax::window_classification::{
                classify_automation_window, AutomationWindowClass,
            };

            classify_automation_window(window.pid, window.window_id)
                .map(|class| class == AutomationWindowClass::SystemCaptureIndicator)
        });
    enumeration
}

/// Cheaply narrow AX classification to small, titleless layer-0 surfaces.
///
/// Geometry is deliberately only a prefilter. The final exclusion requires
/// the exact `WindowSharingSessionButton` AX marker, so legitimate compact
/// utility windows remain visible to automation.
fn capture_indicator_probe_candidate(window: &WindowInfo) -> bool {
    window.layer == 0
        && window.is_on_screen
        && (window.title.is_empty() || window.title == "Window")
        && window.bounds.width > 0.0
        && window.bounds.width <= 256.0
        && window.bounds.height > 0.0
        && window.bounds.height <= 96.0
}

const THEME_WIDGET_CONTROL_VIEW_SERVICE: &str = "ThemeWidgetControlViewService";

fn bounds_match(left: &WindowBounds, right: &WindowBounds) -> bool {
    const TOLERANCE_POINTS: f64 = 0.5;
    (left.x - right.x).abs() <= TOLERANCE_POINTS
        && (left.y - right.y).abs() <= TOLERANCE_POINTS
        && (left.width - right.width).abs() <= TOLERANCE_POINTS
        && (left.height - right.height).abs() <= TOLERANCE_POINTS
}

/// ScreenCaptureKit's WindowSharingSessionButton is represented by two
/// layer-0 CG windows with identical bounds: an app-owned `"Window"` surface
/// and an untitled ThemeWidgetControlViewService surface. The app-owned half
/// is intentionally absent from `AXWindows`, so the exact AX classifier cannot
/// prove what it is. Treat the paired WindowServer representation as the
/// bounded fallback proof instead of guessing from compact geometry alone.
fn has_capture_indicator_twin(window: &WindowInfo, evidence: &[WindowInfo]) -> bool {
    let app_owned_half =
        window.title == "Window" && window.app_name != THEME_WIDGET_CONTROL_VIEW_SERVICE;
    let service_half =
        window.app_name == THEME_WIDGET_CONTROL_VIEW_SERVICE && window.title.is_empty();
    if !app_owned_half && !service_half {
        return false;
    }

    evidence.iter().any(|other| {
        if other.window_id == window.window_id
            || other.pid == window.pid
            || other.layer != 0
            || !bounds_match(&window.bounds, &other.bounds)
        {
            return false;
        }

        if app_owned_half {
            other.app_name == THEME_WIDGET_CONTROL_VIEW_SERVICE && other.title.is_empty()
        } else {
            other.title == "Window" && other.app_name != THEME_WIDGET_CONTROL_VIEW_SERVICE
        }
    })
}

#[cfg(test)]
fn filter_automation_windows_with<F>(
    windows: Vec<WindowInfo>,
    is_system_capture_indicator: F,
) -> Vec<WindowInfo>
where
    F: FnMut(&WindowInfo) -> Option<bool>,
{
    let evidence = windows.clone();
    filter_automation_windows_with_evidence(windows, &evidence, is_system_capture_indicator)
}

fn filter_automation_windows_with_evidence<F>(
    windows: Vec<WindowInfo>,
    evidence: &[WindowInfo],
    mut is_system_capture_indicator: F,
) -> Vec<WindowInfo>
where
    F: FnMut(&WindowInfo) -> Option<bool>,
{
    windows
        .into_iter()
        .filter(|window| {
            if has_capture_indicator_twin(window, evidence) {
                return false;
            }

            if !capture_indicator_probe_candidate(window) {
                return true;
            }

            // Fail open when AX is unavailable. We must never hide an unknown
            // application window merely because it has compact geometry. The
            // WindowServer twin check above is the only non-AX exclusion.
            !matches!(is_system_capture_indicator(window), Some(true))
        })
        .collect()
}

fn apply_window_space_metadata(
    window: &mut WindowInfo,
    space_ids: Option<Vec<u64>>,
    current_space_id: Option<u64>,
) {
    window.on_current_space = window_on_current_space(space_ids.as_deref(), current_space_id);
    window.current_space_id = current_space_id;
    window.space_ids = space_ids;
}

fn window_on_current_space(
    space_ids: Option<&[u64]>,
    current_space_id: Option<u64>,
) -> Option<bool> {
    Some(space_ids?.contains(&current_space_id?))
}

fn z_index_from_front_to_back(total: usize, position: usize) -> usize {
    total.saturating_sub(position)
}

fn get_bounds_num(
    dict: &core_foundation::dictionary::CFDictionary<
        *const std::os::raw::c_void,
        *const std::os::raw::c_void,
    >,
    key: &str,
) -> f64 {
    use core_foundation::{
        base::{CFGetTypeID, TCFType},
        number::CFNumber,
        string::CFString,
    };
    use std::os::raw::c_void;

    let k = CFString::new(key);
    dict.find(k.as_concrete_TypeRef() as *const c_void)
        .and_then(|v| unsafe {
            let v = *v;
            if CFGetTypeID(v) == CFNumber::type_id() {
                CFNumber::wrap_under_get_rule(v as _).to_f64()
            } else {
                None
            }
        })
        .unwrap_or(0.0)
}

/// Look up a window by its CGWindowID across every layer.
///
/// Returns `None` only when WindowServer has no record of the id at all —
/// which is precisely the "closed or fabricated window_id" signal callers need.
pub fn window_info_by_id(window_id: u32) -> Option<WindowInfo> {
    all_windows_including_accessory_layers()
        .into_iter()
        .find(|w| w.window_id == window_id)
}

/// Look up a window's bounds by its CGWindowID.
///
/// Returns `None` if the window is not currently known to WindowServer
/// (e.g. it was closed or the window_id is stale).
pub fn window_bounds_by_id(window_id: u32) -> Option<WindowBounds> {
    window_info_by_id(window_id).map(|w| w.bounds)
}

/// Who owns a requested CGWindowID, as seen by a caller that asked about `pid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowOwner {
    /// The window exists and `pid` owns it.
    SamePid,
    /// The window exists, but a different process owns it. macOS hosts a
    /// sandboxed app's Open/Save panel in
    /// `com.apple.appkit.xpc.openAndSavePanelService`, so the panel's
    /// CGWindowID belongs to that service and not to the app that opened it
    /// (issue #2237).
    ForeignPid {
        owner_pid: i32,
        owner_app_name: String,
    },
    /// WindowServer has no record of the id — closed, stale, or fabricated.
    Unknown,
}

/// Pure form of [`resolve_window_owner`] over an already-enumerated window
/// list, so the ownership decision is testable without a WindowServer.
pub fn resolve_window_owner_in(windows: &[WindowInfo], pid: i32, window_id: u32) -> WindowOwner {
    match windows.iter().find(|w| w.window_id == window_id) {
        None => WindowOwner::Unknown,
        Some(w) if w.pid == pid => WindowOwner::SamePid,
        Some(w) => WindowOwner::ForeignPid {
            owner_pid: w.pid,
            owner_app_name: w.app_name.clone(),
        },
    }
}

/// Resolve whether `pid` really owns `window_id`. Blocking (one CGWindowList
/// enumeration).
pub fn resolve_window_owner(pid: i32, window_id: u32) -> WindowOwner {
    resolve_window_owner_in(&all_windows_including_accessory_layers(), pid, window_id)
}

/// Select the best window_id for a pid.
pub fn resolve_main_window_id(pid: i32) -> anyhow::Result<u32> {
    let windows = all_automation_windows();
    resolve_main_window_id_in(&windows, pid)
}

fn resolve_main_window_id_in(windows: &[WindowInfo], pid: i32) -> anyhow::Result<u32> {
    let pid_windows: Vec<&WindowInfo> = windows.iter().filter(|w| w.pid == pid).collect();
    if pid_windows.is_empty() {
        anyhow::bail!("pid {pid} has no windows");
    }
    let mut on_screen: Vec<&&WindowInfo> = pid_windows.iter().filter(|w| w.is_on_screen).collect();
    if !on_screen.is_empty() {
        on_screen.sort_by_key(|window| std::cmp::Reverse(window.z_index));
        return Ok(on_screen[0].window_id);
    }
    let largest = pid_windows.iter().max_by(|a, b| {
        let area_a = a.bounds.width * a.bounds.height;
        let area_b = b.bounds.width * b.bounds.height;
        area_a
            .partial_cmp(&area_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(largest.unwrap().window_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cg_front_to_back_order_normalizes_to_higher_is_frontmost() {
        let indices: Vec<_> = (0..3)
            .map(|position| z_index_from_front_to_back(3, position))
            .collect();
        assert_eq!(indices, vec![3, 2, 1]);
        assert!(indices[0] > indices[2]);
    }

    #[test]
    fn space_membership_checks_all_spaces_for_a_window() {
        assert_eq!(window_on_current_space(Some(&[2, 4]), Some(4)), Some(true));
        assert_eq!(window_on_current_space(Some(&[2, 4]), Some(3)), Some(false));
    }

    #[test]
    fn space_membership_stays_unknown_without_either_side() {
        assert_eq!(window_on_current_space(None, Some(4)), None);
        assert_eq!(window_on_current_space(Some(&[4]), None), None);
    }

    #[test]
    fn per_window_current_space_is_the_one_used_for_membership() {
        let mut secondary_display_window = window(42, 800, "TextEdit");
        apply_window_space_metadata(&mut secondary_display_window, Some(vec![2, 4]), Some(4));

        assert_eq!(secondary_display_window.current_space_id, Some(4));
        assert_eq!(secondary_display_window.space_ids, Some(vec![2, 4]));
        assert_eq!(secondary_display_window.on_current_space, Some(true));
        assert!(secondary_display_window
            .space_ids
            .as_deref()
            .is_some_and(|spaces| spaces.contains(
                &secondary_display_window
                    .current_space_id
                    .expect("display Space must be present")
            )));
    }

    fn window(window_id: u32, pid: i32, app_name: &str) -> WindowInfo {
        WindowInfo {
            window_id,
            pid,
            app_name: app_name.into(),
            title: String::new(),
            bounds: WindowBounds {
                x: 0.,
                y: 580.,
                width: 500.,
                height: 500.,
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
    fn window_sharing_session_button_is_not_an_automation_window() {
        let mut main = window(10, 800, "Calculator");
        main.title = "Calculator".into();
        main.bounds.width = 230.0;
        main.bounds.height = 408.0;
        main.z_index = 2;

        let mut helper = window(11, 800, "Calculator");
        helper.title = "Window".into();
        helper.bounds.width = 66.0;
        helper.bounds.height = 20.0;
        helper.z_index = 3;

        let filtered = filter_automation_windows_with(vec![main, helper], |candidate| {
            Some(candidate.window_id == 11)
        });

        assert_eq!(
            filtered
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>(),
            vec![10]
        );
        assert_eq!(resolve_main_window_id_in(&filtered, 800).unwrap(), 10);
    }

    #[test]
    fn ax_unresolved_capture_indicator_twin_is_filtered() {
        let mut main = window(10, 800, "Calculator");
        main.title = "Calculator".into();
        main.bounds.width = 230.0;
        main.bounds.height = 408.0;
        main.z_index = 2;

        // ScreenCaptureKit exposes this injected surface through CGWindowList,
        // but it is not necessarily present in the owning app's AXWindows.
        let mut helper = window(11, 800, "Calculator");
        helper.title = "Window".into();
        helper.bounds.width = 66.0;
        helper.bounds.height = 20.0;
        helper.z_index = 3;

        let mut twin = helper.clone();
        twin.window_id = 12;
        twin.pid = 900;
        twin.app_name = THEME_WIDGET_CONTROL_VIEW_SERVICE.into();
        twin.title.clear();
        twin.is_on_screen = false;

        let filtered = filter_automation_windows_with(vec![helper, twin, main], |_| None);

        assert_eq!(
            filtered
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>(),
            vec![10]
        );
        assert_eq!(resolve_main_window_id_in(&filtered, 800).unwrap(), 10);
    }

    #[test]
    fn visible_projection_keeps_offscreen_twin_as_classification_evidence() {
        let mut main = window(10, 800, "Calculator");
        main.title = "Calculator".into();
        main.bounds.width = 230.0;
        main.bounds.height = 408.0;

        let mut helper = window(11, 800, "Calculator");
        helper.title = "Window".into();
        helper.bounds.width = 66.0;
        helper.bounds.height = 20.0;

        let mut twin = helper.clone();
        twin.window_id = 12;
        twin.pid = 900;
        twin.app_name = THEME_WIDGET_CONTROL_VIEW_SERVICE.into();
        twin.title.clear();
        twin.is_on_screen = false;

        let visible = vec![helper, main];
        let evidence = [visible[0].clone(), visible[1].clone(), twin];
        let filtered = filter_automation_windows_with_evidence(visible, &evidence, |_| None);

        assert_eq!(
            filtered
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>(),
            vec![10]
        );
    }

    #[test]
    fn unpaired_compact_window_named_window_is_preserved_when_ax_is_unavailable() {
        let mut compact = window(11, 800, "Utility");
        compact.title = "Window".into();
        compact.bounds.width = 66.0;
        compact.bounds.height = 20.0;

        let filtered = filter_automation_windows_with(vec![compact.clone()], |_| None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].window_id, compact.window_id);
    }

    #[test]
    fn compact_titleless_window_is_preserved_without_exact_ax_marker() {
        let mut compact = window(11, 800, "Utility");
        compact.bounds.width = 66.0;
        compact.bounds.height = 20.0;

        let ordinary = filter_automation_windows_with(vec![compact.clone()], |_| Some(false));
        assert_eq!(ordinary.len(), 1);
        assert_eq!(ordinary[0].window_id, compact.window_id);

        let ax_unavailable = filter_automation_windows_with(vec![compact], |_| None);
        assert_eq!(ax_unavailable.len(), 1, "AX failure must fail open");
    }

    #[test]
    fn owner_resolves_same_pid() {
        let windows = vec![window(42, 800, "TextEdit")];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 42),
            WindowOwner::SamePid
        );
    }

    /// Issue #2237: TextEdit's Open panel is a layer-0 CGWindow owned by
    /// `com.apple.appkit.xpc.openAndSavePanelService`, not by TextEdit. The
    /// caller must be told the real owner pid, not handed TextEdit's menu bar.
    #[test]
    fn owner_detects_out_of_process_panel_host() {
        let windows = vec![
            window(41, 800, "TextEdit"),
            window(42, 900, "Open and Save Panel Service"),
        ];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 42),
            WindowOwner::ForeignPid {
                owner_pid: 900,
                owner_app_name: "Open and Save Panel Service".into(),
            }
        );
    }

    #[test]
    fn owner_is_unknown_for_fabricated_id() {
        let windows = vec![window(42, 800, "TextEdit")];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 0xFFFF_FFF0),
            WindowOwner::Unknown
        );
    }

    #[test]
    fn owner_is_unknown_for_zero_id() {
        // kCGNullWindowID is never a real window number.
        let windows = vec![window(42, 800, "TextEdit")];
        assert_eq!(
            resolve_window_owner_in(&windows, 800, 0),
            WindowOwner::Unknown
        );
    }

    #[test]
    fn owner_is_unknown_after_the_window_closes() {
        // Stale id: it was enumerated once, then the panel was dismissed.
        let before = vec![window(42, 900, "Open and Save Panel Service")];
        assert_eq!(
            resolve_window_owner_in(&before, 900, 42),
            WindowOwner::SamePid
        );
        assert_eq!(resolve_window_owner_in(&[], 900, 42), WindowOwner::Unknown);
    }
}
