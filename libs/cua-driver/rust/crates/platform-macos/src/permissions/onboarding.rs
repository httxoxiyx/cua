//! Private, app-owned macOS permission onboarding state.
//!
//! This module deliberately contains no MCP tool or public CLI command. The
//! packaged driver invokes it through a private LaunchServices entrypoint so
//! macOS attributes every prompt and live ScreenCaptureKit probe to the exact
//! installed app identity.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::permission_observation::current_driver_direct_capture_evidence_store;

use super::status::PermissionsStatus;

/// Private finite launcher consumed by trusted packaging/bootstrap code.
pub const ONBOARDING_LAUNCH_ARG: &str = "__permissions-onboarding";

/// Side-effect-free private compatibility probe used by release assembly.
pub const ONBOARDING_CONTRACT_ARG: &str = "__permissions-onboarding-contract";

/// Private app-host entrypoint. Only a fresh LaunchServices app instance may
/// execute this route.
pub const ONBOARDING_HOST_ARG: &str = "__permissions-onboarding-host";

/// Prefix used for the private, pre-created status file exchanged between the
/// launcher process and the LaunchServices-hosted app process.
pub const ONBOARDING_STATUS_FILE_PREFIX: &str = "cua-driver-onboarding-";
pub const ONBOARDING_LIVENESS_FILE_PREFIX: &str = "cua-driver-onboarding-live-";
pub const ONBOARDING_DIAGNOSTIC_FILE_PREFIX: &str = "cua-driver-onboarding-log-";
pub const ONBOARDING_HOST_PID_FILE_PREFIX: &str = "cua-driver-onboarding-host-";
pub const ONBOARDING_SERIALIZATION_FILE_PREFIX: &str = "cua-driver-onboarding-serial-";

pub const ONBOARDING_SCHEMA_VERSION: u8 = 1;
pub const ONBOARDING_POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const ONBOARDING_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// A TCC grant can terminate the app process that requested it. Keep the
/// trusted launcher alive and give LaunchServices a bounded number of fresh
/// app-host generations to observe the new grant. Three generations cover the
/// deliberate sequence and three additional generations provide a bounded
/// recovery budget for macOS-driven quits and delayed TCC propagation.
pub const ONBOARDING_MAX_HOST_GENERATIONS: u8 = 6;
pub const ONBOARDING_EXPECTED_HOST_GENERATIONS: u8 = 3;
pub const ONBOARDING_HOST_RESTART_DELAY: Duration = Duration::from_secs(1);
/// Avoid immediately raising the same prompt again while tccd publishes a
/// grant to a newly launched process.
pub const ONBOARDING_TCC_PROPAGATION_GRACE: Duration = Duration::from_secs(5);
/// Private nonterminal host exit consumed only by the supervising launcher.
pub const ONBOARDING_RESTART_REQUIRED_EXIT_CODE: i32 = 75;
pub const CAPTURE_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(10);
pub const SETUP_PENDING_CODE: &str = "computer_use_setup_pending";
pub const SETUP_READY_CODE: &str = "computer_use_setup_ready";
pub const SETUP_FAILED_CODE: &str = "computer_use_setup_failed";
/// Stable terminal error emitted when a successful live probe cannot be
/// durably bound to the current installed driver identity.
pub const DIRECT_CAPTURE_VERIFICATION_STORE_FAILED_CODE: &str =
    "direct_capture_verification_store_failed";
const DIRECT_CAPTURE_VERIFICATION_STORE_FAILED_MESSAGE: &str =
    "could not persist direct-capture verification evidence for this driver identity";

/// Static compatibility contract. Reading it never probes TCC, launches an
/// app, opens System Settings, or initializes ScreenCaptureKit.
pub fn contract() -> serde_json::Value {
    serde_json::json!({
        "schema_version": ONBOARDING_SCHEMA_VERSION,
        "entrypoint": ONBOARDING_LAUNCH_ARG,
        "codes": {
            "pending": SETUP_PENDING_CODE,
            "ready": SETUP_READY_CODE,
            "failed": SETUP_FAILED_CODE,
        },
        "stages": {
            "pending": [
                "accessibility",
                "screen_recording_registration",
                "screen_recording",
                "driver_restarting",
                "tcc_propagation",
                "capture_verification"
            ],
            "requires_user_action": {
                "accessibility": true,
                "screen_recording_registration": false,
                "screen_recording": true,
                "driver_restarting": false,
                "tcc_propagation": false,
                "capture_verification": false
            },
            "ready": "ready",
            "failed": "failed",
        },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureVerificationOutcome {
    Ready,
    Unavailable,
    TimedOut,
    Cancelled,
    Failed(String),
}

/// Verify that the app identity can capture a real pixel with
/// `SCScreenshotManager`.
///
/// The onboarding state machine calls this only after both live TCC probes
/// report granted. It must not be used to request or register Screen Recording
/// because doing so can race the preceding Accessibility grant.
///
/// This is stronger than both `CGPreflightScreenCaptureAccess`, whose negative
/// and positive results can be stale within a long-lived process, and
/// `SCShareableContent::get()`, which can return display metadata without
/// exercising the permission-gated capture operation.
pub fn verify_live_screen_capture(timeout: Duration) -> CaptureVerificationOutcome {
    verify_live_screen_capture_with_cancel(timeout, || false)
}

/// Run the live probe while periodically checking whether its trusted launcher
/// still exists. A canceled setup must not leave a permission host waiting on
/// ScreenCaptureKit after the MCP session has closed.
pub fn verify_live_screen_capture_with_cancel(
    timeout: Duration,
    mut cancelled: impl FnMut() -> bool,
) -> CaptureVerificationOutcome {
    if cancelled() {
        return CaptureVerificationOutcome::Cancelled;
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let _worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            use screencapturekit::prelude::{
                SCContentFilter, SCShareableContent, SCStreamConfiguration,
            };
            use screencapturekit::screenshot_manager::SCScreenshotManager;

            let content = SCShareableContent::get().map_err(|error| error.to_string())?;
            let Some(display) = content.displays().into_iter().next() else {
                return Ok(false);
            };
            let filter = SCContentFilter::create()
                .with_display(&display)
                .with_excluding_windows(&[])
                .build();
            // A single output pixel is sufficient to cross the real capture
            // boundary while keeping onboarding work and retained data tiny.
            let config = SCStreamConfiguration::new()
                .with_width(1)
                .with_height(1)
                .with_scales_to_fit(true);
            SCScreenshotManager::capture_image(&filter, &config)
                .map(|image| image.width() > 0 && image.height() > 0)
                .map_err(|error| error.to_string())
        })
        .unwrap_or_else(|_| Err("ScreenCaptureKit verification panicked".to_owned()));
        let _ = tx.send(result);
    });

    let started = std::time::Instant::now();
    loop {
        if cancelled() {
            return CaptureVerificationOutcome::Cancelled;
        }
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            return CaptureVerificationOutcome::TimedOut;
        };
        let wait = remaining.min(Duration::from_millis(100));
        // The onboarding host owns the AppKit main thread. Keep its run loop
        // moving while ScreenCaptureKit performs its asynchronous request so
        // macOS can present and service consent UI. Off-main callers retain
        // the old sleep behavior through this helper's safe fallback.
        super::panel::pump_run_loop_briefly(wait.as_secs_f64());
        match rx.try_recv() {
            Ok(Ok(true)) => return CaptureVerificationOutcome::Ready,
            Ok(Ok(false)) => return CaptureVerificationOutcome::Unavailable,
            Ok(Err(error)) => return CaptureVerificationOutcome::Failed(error),
            Err(std::sync::mpsc::TryRecvError::Empty) => continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return CaptureVerificationOutcome::Failed(
                    "ScreenCaptureKit verification worker disconnected".to_owned(),
                )
            }
        }
    }
}

/// Persist a successful live ScreenCaptureKit probe for the current validated
/// app bundle. The evidence namespace is derived from the executable's real
/// Info.plist identity, using the same store as `check_permissions`.
pub fn persist_live_screen_capture_verification() -> Result<(), String> {
    current_driver_direct_capture_evidence_store()?
        .refresh_now()
        .map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingStage {
    Accessibility,
    ScreenRecordingRegistration,
    ScreenRecording,
    DriverRestarting,
    TccPropagation,
    CaptureVerification,
    Ready,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingError {
    pub code: String,
    pub message: String,
}

/// Stable JSONL payload emitted by the private onboarding launcher.
///
/// Permission booleans always come from live probes. The ScreenCaptureKit
/// result remains `null` until the dedicated capture-verification stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingStatus {
    pub schema_version: u8,
    pub code: String,
    pub stage: OnboardingStage,
    pub retryable: bool,
    pub requires_user_action: bool,
    pub accessibility: bool,
    pub screen_recording: bool,
    pub screen_recording_capturable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OnboardingError>,
}

impl OnboardingStatus {
    pub fn pending(stage: OnboardingStage, permissions: PermissionsStatus) -> Self {
        debug_assert!(matches!(
            stage,
            OnboardingStage::Accessibility
                | OnboardingStage::ScreenRecordingRegistration
                | OnboardingStage::ScreenRecording
                | OnboardingStage::DriverRestarting
                | OnboardingStage::TccPropagation
                | OnboardingStage::CaptureVerification
        ));
        Self {
            schema_version: ONBOARDING_SCHEMA_VERSION,
            code: SETUP_PENDING_CODE.to_owned(),
            stage,
            retryable: true,
            requires_user_action: matches!(
                stage,
                OnboardingStage::Accessibility | OnboardingStage::ScreenRecording
            ),
            accessibility: permissions.accessibility,
            screen_recording: permissions.screen_recording,
            screen_recording_capturable: None,
            error: None,
        }
    }

    pub fn ready(permissions: PermissionsStatus) -> Option<Self> {
        if !permissions.all_granted() {
            return None;
        }
        Some(Self {
            schema_version: ONBOARDING_SCHEMA_VERSION,
            code: SETUP_READY_CODE.to_owned(),
            stage: OnboardingStage::Ready,
            retryable: false,
            requires_user_action: false,
            accessibility: permissions.accessibility,
            screen_recording: permissions.screen_recording,
            screen_recording_capturable: Some(true),
            error: None,
        })
    }

    pub fn failed(
        permissions: PermissionsStatus,
        capture_result: Option<bool>,
        retryable: bool,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: ONBOARDING_SCHEMA_VERSION,
            code: SETUP_FAILED_CODE.to_owned(),
            stage: OnboardingStage::Failed,
            retryable,
            requires_user_action: false,
            accessibility: permissions.accessibility,
            screen_recording: permissions.screen_recording,
            screen_recording_capturable: capture_result,
            error: Some(OnboardingError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    pub fn capture_verification_store_failed(permissions: PermissionsStatus) -> Self {
        Self::failed(
            permissions,
            Some(true),
            false,
            DIRECT_CAPTURE_VERIFICATION_STORE_FAILED_CODE,
            DIRECT_CAPTURE_VERIFICATION_STORE_FAILED_MESSAGE,
        )
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.stage, OnboardingStage::Ready | OnboardingStage::Failed)
    }
}

/// Select the next required stage. Accessibility is intentionally first even
/// when both grants are absent, so the host never opens two Settings panes or
/// raises both system requests at once.
pub fn next_permission_stage(status: PermissionsStatus) -> Option<OnboardingStage> {
    if !status.accessibility {
        Some(OnboardingStage::Accessibility)
    } else if !status.screen_recording {
        Some(OnboardingStage::ScreenRecording)
    } else {
        None
    }
}

/// Probe TCC from a fresh copy of the signed executable so the onboarding
/// host does not inherit stale process-local preflight state.
pub fn fresh_status() -> PermissionsStatus {
    super::gate::fresh_status()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permissions(accessibility: bool, screen_recording: bool) -> PermissionsStatus {
        PermissionsStatus {
            accessibility,
            screen_recording,
        }
    }

    #[test]
    fn onboarding_always_orders_accessibility_before_screen_recording() {
        assert_eq!(
            next_permission_stage(permissions(false, false)),
            Some(OnboardingStage::Accessibility)
        );
        assert_eq!(
            next_permission_stage(permissions(false, true)),
            Some(OnboardingStage::Accessibility)
        );
        assert_eq!(
            next_permission_stage(permissions(true, false)),
            Some(OnboardingStage::ScreenRecording)
        );
        assert_eq!(next_permission_stage(permissions(true, true)), None);
    }

    #[test]
    fn pending_status_has_stable_bootstrap_shape() {
        let value = serde_json::to_value(OnboardingStatus::pending(
            OnboardingStage::ScreenRecording,
            permissions(true, false),
        ))
        .unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["code"], "computer_use_setup_pending");
        assert_eq!(value["stage"], "screen_recording");
        assert_eq!(value["retryable"], true);
        assert_eq!(value["requires_user_action"], true);
        assert_eq!(value["accessibility"], true);
        assert_eq!(value["screen_recording"], false);
        assert!(value["screen_recording_capturable"].is_null());
        assert!(value.get("error").is_none());

        let registration = serde_json::to_value(OnboardingStatus::pending(
            OnboardingStage::ScreenRecordingRegistration,
            permissions(true, false),
        ))
        .unwrap();
        assert_eq!(registration["stage"], "screen_recording_registration");
        assert_eq!(registration["requires_user_action"], false);
        assert_eq!(registration["accessibility"], true);
        assert_eq!(registration["screen_recording"], false);
    }

    #[test]
    fn contract_probe_describes_the_complete_static_protocol() {
        assert_eq!(
            contract(),
            serde_json::json!({
                "schema_version": 1,
                "entrypoint": "__permissions-onboarding",
                "codes": {
                    "pending": "computer_use_setup_pending",
                    "ready": "computer_use_setup_ready",
                    "failed": "computer_use_setup_failed",
                },
                "stages": {
                    "pending": [
                        "accessibility",
                        "screen_recording_registration",
                        "screen_recording",
                        "driver_restarting",
                        "tcc_propagation",
                        "capture_verification"
                    ],
                    "requires_user_action": {
                        "accessibility": true,
                        "screen_recording_registration": false,
                        "screen_recording": true,
                        "driver_restarting": false,
                        "tcc_propagation": false,
                        "capture_verification": false
                    },
                    "ready": "ready",
                    "failed": "failed",
                },
            })
        );
    }

    #[test]
    fn ready_requires_a_verified_capture_terminal() {
        assert!(OnboardingStatus::ready(permissions(false, true)).is_none());
        assert!(OnboardingStatus::ready(permissions(true, false)).is_none());
        let ready = OnboardingStatus::ready(permissions(true, true)).unwrap();
        assert!(ready.is_terminal());
        assert_eq!(ready.stage, OnboardingStage::Ready);
        assert_eq!(ready.screen_recording_capturable, Some(true));
        assert!(!ready.requires_user_action);
        assert!(!ready.retryable);
    }

    #[test]
    fn failure_is_structured_and_terminal() {
        let failure = OnboardingStatus::failed(
            permissions(true, true),
            Some(false),
            true,
            "capture_unavailable",
            "live capture failed",
        );
        assert!(failure.is_terminal());
        assert_eq!(failure.code, "computer_use_setup_failed");
        assert_eq!(failure.stage, OnboardingStage::Failed);
        assert!(failure.retryable);
        assert!(!failure.requires_user_action);
        assert_eq!(failure.screen_recording_capturable, Some(false));
        assert_eq!(failure.error.unwrap().code, "capture_unavailable");

        let terminal = OnboardingStatus::failed(
            permissions(false, false),
            None,
            false,
            "identity_invalid",
            "invalid app identity",
        );
        assert!(!terminal.retryable);
    }

    #[test]
    fn capture_evidence_persistence_failure_has_a_bounded_terminal_contract() {
        let failure = OnboardingStatus::capture_verification_store_failed(permissions(true, true));
        let error = failure.error.expect("structured failure");

        assert_eq!(failure.code, SETUP_FAILED_CODE);
        assert_eq!(failure.stage, OnboardingStage::Failed);
        assert!(!failure.retryable);
        assert!(!failure.requires_user_action);
        assert_eq!(failure.screen_recording_capturable, Some(true));
        assert_eq!(error.code, DIRECT_CAPTURE_VERIFICATION_STORE_FAILED_CODE);
        assert!(error.code.len() <= 128);
        assert!(error.message.len() <= 1024);
    }

    #[test]
    fn capture_verification_honors_cancellation_before_starting() {
        assert_eq!(
            verify_live_screen_capture_with_cancel(Duration::from_secs(1), || true),
            CaptureVerificationOutcome::Cancelled
        );
    }

    #[test]
    fn automatic_restart_stages_never_request_user_action() {
        for stage in [
            OnboardingStage::ScreenRecordingRegistration,
            OnboardingStage::DriverRestarting,
            OnboardingStage::TccPropagation,
            OnboardingStage::CaptureVerification,
        ] {
            let status = OnboardingStatus::pending(stage, permissions(true, true));
            assert!(!status.requires_user_action, "stage {stage:?}");
        }
        for stage in [
            OnboardingStage::Accessibility,
            OnboardingStage::ScreenRecording,
        ] {
            let status = OnboardingStatus::pending(stage, permissions(false, false));
            assert!(status.requires_user_action, "stage {stage:?}");
        }
    }
}
