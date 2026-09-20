//! Explicit user clicks on a PiP card foreground one exact native window.
//! This is not an agent input fallback and never activates all app windows.

use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use core_foundation::{
    base::{CFRelease, TCFType},
    boolean::CFBoolean,
    string::CFString,
};

use crate::ax::bindings::{self as ax, AXUIElementRef};

const AX_TIMEOUT_SECONDS: f32 = 0.1;
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_AX_WINDOWS: usize = 64;

struct OwnedAx(AXUIElementRef);

impl Drop for OwnedAx {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0.cast()) };
        }
    }
}

impl OwnedAx {
    fn bounded(raw: AXUIElementRef) -> anyhow::Result<Self> {
        let element = Self(raw);
        ensure!(!raw.is_null(), "PiP AX element unavailable");
        ensure!(
            unsafe { ax::AXUIElementSetMessagingTimeout(raw, AX_TIMEOUT_SECONDS) }
                == ax::kAXErrorSuccess,
            "PiP AX timeout could not be set"
        );
        Ok(element)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    RestoreMinimized,
    ActivateLogicalHost,
    MakeExactKey,
    RaiseExactWindow,
    MakeExactMain,
    FocusExactWindow,
}

fn activation_steps(minimized: bool, delegated: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    if minimized {
        steps.push(Step::RestoreMinimized);
    }
    if delegated {
        steps.push(Step::ActivateLogicalHost);
    }
    steps.extend([
        Step::MakeExactKey,
        Step::RaiseExactWindow,
        Step::MakeExactMain,
        Step::FocusExactWindow,
    ]);
    steps
}

fn run_steps(
    steps: &[Step],
    mut check: impl FnMut() -> anyhow::Result<()>,
    mut apply: impl FnMut(Step) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for &step in steps {
        check()?;
        apply(step)?;
    }
    check()
}

fn check_ax_status(step: Step, status: ax::AXError) -> anyhow::Result<()> {
    // Main/Focused are optional window attributes. Their absence is not proof
    // of success: the independent exact focus + z-order check still must pass.
    let optional_unsupported = matches!(step, Step::MakeExactMain | Step::FocusExactWindow)
        && status == ax::kAXErrorAttributeUnsupported;
    ensure!(
        status == ax::kAXErrorSuccess || optional_unsupported,
        "PiP {step:?} returned AXError {status}; activation may already have had an effect"
    );
    Ok(())
}

fn confirmation_matches(
    window_id: u32,
    frontmost: bool,
    focused: Option<u32>,
    visible_front: Option<u32>,
) -> bool {
    frontmost && focused == Some(window_id) && visible_front == Some(window_id)
}

pub(super) fn activate(
    logical_pid: i64,
    source_pid: i64,
    window_id: u64,
    still_authorized: impl Fn() -> bool,
) -> anyhow::Result<()> {
    let app_pid = i32::try_from(logical_pid).context("invalid PiP app pid")?;
    let pid = i32::try_from(source_pid).context("invalid PiP source pid")?;
    let window_id = u32::try_from(window_id).context("invalid PiP window id")?;
    ensure!(
        app_pid > 0 && pid > 0 && window_id > 0,
        "invalid PiP target identity"
    );
    let deadline = Instant::now() + ACTIVATION_TIMEOUT;
    let check = || {
        ensure!(Instant::now() < deadline, "PiP exact activation timed out");
        ensure!(still_authorized(), "PiP click target changed or expired");
        ensure!(
            matches!(
                crate::windows::resolve_window_owner(pid, window_id),
                crate::windows::WindowOwner::SamePid
            ),
            "PiP window closed or changed owner"
        );
        Ok(())
    };
    check()?;
    let app = OwnedAx::bounded(unsafe { ax::AXUIElementCreateApplication(pid) })?;
    let snapshot = unsafe { ax::try_copy_ax_windows(app.0) }
        .map_err(|status| anyhow::anyhow!("PiP AXWindows returned {status}"))?;
    // Adopt every Copy reference before an early return, including incomplete
    // or oversized snapshots. Never substitute the application's main window.
    let windows = snapshot
        .windows
        .into_iter()
        .map(OwnedAx)
        .collect::<Vec<_>>();
    ensure!(
        snapshot.complete && windows.len() <= MAX_AX_WINDOWS,
        "PiP AXWindows incomplete or over its bound"
    );
    let mut target = None;
    for window in &windows {
        check()?;
        ensure!(
            unsafe { ax::AXUIElementSetMessagingTimeout(window.0, AX_TIMEOUT_SECONDS) }
                == ax::kAXErrorSuccess,
            "PiP window timeout could not be set"
        );
        if unsafe { ax::ax_get_window_id(window.0) } == Some(window_id) {
            ensure!(
                target.replace(window).is_none(),
                "PiP AX window identity is ambiguous"
            );
        }
    }
    let target = target.context("PiP exact AX window unavailable")?;
    ensure!(
        unsafe { ax::copy_string_attr(target.0, "AXRole") }.as_deref() == Some("AXWindow"),
        "PiP activation requires an exact AXWindow"
    );
    let check_target = || {
        check()?;
        let mut ax_pid = 0;
        ensure!(
            unsafe { ax::AXUIElementGetPid(target.0, &mut ax_pid) } == ax::kAXErrorSuccess
                && ax_pid == pid
                && unsafe { ax::ax_get_window_id(target.0) } == Some(window_id),
            "PiP exact AX window changed"
        );
        check()
    };
    check_target()?;
    let minimized = unsafe { ax::copy_bool_attr(target.0, "AXMinimized") } == Some(true);
    // The click is a persistent user foreground request. Do not let a previous
    // agent input's deferred restore immediately undo it.
    crate::focus_steal::cancel_deferred_suppression(app_pid);
    crate::focus_steal::cancel_deferred_suppression(pid);
    run_steps(
        &activation_steps(minimized, app_pid != pid),
        check_target,
        |step| {
            let status = match step {
                Step::RestoreMinimized => unsafe {
                    ax::AXUIElementSetAttributeValue(
                        target.0,
                        CFString::new("AXMinimized").as_concrete_TypeRef(),
                        CFBoolean::false_value().as_CFTypeRef(),
                    )
                },
                Step::ActivateLogicalHost => {
                    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
                    let host = unsafe {
                        NSRunningApplication::runningApplicationWithProcessIdentifier(app_pid)
                    }
                    .context("PiP logical host is no longer running")?;
                    ensure!(
                        unsafe {
                            host.activateWithOptions(NSApplicationActivationOptions::empty())
                        },
                        "PiP logical host activation was refused"
                    );
                    return Ok(());
                }
                Step::MakeExactKey => {
                    ensure!(
                        crate::input::skylight::make_exact_window_key(pid, window_id),
                        "PiP exact key-window request was refused"
                    );
                    return Ok(());
                }
                Step::RaiseExactWindow => unsafe { ax::perform_action(target.0, "AXRaise") },
                Step::MakeExactMain => unsafe { ax::set_bool_attr_true(target.0, "AXMain") },
                Step::FocusExactWindow => unsafe { ax::set_bool_attr_true(target.0, "AXFocused") },
            };
            check_ax_status(step, status)
        },
    )?;

    // Native post/AX return codes are not a delivery receipt. Observe only;
    // there is no broad fallback or replay if another window remains focused.
    loop {
        check_target()?;
        let focused = unsafe { ax::copy_element_attr(app.0, "AXFocusedWindow") }
            .map(OwnedAx::bounded)
            .transpose()?
            .and_then(|window| unsafe { ax::ax_get_window_id(window.0) });
        let visible = crate::windows::visible_windows_including_accessory_layers_with_snapshot();
        let visible_front = visible
            .succeeded
            .then(|| {
                visible
                    .windows
                    .iter()
                    .filter(|window| {
                        window.pid == pid
                            && window.is_on_screen
                            && window.on_current_space != Some(false)
                            && window.layer == 0
                            && window.bounds.width > 1.0
                            && window.bounds.height > 1.0
                    })
                    .max_by_key(|window| window.z_index)
                    .map(|window| window.window_id)
            })
            .flatten();
        let frontmost = if app_pid == pid {
            crate::input::skylight::front_process_matches(pid, window_id) == Some(true)
        } else {
            crate::apps::frontmost_pid() == Some(app_pid)
        };
        if confirmation_matches(window_id, frontmost, focused, visible_front) {
            check_target()?;
            return Ok(());
        }
        if Instant::now() + Duration::from_millis(20) >= deadline {
            bail!("PiP exact activation unconfirmed: requested {window_id}, focused {focused:?}, visible front {visible_front:?}, app frontmost {frontmost}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_card_never_activates_the_whole_application() {
        assert_eq!(
            activation_steps(false, false),
            [
                Step::MakeExactKey,
                Step::RaiseExactWindow,
                Step::MakeExactMain,
                Step::FocusExactWindow
            ]
        );
    }

    #[test]
    fn delegated_minimized_card_restores_only_its_bound_window() {
        assert_eq!(
            activation_steps(true, true),
            [
                Step::RestoreMinimized,
                Step::ActivateLogicalHost,
                Step::MakeExactKey,
                Step::RaiseExactWindow,
                Step::MakeExactMain,
                Step::FocusExactWindow
            ]
        );
    }

    #[test]
    fn stale_target_stops_before_next_mutation() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let result = run_steps(
            &activation_steps(false, false),
            || {
                ensure!(calls.get() < 2, "target changed");
                Ok(())
            },
            |_| {
                calls.set(calls.get() + 1);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn ambiguous_native_reply_does_not_replay_or_continue() {
        let mut calls = Vec::new();
        let result = run_steps(
            &activation_steps(false, false),
            || Ok(()),
            |step| {
                calls.push(step);
                check_ax_status(step, ax::kAXErrorCannotComplete)
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, [Step::MakeExactKey]);
    }

    #[test]
    fn attribute_support_is_not_confirmation_and_raise_is_required() {
        assert!(check_ax_status(Step::MakeExactMain, ax::kAXErrorAttributeUnsupported).is_ok());
        assert!(check_ax_status(Step::FocusExactWindow, ax::kAXErrorAttributeUnsupported).is_ok());
        assert!(check_ax_status(Step::RaiseExactWindow, ax::kAXErrorActionUnsupported).is_err());
        assert!(check_ax_status(Step::FocusExactWindow, ax::kAXErrorCannotComplete).is_err());
    }

    #[test]
    fn same_app_sibling_window_is_never_a_success() {
        assert!(confirmation_matches(10, true, Some(10), Some(10)));
        for focused in [None, Some(11)] {
            assert!(!confirmation_matches(10, true, focused, Some(10)));
        }
        for front in [None, Some(11)] {
            assert!(!confirmation_matches(10, true, Some(10), front));
        }
        assert!(!confirmation_matches(10, false, Some(10), Some(10)));
    }
}
