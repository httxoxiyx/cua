//! Routing and detection for app-owned transient UI.
//!
//! AppKit and system frameworks sometimes put a modal prompt in an XPC view
//! service rather than in the requesting application's process.  The helper is
//! intentionally absent from `list_apps`, but its WindowServer surface still
//! needs to be observable and, after an explicit foreground escalation,
//! keyboard-addressable through the host application's stable public target.
//!
//! Some applications (notably Blender) instead create a second layer-0 window
//! in the same process for a modal workflow.  Capturing the original window can
//! still composite that front window into the returned image, while coordinates
//! and input remain scoped to the original CGWindowID.  The bounded detector
//! below recognizes only one uniquely focused, contained, frontmost successor;
//! callers must then re-address that exact window rather than silently replaying
//! coordinates against the cached host.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use core_foundation::base::{CFRelease, CFTypeRef};

use crate::ax::bindings::{
    ax_get_window_id, copy_bool_attr, copy_string_attr, try_copy_ax_windows,
    AXUIElementCreateApplication, AXUIElementSetMessagingTimeout,
};
use crate::windows::{WindowBounds, WindowInfo};

// AppKit's `NSModalPanelWindowLevel` / CoreGraphics layer for modal panels.
// Restricting routing to this level prevents unrelated accessory-process
// tooltips, overlays, and status items from being inferred as host UI.
const MODAL_PANEL_WINDOW_LAYER: i32 = 8;
const SHORTCUTS_HOST_BUNDLE_ID: &str = "com.apple.shortcuts";
const SHORTCUTS_HELPER_BUNDLE_ID: &str = "com.apple.WorkflowKit.ShortcutsViewService";
const SHORTCUTS_HELPER_SYSTEM_PATH: &str = "/System/Library/PrivateFrameworks/WorkflowKit.framework/XPCServices/ShortcutsViewService.xpc/Contents/MacOS/ShortcutsViewService";
const CRYPTEX_SYSTEM_PREFIX: &str = "/System/Volumes/Preboot/Cryptexes/";
const BLENDER_BUNDLE_ID: &str = "org.blenderfoundation.blender";
const BLENDER_FILE_VIEW_TITLE: &str = "Blender File View";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct WindowTarget {
    pub(crate) pid: i32,
    pub(crate) window_id: u32,
}

/// Session namespace for transient input authorization.  The anonymous shape
/// is a distinct enum variant rather than a magic string, so no caller-chosen
/// `_session_id` can collide with process-scoped one-shot calls.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum TransientSessionKey {
    Anonymous,
    Session(String),
}

impl TransientSessionKey {
    pub(crate) fn from_args(args: &serde_json::Value) -> Self {
        args.get("_session_id")
            .and_then(serde_json::Value::as_str)
            .filter(|session| !session.is_empty())
            .map(|session| Self::Session(session.to_owned()))
            .unwrap_or(Self::Anonymous)
    }

    fn is_ended(&self) -> bool {
        match self {
            Self::Anonymous => false,
            Self::Session(session) => cua_driver_core::session::is_session_ended(session),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RouteKey {
    session: TransientSessionKey,
    source: WindowTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransientRoute {
    pub(crate) source: WindowTarget,
    pub(crate) target: WindowTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteResolution {
    None,
    Live(TransientRoute),
    Stale(TransientRoute),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransientHelperDetection {
    None,
    Unique(WindowTarget),
    Ambiguous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SamePidTransientClassification {
    DialogMetadata,
    TrustedBlenderFileView,
}

impl SamePidTransientClassification {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DialogMetadata => "dialog_metadata",
            Self::TrustedBlenderFileView => "trusted_blender_file_view",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SamePidTransientProof {
    pub(crate) source: WindowTarget,
    pub(crate) target: WindowTarget,
    pub(crate) classification: SamePidTransientClassification,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SamePidTransientDetection {
    None,
    Unique(SamePidTransientProof),
    Ambiguous,
    Indeterminate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SamePidAxWindowFacts {
    window_id: u32,
    /// Position in the application's `AXWindows` array (front/focused first).
    order: usize,
    subrole: Option<String>,
    identifier: Option<String>,
    modal: Option<bool>,
    focused: Option<bool>,
    main: Option<bool>,
}

impl SamePidAxWindowFacts {
    fn has_dialog_metadata(&self) -> bool {
        self.modal == Some(true)
            || self
                .subrole
                .as_deref()
                .is_some_and(|value| matches!(value, "AXDialog" | "AXSystemDialog" | "AXSheet"))
            || self
                .identifier
                .as_deref()
                .is_some_and(|value| matches!(value, "open-panel" | "save-panel"))
    }
}

impl TransientHelperDetection {
    pub(crate) fn unique_target(self) -> Option<WindowTarget> {
        match self {
            Self::Unique(target) => Some(target),
            Self::None | Self::Ambiguous => None,
        }
    }

    pub(crate) fn helper_is_visible(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Observation-created aliases from a stable host window to its current
/// out-of-process transient.  Entries never authorize input on their own:
/// every lookup revalidates the WindowServer/process evidence, and callers use
/// them only for the explicit foreground keyboard rung.
#[derive(Default)]
pub(crate) struct TransientUiRegistry {
    inner: Mutex<HashMap<RouteKey, WindowTarget>>,
}

impl TransientUiRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record(
        &self,
        session: &TransientSessionKey,
        source: WindowTarget,
        target: Option<WindowTarget>,
    ) {
        let mut routes = self.lock_routes();
        let key = RouteKey {
            session: session.clone(),
            source,
        };
        // Check after acquiring the registry lock. If session_end raced this
        // call, its global ended marker is already authoritative and this
        // write must not recreate an alias after the cleanup hook removed it.
        if session.is_ended() {
            routes.remove(&key);
            return;
        }
        match target {
            Some(target) => {
                routes.insert(key, target);
            }
            None => {
                routes.remove(&key);
            }
        }
    }

    pub(crate) fn clear_route(
        &self,
        session: &TransientSessionKey,
        source: WindowTarget,
    ) -> Option<WindowTarget> {
        self.lock_routes().remove(&RouteKey {
            session: session.clone(),
            source,
        })
    }

    pub(crate) fn clear_session(&self, session: &str) {
        let session = TransientSessionKey::Session(session.to_owned());
        self.lock_routes().retain(|key, _| key.session != session);
    }

    #[cfg(test)]
    pub(crate) fn recorded_target(
        &self,
        session: &TransientSessionKey,
        source: WindowTarget,
    ) -> Option<WindowTarget> {
        self.lock_routes()
            .get(&RouteKey {
                session: session.clone(),
                source,
            })
            .copied()
    }

    /// Revalidate a previously observed route against fresh WindowServer and
    /// NSRunningApplication state.  A changed/disappeared helper is reported as
    /// stale instead of falling back to the host window: typing into the host's
    /// previously focused field would be the dangerous failure mode.
    pub(crate) fn resolve_live(
        &self,
        session: &TransientSessionKey,
        source: WindowTarget,
    ) -> RouteResolution {
        self.resolve_with(session, source, resolve_visible_transient_helper)
    }

    fn resolve_with(
        &self,
        session: &TransientSessionKey,
        source: WindowTarget,
        resolve: impl FnOnce(WindowTarget) -> Option<WindowTarget>,
    ) -> RouteResolution {
        let key = RouteKey {
            session: session.clone(),
            source,
        };
        let recorded = {
            let mut routes = self.lock_routes();
            if session.is_ended() {
                routes.remove(&key);
                None
            } else {
                routes.get(&key).copied()
            }
        };
        let Some(target) = recorded else {
            return RouteResolution::None;
        };

        let live = resolve(source);
        let mut routes = self.lock_routes();
        if session.is_ended() {
            if routes.get(&key).copied() == Some(target) {
                routes.remove(&key);
            }
            return RouteResolution::Stale(TransientRoute { source, target });
        }
        // Observation may have replaced this route while WindowServer was
        // being queried. Never delete or authorize against the newer value.
        if routes.get(&key).copied() != Some(target) {
            return RouteResolution::Stale(TransientRoute { source, target });
        }
        if live == Some(target) {
            RouteResolution::Live(TransientRoute { source, target })
        } else {
            routes.remove(&key);
            RouteResolution::Stale(TransientRoute { source, target })
        }
    }

    /// A poisoned registry must not take down the driver or retain an input
    /// authorization that could have been only partially updated.  Clear all
    /// aliases before recovering the mutex, making the failure mode equivalent
    /// to "no transient route observed" until the next successful observation.
    fn lock_routes(&self) -> MutexGuard<'_, HashMap<RouteKey, WindowTarget>> {
        match self.inner.lock() {
            Ok(routes) => routes,
            Err(poisoned) => {
                let mut routes = poisoned.into_inner();
                routes.clear();
                self.inner.clear_poison();
                routes
            }
        }
    }
}

pub(crate) fn resolve_visible_transient_helper(source: WindowTarget) -> Option<WindowTarget> {
    detect_visible_transient_helper(source).unique_target()
}

/// Detect a same-process transient window that has taken exclusive foreground
/// ownership from `source`.
///
/// This is intentionally much narrower than "pick the topmost window for the
/// pid".  A candidate must be a live layer-0 AX window on the same current
/// Space, strictly contained by the requested source, precede it in the app's
/// AX window order, and be both the application's focused and main AX window
/// while the source is neither. Generic windows additionally require native
/// dialog/modal metadata. Blender 4.5's File View exposes neither, so its only
/// fallback is a deliberately narrow bundle-id plus exact native window-title
/// allowlist. A merely focused, contained sibling is never redirectable.
pub(crate) fn detect_same_pid_transient_in_front(
    source: WindowTarget,
) -> SamePidTransientDetection {
    let enumeration = crate::windows::all_automation_windows_with_space_snapshot();
    if !enumeration.succeeded {
        return SamePidTransientDetection::Indeterminate;
    }
    let windows = enumeration.windows;
    // Avoid an AX round-trip on the overwhelmingly common single-window path.
    // WindowServer geometry is only a prefilter; it never authorizes a
    // redirect without the exact AX focus/main proof collected below.
    if !has_same_pid_transient_geometry_candidate(&windows, source) {
        return SamePidTransientDetection::None;
    }
    let ax_facts = match same_pid_ax_window_facts(source.pid) {
        Ok(facts) => facts,
        Err(()) => return SamePidTransientDetection::Indeterminate,
    };
    let trusted_blender_file_view =
        crate::apps::bundle_id_for_pid(source.pid).as_deref() == Some(BLENDER_BUNDLE_ID);
    detect_same_pid_transient_in_front_in(
        &windows,
        Some(&ax_facts),
        source,
        trusted_blender_file_view,
    )
}

fn has_same_pid_transient_geometry_candidate(windows: &[WindowInfo], source: WindowTarget) -> bool {
    let Some(source_window) = eligible_same_pid_source(windows, source) else {
        return false;
    };
    windows
        .iter()
        .any(|candidate| is_same_pid_transient_geometry_candidate(candidate, source_window))
}

fn same_pid_ax_window_facts(pid: i32) -> Result<Vec<SamePidAxWindowFacts>, ()> {
    const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.2;

    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Err(());
        }
        let _ = AXUIElementSetMessagingTimeout(app, AX_MESSAGING_TIMEOUT_SECONDS);
        let snapshot = match try_copy_ax_windows(app) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                CFRelease(app as CFTypeRef);
                return Err(());
            }
        };
        CFRelease(app as CFTypeRef);

        let mut complete = snapshot.complete;
        let facts = snapshot
            .windows
            .into_iter()
            .enumerate()
            .filter_map(|(order, window)| {
                let _ = AXUIElementSetMessagingTimeout(window, AX_MESSAGING_TIMEOUT_SECONDS);
                let facts = ax_get_window_id(window).map(|window_id| SamePidAxWindowFacts {
                    window_id,
                    order,
                    subrole: copy_string_attr(window, "AXSubrole"),
                    identifier: copy_string_attr(window, "AXIdentifier"),
                    modal: copy_bool_attr(window, "AXModal"),
                    focused: copy_bool_attr(window, "AXFocused"),
                    main: copy_bool_attr(window, "AXMain"),
                });
                if facts.is_none() {
                    complete = false;
                }
                CFRelease(window as CFTypeRef);
                facts
            })
            .collect::<Vec<_>>();
        complete.then_some(facts).ok_or(())
    }
}

fn detect_same_pid_transient_in_front_in(
    windows: &[WindowInfo],
    ax_facts: Option<&[SamePidAxWindowFacts]>,
    source: WindowTarget,
    trusted_blender_file_view: bool,
) -> SamePidTransientDetection {
    let Some(source_window) = eligible_same_pid_source(windows, source) else {
        return SamePidTransientDetection::None;
    };
    let Some(ax_facts) = ax_facts else {
        return SamePidTransientDetection::Indeterminate;
    };
    let Some(source_ax) = ax_facts
        .iter()
        .find(|facts| facts.window_id == source.window_id)
    else {
        return SamePidTransientDetection::Indeterminate;
    };
    let (Some(source_focused), Some(source_main)) = (source_ax.focused, source_ax.main) else {
        return SamePidTransientDetection::Indeterminate;
    };

    let mut matches = Vec::new();
    let mut unproven_focused_successor = false;
    for candidate in windows {
        if !is_same_pid_transient_geometry_candidate(candidate, source_window) {
            continue;
        }
        let Some(candidate_ax) = ax_facts
            .iter()
            .find(|facts| facts.window_id == candidate.window_id)
        else {
            return SamePidTransientDetection::Indeterminate;
        };
        let (Some(candidate_focused), Some(candidate_main)) =
            (candidate_ax.focused, candidate_ax.main)
        else {
            return SamePidTransientDetection::Indeterminate;
        };
        let exclusive_focus_handoff = candidate_ax.order < source_ax.order
            && candidate_focused
            && candidate_main
            && !source_focused
            && !source_main;
        if !exclusive_focus_handoff {
            continue;
        }
        let classification = if candidate_ax.has_dialog_metadata() {
            Some(SamePidTransientClassification::DialogMetadata)
        } else if trusted_blender_file_view && candidate.title == BLENDER_FILE_VIEW_TITLE {
            Some(SamePidTransientClassification::TrustedBlenderFileView)
        } else {
            None
        };
        let Some(classification) = classification else {
            unproven_focused_successor = true;
            continue;
        };
        matches.push(SamePidTransientProof {
            source,
            target: WindowTarget {
                pid: candidate.pid,
                window_id: candidate.window_id,
            },
            classification,
        });
    }

    if unproven_focused_successor || matches.len() > 1 {
        return SamePidTransientDetection::Ambiguous;
    }
    let Some(candidate) = matches.into_iter().next() else {
        return SamePidTransientDetection::None;
    };
    SamePidTransientDetection::Unique(candidate)
}

fn eligible_same_pid_source(windows: &[WindowInfo], source: WindowTarget) -> Option<&WindowInfo> {
    windows.iter().find(|window| {
        window.pid == source.pid
            && window.window_id == source.window_id
            && window.layer == 0
            && window.is_on_screen
            && window.on_current_space == Some(true)
    })
}

fn is_same_pid_transient_geometry_candidate(candidate: &WindowInfo, source: &WindowInfo) -> bool {
    candidate.pid == source.pid
        && candidate.window_id != source.window_id
        && candidate.layer == 0
        && candidate.is_on_screen
        && candidate.on_current_space == Some(true)
        && !candidate.title.trim().is_empty()
        && contained_by(&candidate.bounds, &source.bounds)
        && same_known_space(candidate, source)
}

fn same_known_space(left: &WindowInfo, right: &WindowInfo) -> bool {
    match (left.current_space_id, right.current_space_id) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

pub(crate) fn detect_visible_transient_helper(source: WindowTarget) -> TransientHelperDetection {
    if crate::apps::bundle_id_for_pid(source.pid).as_deref() != Some(SHORTCUTS_HOST_BUNDLE_ID) {
        return TransientHelperDetection::None;
    }
    let windows = crate::windows::all_windows_including_accessory_layers();
    detect_visible_transient_helper_in(&windows, source, |helper_pid| {
        trusted_shortcuts_pair(source.pid, helper_pid)
    })
}

/// Detect any visible trusted transient associated with a host process when a
/// keyboard call omitted `window_id`. Such a call cannot safely inherit a
/// previously observed route, but it must still refuse host fallback while a
/// modal helper is visible.
pub(crate) fn detect_any_visible_transient_helper_for_host(
    host_pid: i32,
) -> TransientHelperDetection {
    if crate::apps::bundle_id_for_pid(host_pid).as_deref() != Some(SHORTCUTS_HOST_BUNDLE_ID) {
        return TransientHelperDetection::None;
    }
    let windows = crate::windows::all_windows_including_accessory_layers();
    detect_any_visible_transient_helper_for_host_in(&windows, host_pid, |helper_pid| {
        trusted_shortcuts_pair(host_pid, helper_pid)
    })
}

/// A narrow WindowServer proof for a helper whose AX window cannot be mapped
/// back to its CGWindowID.  Requiring an active auxiliary process with exactly
/// one visible window avoids treating an arbitrary regular app as the target
/// of a global HID event.
pub(crate) fn active_helper_has_unique_visible_window_for_route(
    route: TransientRoute,
    target: WindowTarget,
) -> bool {
    if route.target != target || resolve_visible_transient_helper(route.source) != Some(target) {
        return false;
    }
    let windows = crate::windows::all_windows_including_accessory_layers();
    active_helper_has_unique_visible_window_in(
        &windows,
        target,
        crate::apps::is_active_auxiliary_application(target.pid),
    )
}

fn active_helper_has_unique_visible_window_in(
    windows: &[WindowInfo],
    target: WindowTarget,
    is_active_auxiliary: bool,
) -> bool {
    if !is_active_auxiliary {
        return false;
    }
    let visible: Vec<_> = windows
        .iter()
        .filter(|window| window.pid == target.pid && window.is_on_screen)
        .collect();
    visible.len() == 1
        && visible[0].window_id == target.window_id
        && visible[0].layer == MODAL_PANEL_WINDOW_LAYER
}

fn detect_visible_transient_helper_in(
    windows: &[WindowInfo],
    source: WindowTarget,
    mut is_trusted_pair: impl FnMut(i32) -> bool,
) -> TransientHelperDetection {
    let Some(host) = windows
        .iter()
        .find(|window| window.pid == source.pid && window.window_id == source.window_id)
    else {
        return TransientHelperDetection::None;
    };
    if !host.is_on_screen || host.layer != 0 || host.app_name.is_empty() || host.title.is_empty() {
        return TransientHelperDetection::None;
    }

    let mut matches = windows.iter().filter(|candidate| {
        candidate.pid != source.pid
            && candidate.pid > 0
            && candidate.window_id != 0
            && candidate.is_on_screen
            && candidate.layer == MODAL_PANEL_WINDOW_LAYER
            && candidate.app_name == host.app_name
            && candidate.title == host.title
            && contained_by(&candidate.bounds, &host.bounds)
            && is_trusted_pair(candidate.pid)
    });
    let Some(candidate) = matches.next() else {
        return TransientHelperDetection::None;
    };
    if matches.next().is_some() {
        return TransientHelperDetection::Ambiguous;
    }
    TransientHelperDetection::Unique(WindowTarget {
        pid: candidate.pid,
        window_id: candidate.window_id,
    })
}

fn detect_any_visible_transient_helper_for_host_in(
    windows: &[WindowInfo],
    host_pid: i32,
    mut is_trusted_pair: impl FnMut(i32) -> bool,
) -> TransientHelperDetection {
    let sources: Vec<_> = windows
        .iter()
        .filter(|window| {
            window.pid == host_pid
                && window.window_id != 0
                && window.is_on_screen
                && window.layer == 0
        })
        .map(|window| WindowTarget {
            pid: host_pid,
            window_id: window.window_id,
        })
        .collect();
    let mut found = None;
    for source in sources {
        match detect_visible_transient_helper_in(windows, source, &mut is_trusted_pair) {
            TransientHelperDetection::None => {}
            TransientHelperDetection::Ambiguous => return TransientHelperDetection::Ambiguous,
            TransientHelperDetection::Unique(target) => match found {
                None => found = Some(target),
                Some(existing) if existing == target => {}
                Some(_) => return TransientHelperDetection::Ambiguous,
            },
        }
    }
    found.map_or(
        TransientHelperDetection::None,
        TransientHelperDetection::Unique,
    )
}

fn trusted_shortcuts_pair(host_pid: i32, helper_pid: i32) -> bool {
    trusted_shortcuts_identity(
        crate::apps::bundle_id_for_pid(host_pid).as_deref(),
        crate::apps::bundle_id_for_pid(helper_pid).as_deref(),
        crate::apps::is_auxiliary_application(helper_pid),
        crate::apps::executable_path_for_pid(helper_pid).as_deref(),
    )
}

pub(crate) fn is_trusted_transient_helper_process(pid: i32) -> bool {
    trusted_shortcuts_helper_identity(
        crate::apps::bundle_id_for_pid(pid).as_deref(),
        crate::apps::is_auxiliary_application(pid),
        crate::apps::executable_path_for_pid(pid).as_deref(),
    )
}

fn trusted_shortcuts_identity(
    host_bundle_id: Option<&str>,
    helper_bundle_id: Option<&str>,
    helper_is_auxiliary: bool,
    helper_executable_path: Option<&str>,
) -> bool {
    host_bundle_id == Some(SHORTCUTS_HOST_BUNDLE_ID)
        && trusted_shortcuts_helper_identity(
            helper_bundle_id,
            helper_is_auxiliary,
            helper_executable_path,
        )
}

fn trusted_shortcuts_helper_identity(
    helper_bundle_id: Option<&str>,
    helper_is_auxiliary: bool,
    helper_executable_path: Option<&str>,
) -> bool {
    helper_bundle_id == Some(SHORTCUTS_HELPER_BUNDLE_ID)
        && helper_is_auxiliary
        && helper_executable_path.is_some_and(trusted_shortcuts_helper_path)
}

fn trusted_shortcuts_helper_path(path: &str) -> bool {
    path == SHORTCUTS_HELPER_SYSTEM_PATH
        || (path.starts_with(CRYPTEX_SYSTEM_PREFIX)
            && path.ends_with(SHORTCUTS_HELPER_SYSTEM_PATH)
            && !std::path::Path::new(path)
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)))
}

fn contained_by(child: &WindowBounds, parent: &WindowBounds) -> bool {
    const TOLERANCE: f64 = 2.0;
    child.width >= 32.0
        && child.height >= 32.0
        && child.width < parent.width
        && child.height < parent.height
        && child.x >= parent.x - TOLERANCE
        && child.y >= parent.y - TOLERANCE
        && child.x + child.width <= parent.x + parent.width + TOLERANCE
        && child.y + child.height <= parent.y + parent.height + TOLERANCE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(
        pid: i32,
        window_id: u32,
        app_name: &str,
        title: &str,
        layer: i32,
        bounds: WindowBounds,
    ) -> WindowInfo {
        WindowInfo {
            window_id,
            pid,
            app_name: app_name.into(),
            title: title.into(),
            bounds,
            layer,
            z_index: window_id as usize,
            is_on_screen: true,
            current_space_id: None,
            on_current_space: None,
            space_ids: None,
        }
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> WindowBounds {
        WindowBounds {
            x,
            y,
            width,
            height,
        }
    }

    fn session(name: &str) -> TransientSessionKey {
        TransientSessionKey::Session(name.to_owned())
    }

    fn same_pid_ax(
        window_id: u32,
        order: usize,
        focused: bool,
        main: bool,
        modal: bool,
    ) -> SamePidAxWindowFacts {
        SamePidAxWindowFacts {
            window_id,
            order,
            subrole: Some("AXStandardWindow".into()),
            identifier: None,
            modal: Some(modal),
            focused: Some(focused),
            main: Some(main),
        }
    }

    fn current_window(
        pid: i32,
        window_id: u32,
        title: &str,
        z_index: usize,
        bounds: WindowBounds,
    ) -> WindowInfo {
        let mut window = window(pid, window_id, "Blender", title, 0, bounds);
        window.z_index = z_index;
        window.current_space_id = Some(1);
        window.on_current_space = Some(true);
        window.space_ids = Some(vec![1]);
        window
    }

    #[test]
    fn detects_only_the_trusted_blender_file_view_fallback() {
        let source = WindowTarget {
            pid: 42,
            window_id: 100,
        };
        let windows = vec![
            current_window(
                42,
                200,
                BLENDER_FILE_VIEW_TITLE,
                // Blender's File View can be visually composited above its
                // host even when CGWindow ordering reports the host first.
                5,
                rect(200.0, 180.0, 600.0, 420.0),
            ),
            current_window(
                42,
                100,
                "arbitrary host title",
                10,
                rect(0.0, 0.0, 1000.0, 800.0),
            ),
        ];
        let ax_facts = vec![
            same_pid_ax(200, 0, true, true, false),
            same_pid_ax(100, 1, false, false, false),
        ];

        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, Some(&ax_facts), source, true),
            SamePidTransientDetection::Unique(SamePidTransientProof {
                source,
                target: WindowTarget {
                    pid: 42,
                    window_id: 200,
                },
                classification: SamePidTransientClassification::TrustedBlenderFileView,
            })
        );
    }

    #[test]
    fn contained_ordinary_same_pid_sibling_is_refused_without_redirect() {
        let source = WindowTarget {
            pid: 42,
            window_id: 100,
        };
        let windows = vec![
            current_window(
                42,
                200,
                "other document",
                20,
                rect(200.0, 180.0, 600.0, 420.0),
            ),
            current_window(42, 100, "host document", 10, rect(0.0, 0.0, 1000.0, 800.0)),
        ];
        let ax_facts = vec![
            same_pid_ax(200, 0, true, true, false),
            same_pid_ax(100, 1, false, false, false),
        ];

        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, Some(&ax_facts), source, true),
            SamePidTransientDetection::Ambiguous,
            "focus and containment alone must never authorize a sibling redirect"
        );
    }

    #[test]
    fn native_dialog_metadata_allows_a_generic_same_pid_redirect() {
        let source = WindowTarget {
            pid: 42,
            window_id: 100,
        };
        let windows = vec![
            current_window(42, 200, "Confirm", 20, rect(200.0, 180.0, 600.0, 420.0)),
            current_window(42, 100, "host", 10, rect(0.0, 0.0, 1000.0, 800.0)),
        ];
        let ax_facts = vec![
            same_pid_ax(200, 0, true, true, true),
            same_pid_ax(100, 1, false, false, false),
        ];

        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, Some(&ax_facts), source, false),
            SamePidTransientDetection::Unique(SamePidTransientProof {
                source,
                target: WindowTarget {
                    pid: 42,
                    window_id: 200,
                },
                classification: SamePidTransientClassification::DialogMetadata,
            })
        );
    }

    #[test]
    fn geometry_candidate_with_unavailable_ax_evidence_is_indeterminate() {
        let source = WindowTarget {
            pid: 42,
            window_id: 100,
        };
        let windows = vec![
            current_window(42, 200, "child", 20, rect(200.0, 180.0, 600.0, 420.0)),
            current_window(42, 100, "host", 10, rect(0.0, 0.0, 1000.0, 800.0)),
        ];

        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, None, source, false),
            SamePidTransientDetection::Indeterminate
        );

        let source_only = vec![same_pid_ax(100, 1, false, false, false)];
        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, Some(&source_only), source, false),
            SamePidTransientDetection::Indeterminate,
            "an unmapped geometry candidate must not be treated as absent"
        );

        let mut unknown_focus = same_pid_ax(200, 0, true, true, true);
        unknown_focus.focused = None;
        let incomplete = vec![unknown_focus, same_pid_ax(100, 1, false, false, false)];
        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, Some(&incomplete), source, false),
            SamePidTransientDetection::Indeterminate,
            "unknown required focus metadata must fail closed"
        );
    }

    #[test]
    fn does_not_redirect_to_an_ordinary_same_pid_sibling() {
        let source = WindowTarget {
            pid: 42,
            window_id: 100,
        };
        let windows = vec![
            current_window(
                42,
                200,
                "other document",
                20,
                rect(1100.0, 0.0, 600.0, 700.0),
            ),
            current_window(42, 100, "host document", 10, rect(0.0, 0.0, 1000.0, 800.0)),
        ];
        let ax_facts = vec![
            same_pid_ax(200, 0, true, true, false),
            same_pid_ax(100, 1, false, false, false),
        ];

        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, Some(&ax_facts), source, false),
            SamePidTransientDetection::None,
            "a focused sibling is not enough without containment"
        );
    }

    #[test]
    fn requires_an_exclusive_focus_handoff_even_for_contained_windows() {
        let source = WindowTarget {
            pid: 42,
            window_id: 100,
        };
        let windows = vec![
            current_window(42, 200, "palette", 20, rect(200.0, 180.0, 600.0, 420.0)),
            current_window(42, 100, "host", 10, rect(0.0, 0.0, 1000.0, 800.0)),
        ];
        let ax_facts = vec![
            same_pid_ax(200, 0, false, false, false),
            same_pid_ax(100, 1, true, true, false),
        ];

        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, Some(&ax_facts), source, false),
            SamePidTransientDetection::None
        );
    }

    #[test]
    fn requires_the_transient_to_precede_the_source_in_ax_window_order() {
        let source = WindowTarget {
            pid: 42,
            window_id: 100,
        };
        let windows = vec![
            current_window(
                42,
                200,
                "contained sibling",
                20,
                rect(200.0, 180.0, 600.0, 420.0),
            ),
            current_window(42, 100, "host", 10, rect(0.0, 0.0, 1000.0, 800.0)),
        ];
        let ax_facts = vec![
            same_pid_ax(100, 0, false, false, false),
            same_pid_ax(200, 1, true, true, false),
        ];

        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, Some(&ax_facts), source, false),
            SamePidTransientDetection::None,
            "focus flags alone do not override the app's AX window order"
        );
    }

    #[test]
    fn multiple_proven_successors_fail_closed_as_ambiguous() {
        let source = WindowTarget {
            pid: 42,
            window_id: 100,
        };
        let windows = vec![
            current_window(42, 200, "first", 30, rect(100.0, 100.0, 700.0, 500.0)),
            current_window(42, 300, "second", 20, rect(200.0, 180.0, 600.0, 420.0)),
            current_window(42, 100, "host", 10, rect(0.0, 0.0, 1000.0, 800.0)),
        ];
        let ax_facts = vec![
            same_pid_ax(200, 0, true, true, true),
            same_pid_ax(300, 1, true, true, true),
            same_pid_ax(100, 2, false, false, false),
        ];

        assert_eq!(
            detect_same_pid_transient_in_front_in(&windows, Some(&ax_facts), source, false),
            SamePidTransientDetection::Ambiguous
        );
    }

    #[test]
    fn session_key_uses_only_runtime_session_id_and_has_explicit_anonymous_variant() {
        assert_eq!(
            TransientSessionKey::from_args(&serde_json::json!({})),
            TransientSessionKey::Anonymous
        );
        assert_eq!(
            TransientSessionKey::from_args(&serde_json::json!({
                "session": "public-label",
                "_session_id": "runtime-session"
            })),
            session("runtime-session")
        );
        assert_eq!(
            TransientSessionKey::from_args(&serde_json::json!({
                "session": "public-label"
            })),
            TransientSessionKey::Anonymous,
            "public labels must not select an authorization namespace"
        );
        assert_eq!(
            TransientSessionKey::from_args(&serde_json::json!({
                "_session_id": "foo "
            })),
            session("foo "),
            "the registry namespace must preserve the core lifecycle id byte-for-byte"
        );
    }

    #[test]
    fn routes_matching_auxiliary_panel_without_listing_helper_app() {
        let source = WindowTarget {
            pid: 85052,
            window_id: 100,
        };
        let windows = vec![
            window(
                85052,
                100,
                "Shortcuts",
                "Demo",
                0,
                rect(0.0, 33.0, 1296.0, 951.0),
            ),
            window(
                12649,
                200,
                "Shortcuts",
                "Demo",
                8,
                rect(694.0, 261.0, 340.0, 111.0),
            ),
        ];

        assert_eq!(
            detect_visible_transient_helper_in(&windows, source, |pid| pid == 12649),
            TransientHelperDetection::Unique(WindowTarget {
                pid: 12649,
                window_id: 200,
            })
        );
    }

    #[test]
    fn rejects_regular_or_unrelated_windows_even_when_they_overlap() {
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let host = window(
            10,
            100,
            "Editor",
            "Document",
            0,
            rect(0.0, 0.0, 1000.0, 800.0),
        );
        let regular = window(
            20,
            200,
            "Editor",
            "Document",
            8,
            rect(200.0, 200.0, 300.0, 120.0),
        );
        let wrong_title = window(
            30,
            300,
            "Editor",
            "Other",
            8,
            rect(200.0, 200.0, 300.0, 120.0),
        );
        let wrong_owner_name = window(
            40,
            400,
            "Other App",
            "Document",
            8,
            rect(200.0, 200.0, 300.0, 120.0),
        );

        let windows = vec![host, regular, wrong_title, wrong_owner_name];
        assert_eq!(
            detect_visible_transient_helper_in(&windows, source, |pid| pid != 20),
            TransientHelperDetection::None
        );
    }

    #[test]
    fn rejects_helper_outside_host_or_on_normal_window_layer() {
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let windows = vec![
            window(
                10,
                100,
                "Editor",
                "Document",
                0,
                rect(0.0, 0.0, 1000.0, 800.0),
            ),
            window(
                20,
                200,
                "Editor",
                "Document",
                8,
                rect(900.0, 700.0, 300.0, 120.0),
            ),
            window(
                30,
                300,
                "Editor",
                "Document",
                0,
                rect(200.0, 200.0, 300.0, 120.0),
            ),
        ];

        assert_eq!(
            detect_visible_transient_helper_in(&windows, source, |_| true),
            TransientHelperDetection::None
        );
    }

    #[test]
    fn rejects_ambiguous_matching_helper_panels() {
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let windows = vec![
            window(
                10,
                100,
                "Editor",
                "Document",
                0,
                rect(0.0, 0.0, 1000.0, 800.0),
            ),
            window(
                20,
                200,
                "Editor",
                "Document",
                MODAL_PANEL_WINDOW_LAYER,
                rect(200.0, 200.0, 300.0, 120.0),
            ),
            window(
                30,
                300,
                "Editor",
                "Document",
                MODAL_PANEL_WINDOW_LAYER,
                rect(250.0, 250.0, 300.0, 120.0),
            ),
        ];

        assert_eq!(
            detect_visible_transient_helper_in(&windows, source, |_| true),
            TransientHelperDetection::Ambiguous,
            "ambiguous helper candidates must fail closed"
        );
    }

    #[test]
    fn only_trusts_the_exact_shortcuts_system_helper_identity_without_requiring_activity() {
        assert!(trusted_shortcuts_identity(
            Some(SHORTCUTS_HOST_BUNDLE_ID),
            Some(SHORTCUTS_HELPER_BUNDLE_ID),
            true,
            Some(SHORTCUTS_HELPER_SYSTEM_PATH),
        ));
        assert!(trusted_shortcuts_identity(
            Some(SHORTCUTS_HOST_BUNDLE_ID),
            Some(SHORTCUTS_HELPER_BUNDLE_ID),
            true,
            Some("/System/Volumes/Preboot/Cryptexes/App/System/Library/PrivateFrameworks/WorkflowKit.framework/XPCServices/ShortcutsViewService.xpc/Contents/MacOS/ShortcutsViewService"),
        ));

        // Activity is intentionally not part of the identity proof. A visible
        // non-Regular helper can be temporarily inactive while another app is
        // frontmost; detection must still surface it so keyboard input fails
        // closed. The boolean below proves only the auxiliary activation
        // policy, while the routed HID bypass separately requires isActive.
        for (host, helper, auxiliary, path) in [
            (
                Some("com.example.shortcuts"),
                Some(SHORTCUTS_HELPER_BUNDLE_ID),
                true,
                Some(SHORTCUTS_HELPER_SYSTEM_PATH),
            ),
            (
                Some(SHORTCUTS_HOST_BUNDLE_ID),
                Some("com.example.ShortcutsViewService"),
                true,
                Some(SHORTCUTS_HELPER_SYSTEM_PATH),
            ),
            (
                Some(SHORTCUTS_HOST_BUNDLE_ID),
                Some(SHORTCUTS_HELPER_BUNDLE_ID),
                false,
                Some(SHORTCUTS_HELPER_SYSTEM_PATH),
            ),
            (
                Some(SHORTCUTS_HOST_BUNDLE_ID),
                Some(SHORTCUTS_HELPER_BUNDLE_ID),
                true,
                Some("/tmp/System/Library/PrivateFrameworks/WorkflowKit.framework/XPCServices/ShortcutsViewService.xpc/Contents/MacOS/ShortcutsViewService"),
            ),
        ] {
            assert!(!trusted_shortcuts_identity(host, helper, auxiliary, path));
        }
    }

    #[test]
    fn host_pid_detection_finds_one_helper_and_refuses_multiple_helpers() {
        let host_a = window(
            10,
            100,
            "Shortcuts",
            "Demo A",
            0,
            rect(0.0, 0.0, 1000.0, 800.0),
        );
        let host_b = window(
            10,
            101,
            "Shortcuts",
            "Demo B",
            0,
            rect(1000.0, 0.0, 1000.0, 800.0),
        );
        let helper_a = window(
            20,
            200,
            "Shortcuts",
            "Demo A",
            MODAL_PANEL_WINDOW_LAYER,
            rect(200.0, 200.0, 300.0, 120.0),
        );
        assert_eq!(
            detect_any_visible_transient_helper_for_host_in(
                &[host_a.clone(), host_b.clone(), helper_a.clone()],
                10,
                |pid| pid == 20,
            ),
            TransientHelperDetection::Unique(WindowTarget {
                pid: 20,
                window_id: 200,
            })
        );

        let helper_b = window(
            30,
            300,
            "Shortcuts",
            "Demo B",
            MODAL_PANEL_WINDOW_LAYER,
            rect(1200.0, 200.0, 300.0, 120.0),
        );
        assert_eq!(
            detect_any_visible_transient_helper_for_host_in(
                &[host_a, host_b, helper_a, helper_b],
                10,
                |pid| matches!(pid, 20 | 30),
            ),
            TransientHelperDetection::Ambiguous
        );
    }

    #[test]
    fn stale_route_is_refused_and_removed_instead_of_falling_back() {
        let registry = TransientUiRegistry::new();
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let target = WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let session = session("stale-route");
        registry.record(&session, source, Some(target));

        assert_eq!(
            registry.resolve_with(&session, source, |_| None),
            RouteResolution::Stale(TransientRoute { source, target })
        );
        assert_eq!(
            registry.resolve_with(&session, source, |_| Some(target)),
            RouteResolution::None,
            "a stale route must be removed so no later call can revive it implicitly"
        );
    }

    #[test]
    fn live_route_requires_the_same_helper_window_observed_before() {
        let registry = TransientUiRegistry::new();
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let target = WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let session = session("live-route");
        registry.record(&session, source, Some(target));

        assert_eq!(
            registry.resolve_with(&session, source, |_| Some(target)),
            RouteResolution::Live(TransientRoute { source, target })
        );
    }

    #[test]
    fn routes_are_isolated_by_session_and_anonymous_is_a_distinct_namespace() {
        let registry = TransientUiRegistry::new();
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let target = WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let session_a = session("session-a");
        let session_b = session("session-b");
        registry.record(&session_a, source, Some(target));

        assert_eq!(
            registry.resolve_with(&session_a, source, |_| Some(target)),
            RouteResolution::Live(TransientRoute { source, target })
        );
        assert_eq!(
            registry.resolve_with(&session_b, source, |_| Some(target)),
            RouteResolution::None
        );
        assert_eq!(
            registry.resolve_with(&TransientSessionKey::Anonymous, source, |_| Some(target)),
            RouteResolution::None
        );
    }

    #[test]
    fn trailing_space_session_is_distinct_and_cleanup_removes_only_exact_id() {
        use std::sync::Arc;

        let registry = Arc::new(TransientUiRegistry::new());
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let target_a = WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let target_b = WindowTarget {
            pid: 30,
            window_id: 300,
        };
        let plain = session("foo");
        let spaced = session("foo ");
        registry.record(&plain, source, Some(target_a));
        registry.record(&spaced, source, Some(target_b));

        let cleanup_registry = registry.clone();
        let _hook = cua_driver_core::session::register_scoped_session_end_hook(move |ended| {
            cleanup_registry.clear_session(ended);
        });
        cua_driver_core::session::fire_session_end("foo");

        assert_eq!(registry.recorded_target(&plain, source), None);
        assert_eq!(
            registry.recorded_target(&spaced, source),
            Some(target_b),
            "session_end cleanup must preserve a byte-distinct session id"
        );
    }

    #[test]
    fn session_end_clears_routes_and_prevents_resurrection() {
        use std::sync::Arc;

        let registry = Arc::new(TransientUiRegistry::new());
        let session_id = "transient-ui-ended-session-V7R4";
        let session = session(session_id);
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let target = WindowTarget {
            pid: 20,
            window_id: 200,
        };
        registry.record(&session, source, Some(target));

        let cleanup_registry = registry.clone();
        let _hook = cua_driver_core::session::register_scoped_session_end_hook(move |ended| {
            cleanup_registry.clear_session(ended);
        });
        cua_driver_core::session::fire_session_end(session_id);

        assert_eq!(
            registry.resolve_with(&session, source, |_| Some(target)),
            RouteResolution::None
        );
        registry.record(&session, source, Some(target));
        assert_eq!(
            registry.resolve_with(&session, source, |_| Some(target)),
            RouteResolution::None,
            "an in-flight observation must not revive an ended session"
        );
    }

    #[test]
    fn stale_revalidation_does_not_remove_a_concurrent_newer_route() {
        let registry = TransientUiRegistry::new();
        let session = session("concurrent-update");
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let old_target = WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let new_target = WindowTarget {
            pid: 30,
            window_id: 300,
        };
        registry.record(&session, source, Some(old_target));

        assert_eq!(
            registry.resolve_with(&session, source, |_| {
                registry.record(&session, source, Some(new_target));
                None
            }),
            RouteResolution::Stale(TransientRoute {
                source,
                target: old_target,
            })
        );
        assert_eq!(
            registry.resolve_with(&session, source, |_| Some(new_target)),
            RouteResolution::Live(TransientRoute {
                source,
                target: new_target,
            }),
            "stale cleanup must compare-and-remove only the value it read"
        );
    }

    #[test]
    fn poisoned_registry_is_cleared_and_recovers_without_panicking() {
        use std::sync::Arc;

        let registry = Arc::new(TransientUiRegistry::new());
        let source = WindowTarget {
            pid: 10,
            window_id: 100,
        };
        let target = WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let session = session("poison-recovery");
        registry.record(&session, source, Some(target));

        let poisoner = registry.clone();
        assert!(std::thread::spawn(move || {
            let _routes = poisoner.inner.lock().unwrap();
            panic!("poison registry for recovery test");
        })
        .join()
        .is_err());

        assert_eq!(
            registry.resolve_with(&session, source, |_| Some(target)),
            RouteResolution::None,
            "poison recovery must clear previously authorized aliases"
        );

        registry.record(&session, source, Some(target));
        assert_eq!(
            registry.resolve_with(&session, source, |_| Some(target)),
            RouteResolution::Live(TransientRoute { source, target }),
            "the registry should accept fresh observations after recovery"
        );
    }

    #[test]
    fn foreground_proof_requires_active_auxiliary_and_one_exact_modal_window() {
        let target = WindowTarget {
            pid: 20,
            window_id: 200,
        };
        let panel = window(
            20,
            200,
            "Editor",
            "Document",
            MODAL_PANEL_WINDOW_LAYER,
            rect(200.0, 200.0, 300.0, 120.0),
        );

        assert!(active_helper_has_unique_visible_window_in(
            std::slice::from_ref(&panel),
            target,
            true
        ));
        assert!(!active_helper_has_unique_visible_window_in(
            std::slice::from_ref(&panel),
            target,
            false
        ));

        let mut sibling = panel.clone();
        sibling.window_id = 201;
        assert!(!active_helper_has_unique_visible_window_in(
            &[panel, sibling],
            target,
            true
        ));
    }
}
