//! Bounded AX classification for system-owned helper windows.
//!
//! Some macOS frameworks inject layer-0 windows into another application's
//! process. They are real CGWindowIDs, but they are not application content
//! and must not participate in implicit "main window" selection.

use core_foundation::base::{CFRelease, CFTypeRef};

use super::bindings::{
    ax_get_window_id, copy_ax_windows, copy_children, copy_string_attr,
    AXUIElementCreateApplication, AXUIElementSetMessagingTimeout,
};

const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.2;
const WINDOW_SHARING_SESSION_BUTTON_TITLE: &str = "WindowSharingSessionButton";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutomationWindowClass {
    Ordinary,
    SystemCaptureIndicator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AxChildSummary {
    role: Option<String>,
    title: Option<String>,
}

fn classify_children(children: &[AxChildSummary]) -> AutomationWindowClass {
    if children.iter().any(|child| {
        child.role.as_deref() == Some("AXButton")
            && child.title.as_deref() == Some(WINDOW_SHARING_SESSION_BUTTON_TITLE)
    }) {
        AutomationWindowClass::SystemCaptureIndicator
    } else {
        AutomationWindowClass::Ordinary
    }
}

/// Classify one known CGWindowID by inspecting only its immediate AX children.
///
/// `None` means AX could not resolve the requested window. Callers must fail
/// open in that case: an unavailable accessibility tree is not evidence that a
/// real application window is synthetic.
pub(crate) fn classify_automation_window(
    pid: i32,
    window_id: u32,
) -> Option<AutomationWindowClass> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let _ = AXUIElementSetMessagingTimeout(app, AX_MESSAGING_TIMEOUT_SECONDS);
        let windows = copy_ax_windows(app);
        CFRelease(app as CFTypeRef);

        let mut target = None;
        for window in windows {
            let _ = AXUIElementSetMessagingTimeout(window, AX_MESSAGING_TIMEOUT_SECONDS);
            if target.is_none() && ax_get_window_id(window) == Some(window_id) {
                target = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }

        let target = target?;
        let children = copy_children(target);
        let summaries = children
            .iter()
            .map(|child| {
                let _ = AXUIElementSetMessagingTimeout(*child, AX_MESSAGING_TIMEOUT_SECONDS);
                AxChildSummary {
                    role: copy_string_attr(*child, "AXRole"),
                    title: copy_string_attr(*child, "AXTitle"),
                }
            })
            .collect::<Vec<_>>();
        for child in children {
            CFRelease(child as CFTypeRef);
        }
        CFRelease(target as CFTypeRef);

        Some(classify_children(&summaries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(role: &str, title: &str) -> AxChildSummary {
        AxChildSummary {
            role: Some(role.into()),
            title: Some(title.into()),
        }
    }

    #[test]
    fn window_sharing_session_button_marks_system_capture_indicator() {
        assert_eq!(
            classify_children(&[child("AXButton", "WindowSharingSessionButton")]),
            AutomationWindowClass::SystemCaptureIndicator
        );
    }

    #[test]
    fn similarly_sized_ordinary_controls_are_not_capture_indicators() {
        assert_eq!(
            classify_children(&[
                child("AXButton", "Share"),
                child("AXStaticText", "WindowSharingSessionButton"),
            ]),
            AutomationWindowClass::Ordinary
        );
    }
}
