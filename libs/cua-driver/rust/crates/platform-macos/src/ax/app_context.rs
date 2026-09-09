//! Application-context window selection for macOS observations.

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Condvar, Mutex, MutexGuard},
};

use core_foundation::base::{CFEqual, CFRelease, CFTypeRef};
use serde_json::{json, Value};

use super::bindings::{
    ax_get_window_id, copy_children, copy_string_attr, kAXErrorAttributeUnsupported,
    kAXErrorFailure, kAXErrorNoValue, try_copy_ax_windows, try_copy_element_attr,
    AXUIElementCreateApplication, AXUIElementSetMessagingTimeout,
};

const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.2;
const MAX_PANEL_AX_TOP_LEVEL_ELEMENTS: usize = 64;
pub(crate) const OPEN_SAVE_PANEL_HELPER_BUNDLE_ID: &str =
    "com.apple.appkit.xpc.openAndSavePanelService";
const OPEN_SAVE_PANEL_HELPER_SYSTEM_PATH: &str = "/System/Library/Frameworks/AppKit.framework/Versions/C/XPCServices/com.apple.appkit.xpc.openAndSavePanelService.xpc/Contents/MacOS/com.apple.appkit.xpc.openAndSavePanelService";
const CRYPTEX_SYSTEM_PREFIX: &str = "/System/Volumes/Preboot/Cryptexes/";
const DELEGATION_ARG: &str = "_app_context_delegation";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AxWindowEvidence {
    Resolved(u32),
    NotQueried,
    NoValue,
    Unsupported,
    Unmappable,
    Failed(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AxWindowsEvidence {
    Available {
        window_ids: Vec<Option<u32>>,
        complete: bool,
    },
    NotQueried,
    NoValue,
    Unsupported,
    Failed(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppContextSnapshot {
    pub(crate) focused: AxWindowEvidence,
    pub(crate) main: AxWindowEvidence,
    pub(crate) windows: AxWindowsEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppContextSelectionReason {
    FocusedWindow,
    MainWindow,
    AxWindowsLast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AppContextSelection {
    pub(crate) window_id: u32,
    pub(crate) reason: AppContextSelectionReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AppContextTarget {
    pub(crate) pid: i32,
    pub(crate) window_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenSavePanelKind {
    Open,
    Save,
}

impl OpenSavePanelKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Save => "save",
        }
    }

    fn from_identifier(identifier: &str) -> Option<Self> {
        match identifier {
            "open-panel" => Some(Self::Open),
            "save-panel" => Some(Self::Save),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AppContextDelegation {
    pub(crate) host_pid: i32,
    pub(crate) target: AppContextTarget,
    pub(crate) panel_kind: OpenSavePanelKind,
}

impl AppContextSelectionReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::FocusedWindow => "ax_focused_window",
            Self::MainWindow => "ax_main_window",
            Self::AxWindowsLast => "ax_windows_last",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpectedAppIdentity {
    pub(crate) bundle_id: Option<String>,
    pub(crate) app_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningAppIdentity {
    pub(crate) bundle_id: Option<String>,
    pub(crate) app_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppContextSelectionError {
    AxWindowsUnavailable,
    AxWindowsIncomplete,
    AxWindowsEmpty,
    LastAxWindowUnmappable,
}

impl AppContextSelectionError {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::AxWindowsUnavailable => "ax_windows_unavailable",
            Self::AxWindowsIncomplete => "ax_windows_incomplete",
            Self::AxWindowsEmpty => "ax_windows_empty",
            Self::LastAxWindowUnmappable => "ax_windows_last_unmappable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppContextResolveError {
    ProcessNotFound,
    ProcessIdentityMismatch {
        actual: Option<RunningAppIdentity>,
    },
    Selection(AppContextSelectionError),
    WindowServerUnavailable,
    SelectedWindowNotFound {
        window_id: u32,
    },
    SelectedWindowOwnerMismatch {
        window_id: u32,
        owner_pid: i32,
        owner_app_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedAppContext {
    pub(crate) identity: RunningAppIdentity,
    pub(crate) snapshot: AppContextSnapshot,
    pub(crate) selection: AppContextSelection,
    pub(crate) target: AppContextTarget,
    pub(crate) delegation: Option<AppContextDelegation>,
}

pub(crate) fn select_app_context_window(
    snapshot: &AppContextSnapshot,
) -> Result<AppContextSelection, AppContextSelectionError> {
    if let AxWindowEvidence::Resolved(window_id) = snapshot.focused {
        return Ok(AppContextSelection {
            window_id,
            reason: AppContextSelectionReason::FocusedWindow,
        });
    }
    if let AxWindowEvidence::Resolved(window_id) = snapshot.main {
        return Ok(AppContextSelection {
            window_id,
            reason: AppContextSelectionReason::MainWindow,
        });
    }
    match &snapshot.windows {
        AxWindowsEvidence::Available {
            window_ids,
            complete,
        } => {
            if !complete {
                return Err(AppContextSelectionError::AxWindowsIncomplete);
            }
            match window_ids.last() {
                Some(Some(window_id)) => Ok(AppContextSelection {
                    window_id: *window_id,
                    reason: AppContextSelectionReason::AxWindowsLast,
                }),
                Some(None) => Err(AppContextSelectionError::LastAxWindowUnmappable),
                None => Err(AppContextSelectionError::AxWindowsEmpty),
            }
        }
        AxWindowsEvidence::NotQueried
        | AxWindowsEvidence::NoValue
        | AxWindowsEvidence::Unsupported
        | AxWindowsEvidence::Failed(_) => Err(AppContextSelectionError::AxWindowsUnavailable),
    }
}

pub(crate) fn accept_revalidated_app_context(
    before: &ResolvedAppContext,
    after: ResolvedAppContext,
) -> Result<ResolvedAppContext, ResolvedAppContext> {
    if before.target == after.target && before.delegation == after.delegation {
        Ok(after)
    } else {
        Err(after)
    }
}

pub(crate) fn process_identity_matches(
    expected: &ExpectedAppIdentity,
    actual: &RunningAppIdentity,
) -> bool {
    if let Some(expected_bundle_id) = expected.bundle_id.as_deref() {
        return actual.bundle_id.as_deref() == Some(expected_bundle_id);
    }
    if actual.bundle_id.is_some() {
        return false;
    }
    expected
        .app_name
        .as_deref()
        .zip(actual.app_name.as_deref())
        .is_some_and(|(expected, actual)| expected == actual)
}

fn attribute_evidence(app: super::bindings::AXUIElementRef, attribute: &str) -> AxWindowEvidence {
    unsafe {
        match try_copy_element_attr(app, attribute) {
            Ok(Some(window)) => {
                let window_id = ax_get_window_id(window);
                CFRelease(window as CFTypeRef);
                window_id
                    .map(AxWindowEvidence::Resolved)
                    .unwrap_or(AxWindowEvidence::Unmappable)
            }
            Ok(None) => AxWindowEvidence::NoValue,
            Err(error) if error == kAXErrorAttributeUnsupported => AxWindowEvidence::Unsupported,
            Err(error) if error == kAXErrorNoValue => AxWindowEvidence::NoValue,
            Err(error) => AxWindowEvidence::Failed(error),
        }
    }
}

fn windows_evidence(app: super::bindings::AXUIElementRef) -> AxWindowsEvidence {
    unsafe {
        match try_copy_ax_windows(app) {
            Ok(ax_snapshot) => {
                let complete = ax_snapshot.complete;
                let window_ids = ax_snapshot
                    .windows
                    .into_iter()
                    .map(|window| {
                        let window_id = ax_get_window_id(window);
                        CFRelease(window as CFTypeRef);
                        window_id
                    })
                    .collect();
                AxWindowsEvidence::Available {
                    window_ids,
                    complete,
                }
            }
            Err(error) if error == kAXErrorAttributeUnsupported => AxWindowsEvidence::Unsupported,
            Err(error) if error == kAXErrorNoValue => AxWindowsEvidence::NoValue,
            Err(error) => AxWindowsEvidence::Failed(error),
        }
    }
}

pub(crate) fn collect_app_context_snapshot(pid: i32) -> AppContextSnapshot {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return AppContextSnapshot {
                focused: AxWindowEvidence::Failed(kAXErrorFailure),
                main: AxWindowEvidence::Failed(kAXErrorFailure),
                windows: AxWindowsEvidence::Failed(kAXErrorFailure),
            };
        }
        let timeout_error = AXUIElementSetMessagingTimeout(app, AX_MESSAGING_TIMEOUT_SECONDS);
        if timeout_error != 0 {
            CFRelease(app as CFTypeRef);
            return AppContextSnapshot {
                focused: AxWindowEvidence::Failed(timeout_error),
                main: AxWindowEvidence::Failed(timeout_error),
                windows: AxWindowsEvidence::Failed(timeout_error),
            };
        }
        super::enablement::ensure_chromium_ax_enabled(pid, app);
        let focused = attribute_evidence(app, "AXFocusedWindow");
        let mut main = AxWindowEvidence::NotQueried;
        let mut windows = AxWindowsEvidence::NotQueried;
        if !matches!(focused, AxWindowEvidence::Resolved(_)) {
            main = attribute_evidence(app, "AXMainWindow");
            if !matches!(main, AxWindowEvidence::Resolved(_)) {
                windows = windows_evidence(app);
            }
        }
        CFRelease(app as CFTypeRef);
        AppContextSnapshot {
            focused,
            main,
            windows,
        }
    }
}

pub(crate) fn running_app_identity(pid: i32) -> Option<RunningAppIdentity> {
    use objc2_app_kit::NSRunningApplication;

    unsafe {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        (!app.isTerminated()).then(|| RunningAppIdentity {
            bundle_id: app
                .bundleIdentifier()
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty()),
            app_name: app
                .localizedName()
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty()),
        })
    }
}

fn trusted_open_save_panel_path(path: &str) -> bool {
    path == OPEN_SAVE_PANEL_HELPER_SYSTEM_PATH
        || (path.starts_with(CRYPTEX_SYSTEM_PREFIX)
            && path.ends_with(OPEN_SAVE_PANEL_HELPER_SYSTEM_PATH)
            && !std::path::Path::new(path)
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)))
}

fn trusted_open_save_panel_identity(
    bundle_id: Option<&str>,
    auxiliary: bool,
    executable_path: Option<&str>,
    apple_signature_valid: bool,
) -> bool {
    bundle_id == Some(OPEN_SAVE_PANEL_HELPER_BUNDLE_ID)
        && auxiliary
        && apple_signature_valid
        && executable_path.is_some_and(trusted_open_save_panel_path)
}

fn signed_open_save_panel_executable_path(pid: i32) -> Option<String> {
    use core_foundation::url::kCFURLPOSIXPathStyle;
    use security_framework::os::macos::code_signing::{
        Flags, GuestAttributes, SecCode, SecRequirement,
    };

    let mut attributes = GuestAttributes::new();
    attributes.set_pid(pid as libc::pid_t);
    let code = SecCode::copy_guest_with_attribues(None, &attributes, Flags::NONE).ok()?;
    let requirement = SecRequirement::from_str(&format!(
        "anchor apple and identifier \"{OPEN_SAVE_PANEL_HELPER_BUNDLE_ID}\""
    ))
    .ok()?;
    code.check_validity(
        Flags::CHECK_TRUSTED_ANCHORS | Flags::NO_NETWORK_ACCESS,
        &requirement,
    )
    .ok()?;
    let url = code.path(Flags::NONE).ok()?;
    let path = url.get_file_system_path(kCFURLPOSIXPathStyle).to_string();
    trusted_open_save_panel_path(&path).then_some(path)
}

pub(crate) fn looks_like_open_save_panel_process(pid: i32) -> bool {
    crate::apps::bundle_id_for_pid(pid).as_deref() == Some(OPEN_SAVE_PANEL_HELPER_BUNDLE_ID)
        || crate::apps::executable_path_for_pid(pid)
            .as_deref()
            .is_some_and(trusted_open_save_panel_path)
}

fn looks_like_open_save_panel_name(app_name: &str) -> bool {
    matches!(
        app_name,
        "Open and Save Panel Service" | "com.apple.appkit.xpc.openAndSavePanelService"
    )
}

/// Cheap inventory filter for the private AppKit panel host. This deliberately
/// avoids code-signature validation on discovery hot paths; exact delegation
/// still requires the full signature/path/AX proof above. A known helper name
/// fails closed when NSWorkspace identity is temporarily unavailable.
pub(crate) fn hide_open_save_panel_from_inventory(pid: i32, app_name: &str) -> bool {
    looks_like_open_save_panel_name(app_name) || looks_like_open_save_panel_process(pid)
}

pub(crate) fn is_trusted_open_save_panel_process(pid: i32) -> bool {
    let signed_path = signed_open_save_panel_executable_path(pid);
    trusted_open_save_panel_identity(
        crate::apps::bundle_id_for_pid(pid).as_deref(),
        crate::apps::is_auxiliary_application(pid),
        signed_path.as_deref(),
        signed_path.is_some(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelAxCandidate {
    window_id: Option<u32>,
    role: String,
    identifier: Option<String>,
}

fn classify_open_save_panel_ax(
    candidates: &[PanelAxCandidate],
    window_id: u32,
) -> Option<OpenSavePanelKind> {
    let mut matches = candidates.iter().filter_map(|candidate| {
        (candidate.window_id == Some(window_id)
            && matches!(candidate.role.as_str(), "AXWindow" | "AXSheet"))
        .then(|| {
            candidate
                .identifier
                .as_deref()
                .and_then(OpenSavePanelKind::from_identifier)
        })
        .flatten()
    });
    let panel = matches.next()?;
    matches.next().is_none().then_some(panel)
}

fn open_save_panel_ax_kind(pid: i32, window_id: u32) -> Option<OpenSavePanelKind> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        if AXUIElementSetMessagingTimeout(app, AX_MESSAGING_TIMEOUT_SECONDS) != 0 {
            CFRelease(app as CFTypeRef);
            return None;
        }

        let mut top_level = copy_children(app);
        let windows = match try_copy_ax_windows(app) {
            Ok(snapshot) if snapshot.complete => snapshot.windows,
            Ok(snapshot) => {
                for element in top_level {
                    CFRelease(element as CFTypeRef);
                }
                for element in snapshot.windows {
                    CFRelease(element as CFTypeRef);
                }
                CFRelease(app as CFTypeRef);
                return None;
            }
            Err(_) => {
                for element in top_level {
                    CFRelease(element as CFTypeRef);
                }
                CFRelease(app as CFTypeRef);
                return None;
            }
        };
        for window in windows {
            if !top_level
                .iter()
                .any(|&element| CFEqual(element as CFTypeRef, window as CFTypeRef) != 0)
            {
                top_level.push(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }

        if top_level.len() > MAX_PANEL_AX_TOP_LEVEL_ELEMENTS {
            for element in top_level {
                CFRelease(element as CFTypeRef);
            }
            CFRelease(app as CFTypeRef);
            return None;
        }

        let candidates = top_level
            .iter()
            .map(|&element| {
                let _ = AXUIElementSetMessagingTimeout(element, AX_MESSAGING_TIMEOUT_SECONDS);
                let role = copy_string_attr(element, "AXRole").unwrap_or_default();
                PanelAxCandidate {
                    window_id: matches!(role.as_str(), "AXWindow" | "AXSheet")
                        .then(|| ax_get_window_id(element))
                        .flatten(),
                    role,
                    identifier: copy_string_attr(element, "AXIdentifier"),
                }
            })
            .collect::<Vec<_>>();
        for element in top_level {
            CFRelease(element as CFTypeRef);
        }
        CFRelease(app as CFTypeRef);
        classify_open_save_panel_ax(&candidates, window_id)
    }
}

fn trusted_open_save_panel_target(
    host_pid: i32,
    selection: AppContextSelection,
    window: &crate::windows::WindowInfo,
) -> Option<(AppContextTarget, AppContextDelegation)> {
    if selection.reason == AppContextSelectionReason::AxWindowsLast
        || window.pid == host_pid
        || window.pid <= 0
        || window.window_id == 0
        || !window.is_on_screen
        || window.layer != 0
        || window.on_current_space != Some(true)
        || !is_trusted_open_save_panel_process(window.pid)
    {
        return None;
    }
    let panel_kind = open_save_panel_ax_kind(window.pid, window.window_id)?;
    let target = AppContextTarget {
        pid: window.pid,
        window_id: window.window_id,
    };
    Some((
        target,
        AppContextDelegation {
            host_pid,
            target,
            panel_kind,
        },
    ))
}

pub(crate) fn resolve_app_context(
    pid: i32,
    expected: &ExpectedAppIdentity,
) -> Result<ResolvedAppContext, AppContextResolveError> {
    let identity = running_app_identity(pid).ok_or(AppContextResolveError::ProcessNotFound)?;
    if !process_identity_matches(expected, &identity) {
        return Err(AppContextResolveError::ProcessIdentityMismatch {
            actual: Some(identity),
        });
    }
    let snapshot = collect_app_context_snapshot(pid);
    let selection =
        select_app_context_window(&snapshot).map_err(AppContextResolveError::Selection)?;
    let enumeration = crate::windows::all_windows_including_accessory_layers_with_snapshot();
    if !enumeration.succeeded {
        return Err(AppContextResolveError::WindowServerUnavailable);
    }
    match enumeration
        .windows
        .iter()
        .find(|window| window.window_id == selection.window_id)
    {
        None => Err(AppContextResolveError::SelectedWindowNotFound {
            window_id: selection.window_id,
        }),
        Some(window) if window.pid != pid => {
            let Some((target, delegation)) = trusted_open_save_panel_target(pid, selection, window)
            else {
                return Err(AppContextResolveError::SelectedWindowOwnerMismatch {
                    window_id: selection.window_id,
                    owner_pid: window.pid,
                    owner_app_name: window.app_name.clone(),
                });
            };
            Ok(ResolvedAppContext {
                identity,
                snapshot,
                selection,
                target,
                delegation: Some(delegation),
            })
        }
        Some(_) => Ok(ResolvedAppContext {
            identity,
            snapshot,
            selection,
            target: AppContextTarget {
                pid,
                window_id: selection.window_id,
            },
            delegation: None,
        }),
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DelegationRouteKey {
    session: crate::transient_ui::TransientSessionKey,
    target: AppContextTarget,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DelegationHostKey {
    session: crate::transient_ui::TransientSessionKey,
    host_pid: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AppContextDelegationRoute {
    pub(crate) session: crate::transient_ui::TransientSessionKey,
    pub(crate) expected_host_identity: ExpectedAppIdentity,
    pub(crate) delegation: AppContextDelegation,
    pub(crate) generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DelegationRouteResolution {
    None,
    Live(AppContextDelegationRoute),
    Stale(AppContextDelegationRoute),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DelegationObservationTicket {
    session: crate::transient_ui::TransientSessionKey,
    host_pid: i32,
    generation: u64,
}

#[derive(Default)]
struct DelegationRegistryInner {
    next_generation: u64,
    active_host_generations: HashMap<DelegationHostKey, u64>,
    active_observation_targets: HashMap<DelegationHostKey, (u64, AppContextTarget)>,
    routes: HashMap<DelegationRouteKey, AppContextDelegationRoute>,
    host_coordinations: HashMap<DelegationHostKey, Arc<DelegationCoordination>>,
    target_coordinations: HashMap<AppContextTarget, Arc<DelegationCoordination>>,
}

fn prune_unused_target_coordinations(inner: &mut DelegationRegistryInner) {
    let in_use = inner
        .routes
        .keys()
        .map(|key| key.target)
        .chain(
            inner
                .active_observation_targets
                .values()
                .map(|(_, target)| *target),
        )
        .collect::<HashSet<_>>();
    inner.target_coordinations.retain(|target, coordination| {
        in_use.contains(target) || Arc::strong_count(coordination) > 1
    });
}

/// Session-scoped authority created only by a successfully published
/// `window_selection:"app_context"` observation of a trusted Open/Save panel.
/// The public helper pid/window never creates authority by itself.
#[derive(Default)]
pub(crate) struct AppContextDelegationRegistry {
    inner: Mutex<DelegationRegistryInner>,
}

#[derive(Default)]
struct DelegationCoordinationState {
    readers: usize,
    writer: bool,
    waiting_writers: usize,
}

#[derive(Default)]
struct DelegationCoordination {
    state: Mutex<DelegationCoordinationState>,
    changed: Condvar,
}

pub(crate) struct AppContextActionLease {
    // Always acquired in host-then-target order. Keeping both read leases for
    // the complete physical action prevents either the logical host route or
    // the shared AppKit helper window from being rebound under the actuator.
    coordinations: Vec<Arc<DelegationCoordination>>,
}

impl Drop for AppContextActionLease {
    fn drop(&mut self) {
        for coordination in self.coordinations.iter().rev() {
            let mut state = coordination
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.readers = state.readers.saturating_sub(1);
            coordination.changed.notify_all();
        }
    }
}

pub(crate) struct AppContextObservationLease {
    coordination: Arc<DelegationCoordination>,
}

pub(crate) struct AppContextTargetObservationLeases {
    // Sorted exact-target writers. Field order preserves drop order, though
    // releasing locks in either order is safe.
    _leases: Vec<AppContextObservationLease>,
}

impl Drop for AppContextObservationLease {
    fn drop(&mut self) {
        let mut state = self
            .coordination
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.writer = false;
        self.coordination.changed.notify_all();
    }
}

impl DelegationCoordination {
    fn acquire_action(self: &Arc<Self>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while state.writer || state.waiting_writers > 0 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.readers += 1;
    }

    fn acquire_observation(self: &Arc<Self>) -> AppContextObservationLease {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.waiting_writers += 1;
        while state.writer || state.readers > 0 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.waiting_writers = state.waiting_writers.saturating_sub(1);
        state.writer = true;
        AppContextObservationLease {
            coordination: Arc::clone(self),
        }
    }
}

impl AppContextDelegationRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn acquire_action_lease(
        &self,
        session: &crate::transient_ui::TransientSessionKey,
        target: AppContextTarget,
    ) -> Option<AppContextActionLease> {
        let (host_coordination, target_coordination) = {
            let inner = self.lock();
            let route = inner.routes.get(&DelegationRouteKey {
                session: session.clone(),
                target,
            })?;
            let host = inner
                .host_coordinations
                .get(&DelegationHostKey {
                    session: session.clone(),
                    host_pid: route.delegation.host_pid,
                })
                .cloned()?;
            let target = inner.target_coordinations.get(&target).cloned()?;
            (host, target)
        };
        // Every path uses this fixed order. Observations acquire the logical
        // host writer before the physical-target writer, so there is no AB/BA
        // cycle when the shared panel service moves a window between hosts.
        host_coordination.acquire_action();
        target_coordination.acquire_action();
        Some(AppContextActionLease {
            coordinations: vec![host_coordination, target_coordination],
        })
    }

    pub(crate) fn acquire_observation_lease(
        &self,
        session: &crate::transient_ui::TransientSessionKey,
        host_pid: i32,
    ) -> AppContextObservationLease {
        let coordination = {
            let mut inner = self.lock();
            inner
                .host_coordinations
                .entry(DelegationHostKey {
                    session: session.clone(),
                    host_pid,
                })
                .or_default()
                .clone()
        };
        coordination.acquire_observation()
    }

    pub(crate) fn acquire_target_observation_leases(
        &self,
        mut targets: Vec<AppContextTarget>,
    ) -> AppContextTargetObservationLeases {
        targets.sort_by_key(|target| (target.pid, target.window_id));
        targets.dedup();
        let coordinations = {
            let mut inner = self.lock();
            targets
                .into_iter()
                .map(|target| {
                    inner
                        .target_coordinations
                        .entry(target)
                        .or_default()
                        .clone()
                })
                .collect::<Vec<_>>()
        };
        AppContextTargetObservationLeases {
            _leases: coordinations
                .into_iter()
                .map(|coordination| coordination.acquire_observation())
                .collect(),
        }
    }

    pub(crate) fn begin_observation(
        &self,
        session: &crate::transient_ui::TransientSessionKey,
        host_pid: i32,
    ) -> (DelegationObservationTicket, Vec<AppContextTarget>) {
        // Production callers hold `acquire_observation_lease()` across this
        // call, native observation, and commit. Kept synchronous so the pure
        // registry transitions remain directly unit-testable.
        let mut inner = self.lock();
        inner.next_generation = inner.next_generation.wrapping_add(1).max(1);
        let generation = inner.next_generation;
        let host_key = DelegationHostKey {
            session: session.clone(),
            host_pid,
        };
        inner
            .host_coordinations
            .entry(host_key.clone())
            .or_default();
        inner
            .active_host_generations
            .insert(host_key.clone(), generation);
        inner.active_observation_targets.remove(&host_key);
        let mut removed = Vec::new();
        inner.routes.retain(|key, route| {
            let keep = key.session != *session || route.delegation.host_pid != host_pid;
            if !keep {
                removed.push(key.target);
            }
            keep
        });
        prune_unused_target_coordinations(&mut inner);
        let result = (
            DelegationObservationTicket {
                session: session.clone(),
                host_pid,
                generation,
            },
            removed.clone(),
        );
        drop(inner);
        for target in removed {
            crate::pip::invalidate_app_context_target(target);
        }
        result
    }

    /// Bind a host observation generation to the exact physical helper
    /// target after the caller has acquired that target's write lease.
    /// Existing capabilities for the same shared helper window are revoked,
    /// including capabilities owned by a different host or session.
    pub(crate) fn bind_observation_target(
        &self,
        ticket: &DelegationObservationTicket,
        target: AppContextTarget,
    ) -> Option<Vec<AppContextTarget>> {
        let mut inner = self.lock();
        let host_key = DelegationHostKey {
            session: ticket.session.clone(),
            host_pid: ticket.host_pid,
        };
        if session_is_ended(&ticket.session)
            || inner.active_host_generations.get(&host_key) != Some(&ticket.generation)
        {
            return None;
        }
        inner.target_coordinations.entry(target).or_default();
        inner
            .active_observation_targets
            .insert(host_key, (ticket.generation, target));
        let mut removed = Vec::new();
        inner.routes.retain(|key, _| {
            let keep = key.target != target;
            if !keep {
                removed.push(key.target);
            }
            keep
        });
        drop(inner);
        crate::pip::invalidate_app_context_target(target);
        Some(removed)
    }

    pub(crate) fn commit_observation(
        &self,
        ticket: &DelegationObservationTicket,
        expected_host_identity: ExpectedAppIdentity,
        resolved: &ResolvedAppContext,
    ) -> bool {
        let Some(delegation) = resolved.delegation.clone() else {
            return false;
        };
        if delegation.host_pid != ticket.host_pid || session_is_ended(&ticket.session) {
            return false;
        }
        let mut inner = self.lock();
        let host_key = DelegationHostKey {
            session: ticket.session.clone(),
            host_pid: ticket.host_pid,
        };
        if inner.active_host_generations.get(&host_key) != Some(&ticket.generation) {
            return false;
        }
        if inner.active_observation_targets.get(&host_key)
            != Some(&(ticket.generation, delegation.target))
        {
            return false;
        }
        let key = DelegationRouteKey {
            session: ticket.session.clone(),
            target: delegation.target,
        };
        inner.routes.insert(
            key,
            AppContextDelegationRoute {
                session: ticket.session.clone(),
                expected_host_identity,
                delegation,
                generation: ticket.generation,
            },
        );
        true
    }

    pub(crate) fn resolve_live(
        &self,
        session: &crate::transient_ui::TransientSessionKey,
        target: AppContextTarget,
    ) -> DelegationRouteResolution {
        self.resolve_with(session, target, delegation_route_is_live)
    }

    pub(crate) fn revoke_committed_observation(
        &self,
        ticket: &DelegationObservationTicket,
        target: AppContextTarget,
    ) {
        let key = DelegationRouteKey {
            session: ticket.session.clone(),
            target,
        };
        let mut inner = self.lock();
        if inner
            .routes
            .get(&key)
            .is_some_and(|route| route.generation == ticket.generation)
        {
            inner.routes.remove(&key);
            prune_unused_target_coordinations(&mut inner);
            drop(inner);
            crate::pip::invalidate_app_context_target(target);
            return;
        }
        prune_unused_target_coordinations(&mut inner);
    }

    fn resolve_with(
        &self,
        session: &crate::transient_ui::TransientSessionKey,
        target: AppContextTarget,
        validate: impl FnOnce(&AppContextDelegationRoute) -> bool,
    ) -> DelegationRouteResolution {
        let key = DelegationRouteKey {
            session: session.clone(),
            target,
        };
        let recorded = {
            let mut inner = self.lock();
            if session_is_ended(session) {
                inner.routes.remove(&key)
            } else {
                inner.routes.get(&key).cloned()
            }
        };
        let Some(recorded) = recorded else {
            return DelegationRouteResolution::None;
        };
        let live = validate(&recorded);
        let mut inner = self.lock();
        if session_is_ended(session) {
            inner.routes.remove(&key);
            prune_unused_target_coordinations(&mut inner);
            drop(inner);
            crate::pip::invalidate_app_context_target(target);
            return DelegationRouteResolution::Stale(recorded);
        }
        let unchanged = inner
            .routes
            .get(&key)
            .is_some_and(|current| current.generation == recorded.generation);
        if live && unchanged {
            DelegationRouteResolution::Live(recorded)
        } else {
            if unchanged {
                inner.routes.remove(&key);
            }
            prune_unused_target_coordinations(&mut inner);
            drop(inner);
            if unchanged {
                crate::pip::invalidate_app_context_target(target);
            }
            DelegationRouteResolution::Stale(recorded)
        }
    }

    pub(crate) fn clear_session(&self, session: &str) {
        let session = crate::transient_ui::TransientSessionKey::Session(session.to_owned());
        // Session teardown is a writer for every logical host shard. The core
        // lifecycle normally invokes cleanup only after in-flight dispatches
        // finish; these leases additionally make direct/internal calls safe.
        let mut coordinations = {
            let inner = self.lock();
            inner
                .host_coordinations
                .iter()
                .filter(|(key, _)| key.session == session)
                .map(|(key, coordination)| (key.host_pid, Arc::clone(coordination)))
                .collect::<Vec<_>>()
        };
        coordinations.sort_by_key(|(host_pid, _)| *host_pid);
        let _leases = coordinations
            .into_iter()
            .map(|(_, coordination)| coordination.acquire_observation())
            .collect::<Vec<_>>();
        let mut inner = self.lock();
        let removed_targets = inner
            .routes
            .iter()
            .filter(|(key, _)| key.session == session)
            .map(|(key, _)| key.target)
            .collect::<Vec<_>>();
        inner.routes.retain(|key, _| key.session != session);
        inner
            .active_host_generations
            .retain(|key, _| key.session != session);
        inner
            .active_observation_targets
            .retain(|key, _| key.session != session);
        inner
            .host_coordinations
            .retain(|key, _| key.session != session);
        prune_unused_target_coordinations(&mut inner);
        drop(inner);
        for target in removed_targets {
            crate::pip::invalidate_app_context_target(target);
        }
    }

    fn lock(&self) -> MutexGuard<'_, DelegationRegistryInner> {
        match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => {
                let mut inner = poisoned.into_inner();
                inner.routes.clear();
                inner.active_host_generations.clear();
                inner.active_observation_targets.clear();
                inner.host_coordinations.clear();
                inner.target_coordinations.clear();
                self.inner.clear_poison();
                inner
            }
        }
    }
}

/// Re-establish the complete host-to-panel proof immediately before an action.
///
/// The registry is only an observation-time capability.  This check deliberately
/// re-runs app-context selection and host identity validation so a recycled helper
/// pid/window or a different panel cannot inherit that capability during model
/// deliberation or foreground activation.
pub(crate) fn delegation_route_is_live(route: &AppContextDelegationRoute) -> bool {
    if session_is_ended(&route.session) {
        return false;
    }
    resolve_app_context(route.delegation.host_pid, &route.expected_host_identity)
        .ok()
        .is_some_and(|resolved| {
            resolved.target == route.delegation.target
                && resolved.delegation.as_ref() == Some(&route.delegation)
        })
}

pub(crate) fn delegation_route_matches_element_window(
    route: &AppContextDelegationRoute,
    element_window_id: Option<u32>,
) -> bool {
    element_window_id == Some(route.delegation.target.window_id)
}

fn session_is_ended(session: &crate::transient_ui::TransientSessionKey) -> bool {
    match session {
        crate::transient_ui::TransientSessionKey::Anonymous => false,
        crate::transient_ui::TransientSessionKey::Session(session) => {
            cua_driver_core::session::is_session_ended(session)
        }
    }
}

pub(crate) fn inject_delegation_arg(args: &mut Value, route: &AppContextDelegationRoute) {
    args[DELEGATION_ARG] = json!({
        "host_pid": route.delegation.host_pid,
        "target_pid": route.delegation.target.pid,
        "target_window_id": route.delegation.target.window_id,
        "panel_kind": route.delegation.panel_kind.as_str(),
        "expected_bundle_id": route.expected_host_identity.bundle_id,
        "expected_app_name": route.expected_host_identity.app_name,
        "generation": route.generation,
    });
}

pub(crate) fn clear_delegation_arg(args: &mut Value) {
    if let Some(object) = args.as_object_mut() {
        object.remove(DELEGATION_ARG);
    }
}

pub(crate) fn delegation_route_from_args(args: &Value) -> Option<AppContextDelegationRoute> {
    let value = args.get(DELEGATION_ARG)?.as_object()?;
    let host_pid = i32::try_from(value.get("host_pid")?.as_i64()?).ok()?;
    let target_pid = i32::try_from(value.get("target_pid")?.as_i64()?).ok()?;
    let target_window_id = u32::try_from(value.get("target_window_id")?.as_u64()?).ok()?;
    let panel_kind = match value.get("panel_kind")?.as_str()? {
        "open" => OpenSavePanelKind::Open,
        "save" => OpenSavePanelKind::Save,
        _ => return None,
    };
    let optional_string = |key: &str| match value.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Some(Some(value.clone())),
        Some(Value::Null) => Some(None),
        _ => None,
    };
    let expected_host_identity = ExpectedAppIdentity {
        bundle_id: optional_string("expected_bundle_id")?,
        app_name: optional_string("expected_app_name")?,
    };
    let generation = value.get("generation")?.as_u64()?;
    Some(AppContextDelegationRoute {
        session: crate::transient_ui::TransientSessionKey::from_args(args),
        expected_host_identity,
        delegation: AppContextDelegation {
            host_pid,
            target: AppContextTarget {
                pid: target_pid,
                window_id: target_window_id,
            },
            panel_kind,
        },
        generation,
    })
}

impl AxWindowEvidence {
    fn to_json(&self) -> Value {
        match self {
            Self::Resolved(window_id) => json!({"status": "resolved", "window_id": window_id}),
            Self::NotQueried => json!({"status": "not_queried"}),
            Self::NoValue => json!({"status": "no_value"}),
            Self::Unsupported => json!({"status": "unsupported"}),
            Self::Unmappable => json!({"status": "unmappable"}),
            Self::Failed(error) => json!({"status": "failed", "ax_error": error}),
        }
    }
}

impl AxWindowsEvidence {
    fn to_json(&self) -> Value {
        match self {
            Self::Available {
                window_ids,
                complete,
            } => json!({
                "status": "available",
                "complete": complete,
                "window_ids": window_ids,
            }),
            Self::NotQueried => json!({"status": "not_queried", "complete": false}),
            Self::NoValue => json!({"status": "no_value", "complete": false}),
            Self::Unsupported => json!({"status": "unsupported", "complete": false}),
            Self::Failed(error) => {
                json!({"status": "failed", "complete": false, "ax_error": error})
            }
        }
    }
}

impl ResolvedAppContext {
    pub(crate) fn selection_json(&self, stable: bool) -> Value {
        let mut selection = json!({
            "mode": "app_context",
            "stable": stable,
            "reason": self.selection.reason.as_str(),
            "selected_window_id": self.selection.window_id,
            "target_pid": self.target.pid,
            "target_window_id": self.target.window_id,
            "observed_bundle_id": self.identity.bundle_id,
            "observed_app_name": self.identity.app_name,
            "focused_window": self.snapshot.focused.to_json(),
            "main_window": self.snapshot.main.to_json(),
            "ax_windows": self.snapshot.windows.to_json(),
            "observed_identity": {
                "bundle_id": self.identity.bundle_id,
                "app_name": self.identity.app_name,
            }
        });
        if let Some(delegation) = self.delegation.as_ref() {
            selection["delegation"] = json!({
                "kind": "trusted_macos_open_save_panel",
                "stable": stable,
                "host_pid": delegation.host_pid,
                "target_pid": delegation.target.pid,
                "target_window_id": delegation.target.window_id,
                "panel_kind": delegation.panel_kind.as_str(),
                "host_ax_reference": true,
                "helper_identity_verified": true,
                "helper_ax_window_verified": true,
                "helper_bundle_id": OPEN_SAVE_PANEL_HELPER_BUNDLE_ID,
            });
        }
        selection
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available(ids: &[Option<u32>]) -> AxWindowsEvidence {
        AxWindowsEvidence::Available {
            window_ids: ids.to_vec(),
            complete: true,
        }
    }

    #[test]
    fn focused_window_wins_over_main_and_ax_window_order() {
        let selection = select_app_context_window(&AppContextSnapshot {
            focused: AxWindowEvidence::Resolved(30),
            main: AxWindowEvidence::Resolved(20),
            windows: available(&[Some(20), Some(30), Some(10)]),
        })
        .unwrap();

        assert_eq!(selection.window_id, 30);
        assert_eq!(selection.reason, AppContextSelectionReason::FocusedWindow);
    }

    #[test]
    fn successful_short_circuit_evidence_can_mark_lower_priority_sources_unqueried() {
        let context = ResolvedAppContext {
            identity: RunningAppIdentity {
                bundle_id: Some("com.example.editor".into()),
                app_name: Some("Editor".into()),
            },
            snapshot: AppContextSnapshot {
                focused: AxWindowEvidence::Resolved(30),
                main: AxWindowEvidence::NotQueried,
                windows: AxWindowsEvidence::NotQueried,
            },
            selection: AppContextSelection {
                window_id: 30,
                reason: AppContextSelectionReason::FocusedWindow,
            },
            target: AppContextTarget {
                pid: 42,
                window_id: 30,
            },
            delegation: None,
        };

        let selection = context.selection_json(true);
        assert_eq!(selection["focused_window"]["window_id"], 30);
        assert_eq!(selection["main_window"]["status"], "not_queried");
        assert_eq!(selection["ax_windows"]["status"], "not_queried");
    }

    #[test]
    fn main_window_is_used_when_focused_window_has_no_usable_id() {
        for focused in [
            AxWindowEvidence::NoValue,
            AxWindowEvidence::Unsupported,
            AxWindowEvidence::Unmappable,
            AxWindowEvidence::Failed(-25204),
        ] {
            let selection = select_app_context_window(&AppContextSnapshot {
                focused,
                main: AxWindowEvidence::Resolved(20),
                windows: available(&[Some(10)]),
            })
            .unwrap();
            assert_eq!(selection.window_id, 20);
            assert_eq!(selection.reason, AppContextSelectionReason::MainWindow);
        }
    }

    #[test]
    fn last_ax_window_is_the_final_fallback_without_claiming_z_order() {
        let selection = select_app_context_window(&AppContextSnapshot {
            focused: AxWindowEvidence::NoValue,
            main: AxWindowEvidence::NoValue,
            windows: available(&[Some(10), Some(20), Some(30)]),
        })
        .unwrap();

        assert_eq!(selection.window_id, 30);
        assert_eq!(selection.reason, AppContextSelectionReason::AxWindowsLast);

        let selection = select_app_context_window(&AppContextSnapshot {
            focused: AxWindowEvidence::NoValue,
            main: AxWindowEvidence::NoValue,
            windows: available(&[None, Some(30)]),
        })
        .unwrap();
        assert_eq!(selection.window_id, 30);
    }

    #[test]
    fn incomplete_or_unmappable_ax_window_fallback_fails_closed() {
        for windows in [
            AxWindowsEvidence::Available {
                window_ids: vec![Some(10), Some(20)],
                complete: false,
            },
            available(&[Some(10), None]),
            AxWindowsEvidence::NoValue,
            AxWindowsEvidence::Unsupported,
            AxWindowsEvidence::Failed(-25204),
        ] {
            assert!(select_app_context_window(&AppContextSnapshot {
                focused: AxWindowEvidence::NoValue,
                main: AxWindowEvidence::NoValue,
                windows,
            })
            .is_err());
        }
    }

    #[test]
    fn pre_and_post_selection_must_resolve_to_the_same_window() {
        let after = ResolvedAppContext {
            identity: RunningAppIdentity {
                bundle_id: Some("com.example.editor".into()),
                app_name: Some("Editor".into()),
            },
            snapshot: AppContextSnapshot {
                focused: AxWindowEvidence::NoValue,
                main: AxWindowEvidence::Resolved(10),
                windows: available(&[Some(10)]),
            },
            selection: AppContextSelection {
                window_id: 10,
                reason: AppContextSelectionReason::MainWindow,
            },
            target: AppContextTarget {
                pid: 42,
                window_id: 10,
            },
            delegation: None,
        };
        let before = ResolvedAppContext {
            snapshot: AppContextSnapshot {
                focused: AxWindowEvidence::Resolved(10),
                main: AxWindowEvidence::NotQueried,
                windows: AxWindowsEvidence::NotQueried,
            },
            selection: AppContextSelection {
                window_id: 10,
                reason: AppContextSelectionReason::FocusedWindow,
            },
            ..after.clone()
        };
        let accepted = accept_revalidated_app_context(&before, after).unwrap();
        let published = accepted.selection_json(true);
        assert_eq!(published["reason"], "ax_main_window");
        assert_eq!(published["focused_window"]["status"], "no_value");
        assert_eq!(published["main_window"]["window_id"], 10);

        let changed = ResolvedAppContext {
            selection: AppContextSelection {
                window_id: 20,
                reason: AppContextSelectionReason::FocusedWindow,
            },
            target: AppContextTarget {
                pid: 42,
                window_id: 20,
            },
            ..accepted
        };
        let rejected = accept_revalidated_app_context(&before, changed.clone()).unwrap_err();
        assert_eq!(rejected, changed);
    }

    #[test]
    fn successful_selection_contract_names_the_actual_window_and_stability() {
        let context = ResolvedAppContext {
            identity: RunningAppIdentity {
                bundle_id: Some("com.example.editor".into()),
                app_name: Some("Editor".into()),
            },
            snapshot: AppContextSnapshot {
                focused: AxWindowEvidence::Resolved(30),
                main: AxWindowEvidence::Resolved(20),
                windows: available(&[Some(20), Some(30)]),
            },
            selection: AppContextSelection {
                window_id: 30,
                reason: AppContextSelectionReason::FocusedWindow,
            },
            target: AppContextTarget {
                pid: 42,
                window_id: 30,
            },
            delegation: None,
        };
        let selection = context.selection_json(true);
        assert_eq!(selection["mode"], "app_context");
        assert_eq!(selection["stable"], true);
        assert_eq!(selection["selected_window_id"], 30);
        assert_eq!(selection["target_pid"], 42);
        assert_eq!(selection["target_window_id"], 30);
        assert_eq!(selection["observed_bundle_id"], "com.example.editor");
        assert_eq!(selection["reason"], "ax_focused_window");
    }

    #[test]
    fn pid_reuse_by_another_bundle_invalidates_the_cached_app_identity() {
        let expected = ExpectedAppIdentity {
            bundle_id: Some("com.apple.calculator".into()),
            app_name: Some("Calculator".into()),
        };
        assert!(process_identity_matches(
            &expected,
            &RunningAppIdentity {
                bundle_id: Some("com.apple.calculator".into()),
                app_name: Some("Calculator".into()),
            }
        ));
        assert!(!process_identity_matches(
            &expected,
            &RunningAppIdentity {
                bundle_id: Some("com.example.other-app".into()),
                app_name: Some("OtherApp".into()),
            }
        ));
    }

    #[test]
    fn app_name_is_only_a_fallback_when_the_live_bundle_is_unavailable() {
        let expected = ExpectedAppIdentity {
            bundle_id: None,
            app_name: Some("Example".into()),
        };
        assert!(process_identity_matches(
            &expected,
            &RunningAppIdentity {
                bundle_id: None,
                app_name: Some("Example".into()),
            }
        ));
        assert!(!process_identity_matches(
            &ExpectedAppIdentity {
                bundle_id: Some("com.example.unbundled".into()),
                app_name: Some("Example".into()),
            },
            &RunningAppIdentity {
                bundle_id: None,
                app_name: Some("Example".into()),
            }
        ));
        assert!(!process_identity_matches(
            &expected,
            &RunningAppIdentity {
                bundle_id: Some("com.example.other".into()),
                app_name: Some("Example".into()),
            }
        ));
        assert!(!process_identity_matches(
            &ExpectedAppIdentity {
                bundle_id: None,
                app_name: Some("Example".into()),
            },
            &RunningAppIdentity {
                bundle_id: Some("com.example.real-bundle".into()),
                app_name: Some("Example".into()),
            }
        ));
    }

    fn delegated_context(
        host_pid: i32,
        target_pid: i32,
        target_window_id: u32,
        panel_kind: OpenSavePanelKind,
    ) -> ResolvedAppContext {
        let target = AppContextTarget {
            pid: target_pid,
            window_id: target_window_id,
        };
        ResolvedAppContext {
            identity: RunningAppIdentity {
                bundle_id: Some("com.example.editor".into()),
                app_name: Some("Editor".into()),
            },
            snapshot: AppContextSnapshot {
                focused: AxWindowEvidence::Resolved(target_window_id),
                main: AxWindowEvidence::NotQueried,
                windows: AxWindowsEvidence::NotQueried,
            },
            selection: AppContextSelection {
                window_id: target_window_id,
                reason: AppContextSelectionReason::FocusedWindow,
            },
            target,
            delegation: Some(AppContextDelegation {
                host_pid,
                target,
                panel_kind,
            }),
        }
    }

    fn expected_editor() -> ExpectedAppIdentity {
        ExpectedAppIdentity {
            bundle_id: Some("com.example.editor".into()),
            app_name: Some("Editor".into()),
        }
    }

    fn bind_and_commit(
        registry: &AppContextDelegationRegistry,
        ticket: &DelegationObservationTicket,
        context: &ResolvedAppContext,
    ) -> bool {
        let _target_lease = registry.acquire_target_observation_leases(vec![context.target]);
        let Some(_) = registry.bind_observation_target(ticket, context.target) else {
            return false;
        };
        registry.commit_observation(ticket, expected_editor(), context)
    }

    #[test]
    fn trusted_panel_identity_requires_every_independent_proof() {
        assert!(trusted_open_save_panel_identity(
            Some(OPEN_SAVE_PANEL_HELPER_BUNDLE_ID),
            true,
            Some(OPEN_SAVE_PANEL_HELPER_SYSTEM_PATH),
            true,
        ));
        assert!(!trusted_open_save_panel_identity(
            Some("com.example.lookalike"),
            true,
            Some(OPEN_SAVE_PANEL_HELPER_SYSTEM_PATH),
            true,
        ));
        assert!(!trusted_open_save_panel_identity(
            Some(OPEN_SAVE_PANEL_HELPER_BUNDLE_ID),
            false,
            Some(OPEN_SAVE_PANEL_HELPER_SYSTEM_PATH),
            true,
        ));
        assert!(!trusted_open_save_panel_identity(
            Some(OPEN_SAVE_PANEL_HELPER_BUNDLE_ID),
            true,
            Some("/tmp/com.apple.appkit.xpc.openAndSavePanelService"),
            true,
        ));
        assert!(!trusted_open_save_panel_identity(
            Some(OPEN_SAVE_PANEL_HELPER_BUNDLE_ID),
            true,
            Some(OPEN_SAVE_PANEL_HELPER_SYSTEM_PATH),
            false,
        ));
    }

    #[test]
    fn inventory_filter_fails_closed_for_known_panel_service_names() {
        assert!(looks_like_open_save_panel_name(
            "Open and Save Panel Service"
        ));
        assert!(looks_like_open_save_panel_name(
            OPEN_SAVE_PANEL_HELPER_BUNDLE_ID
        ));
        assert!(!looks_like_open_save_panel_name("TextEdit"));
    }

    #[test]
    fn cryptex_path_is_constrained_and_parent_traversal_is_rejected() {
        let valid = format!("{CRYPTEX_SYSTEM_PREFIX}OS{OPEN_SAVE_PANEL_HELPER_SYSTEM_PATH}");
        assert!(trusted_open_save_panel_path(&valid));
        let traversal =
            format!("{CRYPTEX_SYSTEM_PREFIX}OS/../OS{OPEN_SAVE_PANEL_HELPER_SYSTEM_PATH}");
        assert!(!trusted_open_save_panel_path(&traversal));
    }

    #[test]
    fn panel_ax_classifier_requires_one_exact_window_role_and_identifier() {
        let open = PanelAxCandidate {
            window_id: Some(77),
            role: "AXWindow".into(),
            identifier: Some("open-panel".into()),
        };
        assert_eq!(
            classify_open_save_panel_ax(std::slice::from_ref(&open), 77),
            Some(OpenSavePanelKind::Open)
        );
        assert_eq!(
            classify_open_save_panel_ax(
                &[PanelAxCandidate {
                    role: "AXButton".into(),
                    ..open.clone()
                }],
                77,
            ),
            None
        );
        assert_eq!(
            classify_open_save_panel_ax(
                &[PanelAxCandidate {
                    identifier: Some("not-a-panel".into()),
                    ..open.clone()
                }],
                77,
            ),
            None
        );
        assert_eq!(
            classify_open_save_panel_ax(&[open.clone(), open], 77),
            None,
            "ambiguous duplicate evidence must fail closed"
        );
    }

    #[test]
    fn delegated_selection_schema_names_actual_target_and_fixed_evidence() {
        let selection = delegated_context(42, 99, 77, OpenSavePanelKind::Save).selection_json(true);
        assert_eq!(selection["target_pid"], 99);
        assert_eq!(selection["target_window_id"], 77);
        assert_eq!(
            selection["delegation"]["kind"],
            "trusted_macos_open_save_panel"
        );
        assert_eq!(selection["delegation"]["stable"], true);
        assert_eq!(selection["delegation"]["host_pid"], 42);
        assert_eq!(selection["delegation"]["target_pid"], 99);
        assert_eq!(selection["delegation"]["target_window_id"], 77);
        assert_eq!(selection["delegation"]["panel_kind"], "save");
        assert_eq!(selection["delegation"]["host_ax_reference"], true);
        assert_eq!(selection["delegation"]["helper_identity_verified"], true);
        assert_eq!(selection["delegation"]["helper_ax_window_verified"], true);
        assert_eq!(
            selection["delegation"]["helper_bundle_id"],
            OPEN_SAVE_PANEL_HELPER_BUNDLE_ID
        );
    }

    #[test]
    fn delegation_registry_is_session_scoped_and_clear_revokes_only_one_session() {
        let registry = AppContextDelegationRegistry::new();
        let session_a =
            crate::transient_ui::TransientSessionKey::Session("panel-registry-session-a".into());
        let session_b =
            crate::transient_ui::TransientSessionKey::Session("panel-registry-session-b".into());
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let context_b = delegated_context(42, 99, 78, OpenSavePanelKind::Open);
        let (ticket_a, _) = registry.begin_observation(&session_a, 42);
        let (ticket_b, _) = registry.begin_observation(&session_b, 42);
        assert!(bind_and_commit(&registry, &ticket_a, &context));
        assert!(bind_and_commit(&registry, &ticket_b, &context_b));
        assert!(matches!(
            registry.resolve_with(&session_a, context.target, |_| true),
            DelegationRouteResolution::Live(_)
        ));
        assert!(matches!(
            registry.resolve_with(&session_b, context_b.target, |_| true),
            DelegationRouteResolution::Live(_)
        ));

        registry.clear_session("panel-registry-session-a");
        assert_eq!(
            registry.resolve_with(&session_a, context.target, |_| true),
            DelegationRouteResolution::None
        );
        assert!(matches!(
            registry.resolve_with(&session_b, context_b.target, |_| true),
            DelegationRouteResolution::Live(_)
        ));
    }

    #[test]
    fn newer_observation_revokes_old_route_and_old_ticket_cannot_commit() {
        let registry = AppContextDelegationRegistry::new();
        let session =
            crate::transient_ui::TransientSessionKey::Session("panel-registry-generation".into());
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (old_ticket, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &old_ticket, &context));
        let (new_ticket, removed) = registry.begin_observation(&session, 42);
        assert_eq!(removed, vec![context.target]);
        assert!(!registry.commit_observation(&old_ticket, expected_editor(), &context));
        assert!(bind_and_commit(&registry, &new_ticket, &context));
    }

    #[test]
    fn late_observation_failure_can_revoke_only_its_committed_generation() {
        let registry = AppContextDelegationRegistry::new();
        let session =
            crate::transient_ui::TransientSessionKey::Session("panel-registry-late-failure".into());
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (ticket, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &ticket, &context));
        registry.revoke_committed_observation(&ticket, context.target);
        assert_eq!(
            registry.resolve_with(&session, context.target, |_| true),
            DelegationRouteResolution::None
        );
    }

    #[test]
    fn unused_physical_target_coordination_is_pruned_after_route_revocation() {
        let registry = AppContextDelegationRegistry::new();
        let session =
            crate::transient_ui::TransientSessionKey::Session("panel-registry-target-prune".into());
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (ticket, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &ticket, &context));
        assert_eq!(registry.lock().target_coordinations.len(), 1);
        let _ = registry.begin_observation(&session, 42);
        assert!(registry.lock().target_coordinations.is_empty());
    }

    #[test]
    fn failed_live_check_returns_stale_once_then_removes_route() {
        let registry = AppContextDelegationRegistry::new();
        let session =
            crate::transient_ui::TransientSessionKey::Session("panel-registry-stale".into());
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (ticket, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &ticket, &context));
        assert!(matches!(
            registry.resolve_with(&session, context.target, |_| false),
            DelegationRouteResolution::Stale(_)
        ));
        assert_eq!(
            registry.resolve_with(&session, context.target, |_| true),
            DelegationRouteResolution::None
        );
    }

    #[test]
    fn stale_validation_cannot_remove_a_concurrent_newer_route() {
        let registry = AppContextDelegationRegistry::new();
        let session = crate::transient_ui::TransientSessionKey::Session(
            "panel-registry-concurrent-generation".into(),
        );
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (old_ticket, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &old_ticket, &context));

        assert!(matches!(
            registry.resolve_with(&session, context.target, |_| {
                let (new_ticket, _) = registry.begin_observation(&session, 42);
                assert!(bind_and_commit(&registry, &new_ticket, &context));
                false
            }),
            DelegationRouteResolution::Stale(_)
        ));
        assert!(matches!(
            registry.resolve_with(&session, context.target, |_| true),
            DelegationRouteResolution::Live(_)
        ));
    }

    #[test]
    fn ended_session_turns_an_existing_route_stale() {
        let registry = AppContextDelegationRegistry::new();
        let id = format!("panel-registry-ended-{}", std::process::id());
        let session = crate::transient_ui::TransientSessionKey::Session(id.clone());
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (ticket, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &ticket, &context));
        cua_driver_core::session::end_session(&id);
        assert!(matches!(
            registry.resolve_with(&session, context.target, |_| true),
            DelegationRouteResolution::Stale(_)
        ));
    }

    #[test]
    fn internal_delegation_argument_round_trips_and_can_be_stripped() {
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Save);
        let route = AppContextDelegationRoute {
            session: crate::transient_ui::TransientSessionKey::Anonymous,
            expected_host_identity: expected_editor(),
            delegation: context.delegation.expect("delegation"),
            generation: 9,
        };
        let mut args = json!({"pid": 99, "window_id": 77});
        inject_delegation_arg(&mut args, &route);
        assert_eq!(delegation_route_from_args(&args), Some(route));
        clear_delegation_arg(&mut args);
        assert!(delegation_route_from_args(&args).is_none());
    }

    #[test]
    fn delegated_element_must_belong_to_the_exact_panel_window() {
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let route = AppContextDelegationRoute {
            session: crate::transient_ui::TransientSessionKey::Anonymous,
            expected_host_identity: expected_editor(),
            delegation: context.delegation.unwrap(),
            generation: 1,
        };
        assert!(delegation_route_matches_element_window(&route, Some(77)));
        assert!(!delegation_route_matches_element_window(&route, Some(78)));
        assert!(!delegation_route_matches_element_window(&route, None));
    }

    #[test]
    fn observation_writer_waits_for_an_inflight_action_lease() {
        let registry = Arc::new(AppContextDelegationRegistry::new());
        let session = crate::transient_ui::TransientSessionKey::Session(
            "panel-registry-observe-lease".into(),
        );
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (ticket, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &ticket, &context));
        let action = registry
            .acquire_action_lease(&session, context.target)
            .expect("live route action lease");
        let (tx, rx) = std::sync::mpsc::channel();
        let observer = Arc::clone(&registry);
        let observer_session = session.clone();
        let thread = std::thread::spawn(move || {
            let _observation = observer.acquire_observation_lease(&observer_session, 42);
            tx.send(()).unwrap();
        });

        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err());
        drop(action);
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .expect("observation should proceed after action finishes");
        thread.join().unwrap();
    }

    #[test]
    fn session_teardown_waits_for_inflight_action_then_revokes_route() {
        let registry = Arc::new(AppContextDelegationRegistry::new());
        let session = crate::transient_ui::TransientSessionKey::Session(
            "panel-registry-teardown-lease".into(),
        );
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (ticket, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &ticket, &context));

        let action = registry
            .acquire_action_lease(&session, context.target)
            .expect("live route action lease");
        let cleanup = Arc::clone(&registry);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            cleanup.clear_session("panel-registry-teardown-lease");
            tx.send(()).unwrap();
        });
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err());
        drop(action);
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .expect("session cleanup should proceed after action finishes");
        thread.join().unwrap();
        assert_eq!(
            registry.resolve_with(&session, context.target, |_| true),
            DelegationRouteResolution::None
        );
    }

    #[test]
    fn unrelated_session_and_host_observation_does_not_wait_on_action_lease() {
        let registry = Arc::new(AppContextDelegationRegistry::new());
        let session_a =
            crate::transient_ui::TransientSessionKey::Session("panel-registry-shard-a".into());
        let session_b =
            crate::transient_ui::TransientSessionKey::Session("panel-registry-shard-b".into());
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (ticket, _) = registry.begin_observation(&session_a, 42);
        assert!(bind_and_commit(&registry, &ticket, &context));
        let _action = registry
            .acquire_action_lease(&session_a, context.target)
            .expect("live route action lease");

        let (tx, rx) = std::sync::mpsc::channel();
        let observer = Arc::clone(&registry);
        let thread = std::thread::spawn(move || {
            let _observation = observer.acquire_observation_lease(&session_b, 43);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .expect("unrelated host/session shard should remain concurrent");
        thread.join().unwrap();
    }

    #[test]
    fn shared_physical_target_rebind_waits_for_inflight_action_then_revokes_old_host() {
        let registry = Arc::new(AppContextDelegationRegistry::new());
        let session = crate::transient_ui::TransientSessionKey::Session(
            "panel-registry-shared-target".into(),
        );
        let context_a = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let context_b = delegated_context(43, 99, 77, OpenSavePanelKind::Open);
        let (ticket_a, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &ticket_a, &context_a));
        let action = registry
            .acquire_action_lease(&session, context_a.target)
            .expect("host A action lease");

        let observer = Arc::clone(&registry);
        let observer_session = session.clone();
        let (host_tx, host_rx) = std::sync::mpsc::channel();
        let (target_tx, target_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _host = observer.acquire_observation_lease(&observer_session, 43);
            let (ticket_b, _) = observer.begin_observation(&observer_session, 43);
            host_tx.send(()).unwrap();
            let _target = observer.acquire_target_observation_leases(vec![context_b.target]);
            target_tx.send(()).unwrap();
            assert!(observer
                .bind_observation_target(&ticket_b, context_b.target)
                .is_some());
            assert!(observer.commit_observation(&ticket_b, expected_editor(), &context_b));
        });

        host_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("different host shard should be acquired");
        assert!(target_rx
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err());
        drop(action);
        target_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("target rebind should proceed after action finishes");
        thread.join().unwrap();

        assert!(matches!(
            registry.resolve_with(&session, context_a.target, |_| true),
            DelegationRouteResolution::Live(route)
                if route.delegation.host_pid == 43
        ));
    }

    #[test]
    fn unrelated_physical_target_observation_remains_concurrent() {
        let registry = Arc::new(AppContextDelegationRegistry::new());
        let session = crate::transient_ui::TransientSessionKey::Session(
            "panel-registry-distinct-target".into(),
        );
        let context = delegated_context(42, 99, 77, OpenSavePanelKind::Open);
        let (ticket, _) = registry.begin_observation(&session, 42);
        assert!(bind_and_commit(&registry, &ticket, &context));
        let _action = registry
            .acquire_action_lease(&session, context.target)
            .expect("host A action lease");

        let observer = Arc::clone(&registry);
        let observer_session = session.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _host = observer.acquire_observation_lease(&observer_session, 43);
            let _target = observer.acquire_target_observation_leases(vec![AppContextTarget {
                pid: 99,
                window_id: 78,
            }]);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .expect("different host and physical target should remain concurrent");
        thread.join().unwrap();
    }
}
