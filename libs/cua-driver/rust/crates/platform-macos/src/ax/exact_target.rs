//! Fresh exact-target acquisition for background input decisions.
//!
//! Gathers the facts [`cua_driver_core::background_input`] needs about one
//! requested `(pid, CGWindowID)` immediately before a background mutation:
//! WindowServer ownership, fresh `AXWindows` membership (mapped through
//! `_AXUIElementGetWindow`), minimized/hidden state, competing same-pid AX
//! top-level keyboard destinations, and addressed-element ancestry. All reads are
//! bounded and fail closed — an unreadable fact never unlocks a route.

use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::background_input::{
    BackgroundTargetFacts, ElementAncestry, WindowServerOwnership,
};
use std::collections::{HashMap, HashSet};

use super::bindings::{
    ax_get_window_id, copy_ax_windows, copy_bool_attr, copy_element_attr, copy_string_attr,
    focused_element_of_pid, kAXErrorSuccess, try_copy_ax_windows, AXUIElementCreateApplication,
    AXUIElementRef, AXUIElementSetMessagingTimeout,
};
use crate::windows::{all_automation_windows, resolve_window_owner, WindowOwner};

/// Bounded `AXParent` ascent used when an element does not expose `AXWindow`.
const MAX_ANCESTRY_DEPTH: usize = 40;

/// Resolve the CGWindowID of the top-level AX window that owns `element`.
///
/// Prefers the element's `AXWindow` attribute and falls back to a bounded
/// `AXParent` walk. `None` means ancestry could not be proven — callers must
/// treat that as "not the requested window", never as a wildcard.
///
/// # Safety
///
/// `element` must be a valid `AXUIElementRef` for the duration of the call.
pub unsafe fn element_window_id(element: AXUIElementRef) -> Option<u32> {
    if let Some(window) = copy_element_attr(element, "AXWindow") {
        let window_id = ax_get_window_id(window);
        CFRelease(window as CFTypeRef);
        if window_id.is_some() {
            return window_id;
        }
    }
    // Fallback: ascend AXParent until a window role, then map it.
    let mut current: AXUIElementRef = element;
    let mut owned = false;
    let mut resolved = None;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        match copy_string_attr(current, "AXRole").as_deref() {
            Some("AXWindow") | Some("AXSheet") => {
                resolved = ax_get_window_id(current);
                break;
            }
            Some("AXApplication") | None => break,
            _ => {}
        }
        let parent = copy_element_attr(current, "AXParent");
        if owned {
            CFRelease(current as CFTypeRef);
        }
        match parent {
            Some(parent) => {
                current = parent;
                owned = true;
            }
            None => return None,
        }
    }
    if owned {
        CFRelease(current as CFTypeRef);
    }
    resolved
}

/// The process's focused AX element, but only when it provably belongs to the
/// requested window. Returns a retained element the caller must release.
///
/// This is the only focused-element reader background window-scoped keyboard
/// paths may use: a PID-global focused element can belong to a sibling window,
/// and sibling state must never address or confirm the requested target.
///
/// # Safety
///
/// Caller must `CFRelease` the returned element.
pub unsafe fn focused_element_in_window(pid: i32, window_id: u32) -> Option<AXUIElementRef> {
    let element = focused_element_of_pid(pid)?;
    if element_window_id(element) == Some(window_id) {
        Some(element)
    } else {
        CFRelease(element as CFTypeRef);
        None
    }
}

/// One fresh `AXWindows` row: the mapped CGWindowID plus its minimized state.
/// `minimized: None` means the attribute could not be read — unknown, not
/// "not minimized".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AxWindowRecord {
    window_id: u32,
    minimized: Option<bool>,
}

/// Fresh evidence about whether one WindowServer row still has an exact AX
/// top-level window. This deliberately does not call `WindowServerOnly`
/// "closed": some applications expose legitimate compositor-only surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AxWindowLifecycleEvidence {
    AxPresent {
        minimized: Option<bool>,
        app_hidden: Option<bool>,
        snapshot_complete: bool,
    },
    WindowServerOnly {
        app_hidden: Option<bool>,
    },
    AxUnavailable {
        app_hidden: Option<bool>,
        query_succeeded: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AxWindowSnapshot {
    records: Vec<AxWindowRecord>,
    complete: bool,
}

/// Hard bound on per-call exact AX window work. Each AX attribute read can use
/// the 200 ms messaging timeout, so an unbounded AXWindows array would otherwise
/// turn an opt-in lifecycle probe into an unbounded worker.
const MAX_LIFECYCLE_AX_WINDOWS: usize = 32;

fn bounded_ax_window_count(total: usize) -> (usize, bool) {
    (
        total.min(MAX_LIFECYCLE_AX_WINDOWS),
        total <= MAX_LIFECYCLE_AX_WINDOWS,
    )
}

fn read_minimized_if_requested<F>(requested: &HashSet<u32>, window_id: u32, read: F) -> Option<bool>
where
    F: FnOnce() -> Option<bool>,
{
    requested.contains(&window_id).then(read).flatten()
}

/// Map the application's fresh `AXWindows` through `_AXUIElementGetWindow`.
/// Windows whose id the SPI cannot resolve are omitted: an unmappable window
/// can never satisfy an exact-target requirement.
unsafe fn ax_window_records(app: AXUIElementRef) -> Vec<AxWindowRecord> {
    copy_ax_windows(app)
        .into_iter()
        .filter_map(|window| {
            let record = ax_get_window_id(window).map(|window_id| AxWindowRecord {
                window_id,
                minimized: copy_bool_attr(window, "AXMinimized"),
            });
            CFRelease(window as CFTypeRef);
            record
        })
        .collect()
}

fn classify_ax_window_lifecycle(
    snapshot: Option<&AxWindowSnapshot>,
    window_id: u32,
    app_hidden: Option<bool>,
) -> AxWindowLifecycleEvidence {
    let Some(snapshot) = snapshot else {
        return AxWindowLifecycleEvidence::AxUnavailable {
            app_hidden,
            query_succeeded: false,
        };
    };
    if let Some(record) = snapshot
        .records
        .iter()
        .find(|record| record.window_id == window_id)
    {
        return AxWindowLifecycleEvidence::AxPresent {
            minimized: record.minimized,
            app_hidden,
            snapshot_complete: snapshot.complete,
        };
    }
    if snapshot.complete {
        AxWindowLifecycleEvidence::WindowServerOnly { app_hidden }
    } else {
        AxWindowLifecycleEvidence::AxUnavailable {
            app_hidden,
            query_succeeded: true,
        }
    }
}

/// Gather one bounded, fresh `AXWindows` membership snapshot for a process and
/// classify the requested CGWindowIDs against it.
///
/// This is intentionally opt-in from `list_windows`: the ordinary enumeration
/// stays a cheap WindowServer read, while teardown/lifecycle consumers can ask
/// for the additional proof needed to distinguish a minimized or off-Space AX
/// window from a stale WindowServer-only row.
pub(crate) fn gather_ax_window_lifecycle_evidence(
    pid: i32,
    window_ids: impl IntoIterator<Item = u32>,
) -> HashMap<u32, AxWindowLifecycleEvidence> {
    const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.2;

    let window_ids: HashSet<u32> = window_ids.into_iter().collect();
    if window_ids.is_empty() {
        return HashMap::new();
    }

    // SAFETY: the application element is created and released here. Every
    // window returned from try_copy_ax_windows is released after its two
    // bounded attributes are read.
    let (records, app_hidden) = unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            (None, None)
        } else {
            if AXUIElementSetMessagingTimeout(app, AX_MESSAGING_TIMEOUT_SECONDS) != kAXErrorSuccess
            {
                CFRelease(app as CFTypeRef);
                return window_ids
                    .into_iter()
                    .map(|window_id| {
                        (
                            window_id,
                            AxWindowLifecycleEvidence::AxUnavailable {
                                app_hidden: None,
                                query_succeeded: false,
                            },
                        )
                    })
                    .collect();
            }
            super::enablement::ensure_chromium_ax_enabled(pid, app);
            let app_hidden = copy_bool_attr(app, "AXHidden");
            let records = try_copy_ax_windows(app).ok().map(|ax_snapshot| {
                let (scan_count, within_limit) = bounded_ax_window_count(ax_snapshot.windows.len());
                let mut records = Vec::with_capacity(scan_count);
                let mut complete = ax_snapshot.complete && within_limit;
                for (index, window) in ax_snapshot.windows.into_iter().enumerate() {
                    if index >= scan_count {
                        CFRelease(window as CFTypeRef);
                        continue;
                    }
                    if AXUIElementSetMessagingTimeout(window, AX_MESSAGING_TIMEOUT_SECONDS)
                        != kAXErrorSuccess
                    {
                        complete = false;
                        CFRelease(window as CFTypeRef);
                        continue;
                    }
                    if let Some(window_id) = ax_get_window_id(window) {
                        records.push(AxWindowRecord {
                            window_id,
                            minimized: read_minimized_if_requested(&window_ids, window_id, || {
                                copy_bool_attr(window, "AXMinimized")
                            }),
                        });
                    } else {
                        complete = false;
                    }
                    CFRelease(window as CFTypeRef);
                }
                AxWindowSnapshot { records, complete }
            });
            CFRelease(app as CFTypeRef);
            (records, app_hidden)
        }
    };

    window_ids
        .into_iter()
        .map(|window_id| {
            (
                window_id,
                classify_ax_window_lifecycle(records.as_ref(), window_id, app_hidden),
            )
        })
        .collect()
}

/// Count independently AX-mapped, non-minimized sibling top-level windows.
///
/// WindowServer may expose several layer-0 compositor surfaces for one native
/// Electron, Tauri, or WebKit window. A raw same-pid CGWindow row is therefore
/// not enough to prove another process-scoped keyboard destination. Requiring a
/// fresh `AXWindows` mapping preserves the fail-closed two-window guard while
/// ignoring render surfaces that cannot independently become the AX key window.
fn count_competing_keyboard_destinations(
    pid: i32,
    target_window_id: u32,
    window_server_rows: impl IntoIterator<Item = (i32, u32)>,
    ax_records: &[AxWindowRecord],
) -> usize {
    window_server_rows
        .into_iter()
        .filter(|(owner_pid, window_id)| {
            *owner_pid == pid
                && *window_id != target_window_id
                && ax_records
                    .iter()
                    .any(|record| record.window_id == *window_id && record.minimized != Some(true))
        })
        .count()
}

/// Gather fresh background-input facts for one `(pid, window_id)` target.
///
/// `element_ptr` is an optional retained `AXUIElementRef` (as `usize`) for an
/// explicitly addressed element; the caller must keep it retained for the
/// duration of this call. Blocking: performs one CGWindowList enumeration and
/// bounded AX reads. Call from a blocking context immediately before deciding.
pub fn gather_background_facts(
    pid: i32,
    window_id: u32,
    element_ptr: Option<usize>,
) -> BackgroundTargetFacts {
    let window_server = match resolve_window_owner(pid, window_id) {
        WindowOwner::SamePid => WindowServerOwnership::SamePid,
        WindowOwner::Unknown => WindowServerOwnership::NotFound,
        WindowOwner::ForeignPid { owner_pid, .. } => {
            WindowServerOwnership::ForeignPid { owner_pid }
        }
    };

    // SAFETY: the application element is created and released here; window
    // elements are released inside ax_window_records; the caller guarantees
    // element_ptr stays retained.
    let (records, app_hidden, element) = unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            (
                Vec::new(),
                None,
                element_ptr.map(|_| ElementAncestry::Unproven),
            )
        } else {
            // Electron/Chromium apps may need per-process-lifetime enablement
            // before their AX windows and subtrees are materialized.
            super::enablement::ensure_chromium_ax_enabled(pid, app);
            let records = ax_window_records(app);
            let app_hidden = copy_bool_attr(app, "AXHidden");
            let element = element_ptr.map(|ptr| match element_window_id(ptr as AXUIElementRef) {
                Some(id) if id == window_id => ElementAncestry::ProvenDescendant,
                Some(_) => ElementAncestry::OutsideTargetWindow,
                None => ElementAncestry::Unproven,
            });
            CFRelease(app as CFTypeRef);
            (records, app_hidden, element)
        }
    };

    let target = records.iter().find(|record| record.window_id == window_id);
    let competing_keyboard_destinations = count_competing_keyboard_destinations(
        pid,
        window_id,
        all_automation_windows()
            .iter()
            .map(|window| (window.pid, window.window_id)),
        &records,
    );

    BackgroundTargetFacts {
        window_server,
        ax_window_present: target.is_some(),
        target_minimized: target.and_then(|record| record.minimized),
        app_hidden,
        competing_keyboard_destinations,
        element: element.unwrap_or(ElementAncestry::NotAddressed),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bounded_ax_window_count, classify_ax_window_lifecycle,
        count_competing_keyboard_destinations, read_minimized_if_requested,
        AxWindowLifecycleEvidence, AxWindowRecord, AxWindowSnapshot, MAX_LIFECYCLE_AX_WINDOWS,
    };
    use std::{cell::Cell, collections::HashSet};

    fn ax_window(window_id: u32, minimized: Option<bool>) -> AxWindowRecord {
        AxWindowRecord {
            window_id,
            minimized,
        }
    }

    #[test]
    fn compositor_surfaces_do_not_create_keyboard_ambiguity() {
        let rows = [(42, 10), (42, 11), (42, 12), (42, 13), (42, 14), (42, 15)];
        let records = [ax_window(10, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records),
            0
        );
    }

    #[test]
    fn independently_mapped_sibling_remains_ambiguous() {
        let rows = [(42, 10), (42, 11)];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records),
            1
        );
    }

    #[test]
    fn minimized_mapped_sibling_is_not_a_keyboard_destination() {
        let rows = [(42, 10), (42, 11)];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(true))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records),
            0
        );
    }

    #[test]
    fn unmapped_window_server_sibling_is_not_a_keyboard_destination() {
        let rows = [(42, 10), (42, 99), (7, 11)];
        let records = [ax_window(10, Some(false)), ax_window(11, Some(false))];

        assert_eq!(
            count_competing_keyboard_destinations(42, 10, rows, &records),
            0
        );
    }

    #[test]
    fn lifecycle_evidence_distinguishes_minimized_ax_window_from_server_only_row() {
        let records = [ax_window(10, Some(true)), ax_window(11, Some(false))];
        let complete = AxWindowSnapshot {
            records: records.to_vec(),
            complete: true,
        };

        assert_eq!(
            classify_ax_window_lifecycle(Some(&complete), 10, Some(false)),
            AxWindowLifecycleEvidence::AxPresent {
                minimized: Some(true),
                app_hidden: Some(false),
                snapshot_complete: true
            }
        );
        assert_eq!(
            classify_ax_window_lifecycle(Some(&complete), 99, Some(false)),
            AxWindowLifecycleEvidence::WindowServerOnly {
                app_hidden: Some(false)
            }
        );
        assert_eq!(
            classify_ax_window_lifecycle(None, 10, Some(false)),
            AxWindowLifecycleEvidence::AxUnavailable {
                app_hidden: Some(false),
                query_succeeded: false
            }
        );
    }

    #[test]
    fn partial_ax_mapping_never_becomes_authoritative_absence() {
        let partial = AxWindowSnapshot {
            records: vec![ax_window(10, Some(false))],
            complete: false,
        };

        assert_eq!(
            classify_ax_window_lifecycle(Some(&partial), 10, Some(false)),
            AxWindowLifecycleEvidence::AxPresent {
                minimized: Some(false),
                app_hidden: Some(false),
                snapshot_complete: false
            }
        );
        assert_eq!(
            classify_ax_window_lifecycle(Some(&partial), 99, Some(false)),
            AxWindowLifecycleEvidence::AxUnavailable {
                app_hidden: Some(false),
                query_succeeded: true
            }
        );
    }

    #[test]
    fn lifecycle_ax_scan_has_a_fixed_upper_bound() {
        assert_eq!(bounded_ax_window_count(0), (0, true));
        assert_eq!(
            bounded_ax_window_count(MAX_LIFECYCLE_AX_WINDOWS),
            (32, true)
        );
        assert_eq!(
            bounded_ax_window_count(MAX_LIFECYCLE_AX_WINDOWS + 1),
            (32, false)
        );
    }

    #[test]
    fn minimized_attribute_is_not_read_for_non_requested_ax_windows() {
        let requested = HashSet::from([10]);
        let called = Cell::new(false);
        let minimized = read_minimized_if_requested(&requested, 11, || {
            called.set(true);
            Some(false)
        });

        assert_eq!(minimized, None);
        assert!(!called.get());
    }
}
