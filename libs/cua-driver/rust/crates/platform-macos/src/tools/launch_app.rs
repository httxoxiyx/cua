use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::path::PathBuf;

mod reuse;

pub struct LaunchAppTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "launch_app".into(),
        description:
            "Request a background macOS app launch with bounded protection against app self-activation.\n\n\
             Provide either `bundle_id` (preferred — unambiguous, e.g. `com.apple.calculator`) \
             or `name` (e.g. \"Calculator\"). If both are given, bundle_id wins.\n\n\
             Optional `urls` are handed to the app as open targets. Finder folder paths use \
             the normal background URL handoff with the same bounded focus protection.\n\n\
             Browser DevTools setup belongs to `browser_prepare`, which can prove that a \
             separate isolated profile is driver-owned before enabling CDP.\n\n\
             Optional `webkit_inspector_port`: opens a WebKit inspector server on the specified \
             port (sets WEBKIT_INSPECTOR_SERVER=127.0.0.1:N + TAURI_WEBVIEW_AUTOMATION=1). \
             Use this for Tauri/WebKit-based apps.\n\n\
             Optional `creates_new_application_instance`: when true, forces a new app instance \
             even if one is already running (passes -n to open). Reach for this when another \
             agent or session may drive the SAME app concurrently — it returns a fresh pid + \
             window so each session acts on its own isolated window instead of clobbering one \
             shared instance. Without it, single-instance apps (Calculator, many utilities) hand \
             every caller the same window, so two sessions fight over it.\n\n\
             Optional `additional_arguments`: extra argv strings appended after --args.\n\n\
             Returns the launched app's pid, bundle_id, name, and a `windows` array \
             (same shape as `list_windows`) so callers can skip an extra round-trip before \
             `get_window_state(pid, window_id)`. `launch_state` distinguishes whether the \
             request was sent, the process is running, and a window is ready. When the \
             bounded suppression window ran (target pid ≠ prior frontmost), the response \
             includes `self_activation_suppressed: bool` — true if the launched app was \
             not frontmost at its end, false if it held focus. No delayed demotion follows."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "bundle_id": {
                    "type": "string",
                    "description": "App bundle identifier, e.g. com.apple.calculator. Preferred over name."
                },
                "name": {
                    "type": "string",
                    "description": "App display name. Used only when bundle_id is absent."
                },
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file paths or URLs to open with the app (e.g. a folder path for Finder)."
                },
                "webkit_inspector_port": {
                    "type": "integer",
                    "description": "Open a WebKit inspector server on this port (sets WEBKIT_INSPECTOR_SERVER env var)."
                },
                "creates_new_application_instance": {
                    "type": "boolean",
                    "description": "When true, force a new app instance even if already running (open -n). Use for concurrent multi-agent/multi-session work so each session gets an isolated instance + window instead of sharing one — on single-instance apps (e.g. Calculator) every caller otherwise gets the same window and the sessions clobber each other."
                },
                "additional_arguments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra arguments appended after --args when launching."
                }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: true,
    })
}

#[async_trait]
impl Tool for LaunchAppTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let bundle_id = args.opt_str("bundle_id");
        let name = args.opt_str("name");
        let mut response_bundle_id = bundle_id.clone();
        let response_requested_name = name.clone();
        let urls: Vec<String> = args
            .str_array("urls")
            .into_iter()
            .map(normalize_launch_url)
            .collect();
        if args.get("cdp_debugging_port").is_some() {
            return ToolResult::error(
                "cdp_debugging_port moved to browser_prepare so DevTools is never enabled on an unproven user profile",
            );
        }
        let webkit_inspector_port = args.opt_u64("webkit_inspector_port").map(|v| v as u16);
        let creates_new_instance = args.bool_or("creates_new_application_instance", false);
        let additional_arguments: Vec<String> = args.str_array("additional_arguments");
        if additional_arguments.iter().any(|argument| {
            argument == super::check_permissions::PERMISSIONS_HOST_REQUEST_ARG
                || argument == crate::permissions::onboarding::ONBOARDING_CONTRACT_ARG
                || argument == crate::permissions::onboarding::ONBOARDING_LAUNCH_ARG
                || argument == crate::permissions::onboarding::ONBOARDING_HOST_ARG
        }) {
            return protected_host_launch_refusal();
        }
        if additional_arguments
            .iter()
            .any(|argument| contains_remote_debugging_flag(argument))
        {
            return ToolResult::error(
                "Chromium remote-debugging flags moved to browser_prepare so DevTools is never enabled on an unproven user profile",
            );
        }

        if bundle_id.is_none() && name.is_none() {
            return ToolResult::error(
                "Provide either bundle_id or name to identify the app to launch.",
            );
        }
        if bundle_id.as_deref().is_some_and(is_cua_driver_bundle_id) {
            return protected_host_launch_refusal();
        }
        if let Some(ref bid) = bundle_id {
            if crate::apps::resolve_bundle_id_to_locator(bid).is_none() {
                return structured_launch_error(
                    "APP_NOT_INSTALLED",
                    format!("No installed macOS app found for bundle_id '{bid}'."),
                    serde_json::json!({ "bundle_id": bid }),
                );
            }
        } else if let Some(ref n) = name {
            let Some(locator) = crate::apps::locate_by_name(n) else {
                return structured_launch_error(
                    "APP_NOT_INSTALLED",
                    format!("No installed macOS app found for name '{n}'."),
                    serde_json::json!({ "name": n }),
                );
            };
            let (_, resolved_bundle_id) = locator.app_ref_and_bundle_id();
            response_bundle_id = resolved_bundle_id.clone();
            if resolved_bundle_id
                .as_deref()
                .is_some_and(is_cua_driver_bundle_id)
            {
                return protected_host_launch_refusal();
            }
        }
        if let Some(err) = preflight_file_urls(&urls) {
            return err;
        }

        // Build env dict for webkit inspector.
        let mut env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if let Some(port) = webkit_inspector_port {
            env.insert(
                "WEBKIT_INSPECTOR_SERVER".to_string(),
                format!("127.0.0.1:{port}"),
            );
            env.insert("TAURI_WEBVIEW_AUTOMATION".to_string(), "1".to_string());
        }

        let port_summary = {
            let mut s = String::new();
            if let Some(port) = webkit_inspector_port {
                s.push_str(&format!("\nWebKit inspector available on port {port}."));
            }
            s
        };

        // ── Layer-3 focus-steal suppression ─────────────────────────────
        //
        // Captures the prior frontmost pid, arms a wildcard suppression
        // BEFORE the launch (covers self-activations the target fires
        // synchronously during `open()`), then upgrades to a targeted
        // suppression keyed to the actual launched pid. Narrow the SAME
        // lease atomically: this has no wildcard→targeted gap and retains
        // the original exact-window restoration evidence. A new capture
        // after the target activates can no longer prove the prior window.
        //
        // A newly launched native app can activate after its first window is
        // published (Keka did so about 1.4s after launch return). The short
        // reopen settle period is only suitable for an already-running pid.
        // The original five-second lease cap and uninterrupted native activity
        // evidence still apply; no later timer may undo a user activation.
        let previously_running_pids: Vec<i32> = response_bundle_id
            .as_deref()
            .map(|bid| {
                crate::apps::list_running_apps()
                    .into_iter()
                    .filter(|app| app.bundle_id.as_deref() == Some(bid))
                    .map(|app| app.pid)
                    .collect()
            })
            .unwrap_or_default();
        // A bare request can reuse a positively observed, unhidden ordinary
        // window without sending the application's reopen AppleEvent. Do not
        // infer this from the running PID alone, or short-circuit file opens.
        if reuse::bare_request(&urls, &additional_arguments, &env, creates_new_instance) {
            if let (Some(pid), Some(bid)) = (
                existing_reopen_pid(&previously_running_pids, creates_new_instance),
                response_bundle_id.clone(),
            ) {
                let reused = crate::foreground_activity::spawn_blocking(move || {
                    crate::foreground_activity::check_request()?;
                    let proof = reuse::observe(pid, &bid);
                    crate::foreground_activity::check_request()?;
                    Ok::<_, anyhow::Error>(proof)
                })
                .await;
                match reused {
                    Ok(Ok(Some((app, windows)))) => {
                        let windows: Vec<Value> = windows
                            .iter()
                            .map(super::list_windows::window_record_json)
                            .collect();
                        return ToolResult::text(format!(
                            "Reused existing {} (pid {}) without a launch request. Call get_window_state to inspect.",
                            app.name, app.pid,
                        )).with_structured(serde_json::json!({
                            "pid": app.pid, "bundle_id": app.bundle, "name": app.name,
                            "windows": windows,
                            "launch_state": launch_state(false, true, true),
                            "visibility_request": {"requested": false, "was_hidden": false},
                        }));
                    }
                    Ok(Ok(None)) => {} // Unproven: retain the original launch path.
                    Ok(Err(error)) => return structured_launch_failure(&error),
                    Err(error) => return ToolResult::error(format!("Task error: {error}")),
                }
            }
        }
        let prior_frontmost = crate::apps::frontmost_pid();
        // Every handoff, including a Finder folder, uses the normal
        // activates=false NSWorkspace configuration and the same lease.
        let wildcard_lease = prior_frontmost.map(|prior| {
            crate::focus_steal::FocusStealPreventer::begin_suppression(
                None,
                prior,
                "LaunchAppTool.pre",
            )
        });

        // A hidden application can order its old window above the user's
        // foreground without activating. Capture those exact window IDs and
        // the original foreground before reopening; a post-launch capture
        // would wrongly treat the already-raised window as the baseline.
        let reopen_ordering = capture_reopen_ordering(
            &previously_running_pids,
            response_bundle_id.as_deref(),
            creates_new_instance,
            !urls.is_empty(),
        );

        // Predicate captured BEFORE moving inputs into spawn_blocking.
        // Same condition that selects the `openURLs:withApplicationAtURL:`
        // chain over the simpler `openApplicationAtURL:` path. Used after
        // the spawn returns to size the suppression window — the slow
        // path triggers a SECOND activation when the file-open delivers,
        // which lands AFTER the bundle-only-launch activation window.
        let slow_launch_path = !urls.is_empty()
            || !additional_arguments.is_empty()
            || !env.is_empty()
            || creates_new_instance;
        let expected_bundle_id = response_bundle_id.clone();

        // Move the launch closure inputs into spawn_blocking. The
        // blocking task returns (pid, app_info, windows). Suppression
        // upgrade happens AFTER the blocking call returns (back on the
        // async runtime), then we sleep holding the targeted lease.
        let launch_result = crate::foreground_activity::spawn_blocking(move || {
            let pid = if let Some(ref bid) = bundle_id {
                if urls.is_empty()
                    && additional_arguments.is_empty()
                    && env.is_empty()
                    && !creates_new_instance
                {
                    crate::apps::launch_app(bid)?
                } else {
                    crate::apps::launch_with_urls_by_bundle(
                        bid,
                        &urls,
                        &additional_arguments,
                        &env,
                        creates_new_instance,
                    )?
                }
            } else {
                let n = name.as_deref().unwrap();
                if urls.is_empty()
                    && additional_arguments.is_empty()
                    && env.is_empty()
                    && !creates_new_instance
                {
                    crate::apps::launch_app_by_name(n)?
                } else {
                    crate::apps::launch_with_urls_by_name(
                        n,
                        &urls,
                        &additional_arguments,
                        &env,
                        creates_new_instance,
                    )?
                }
            };

            // LaunchServices can acknowledge rapp while leaving an existing
            // document app hidden (notably when a Save sheet is open). The
            // caller explicitly requested reopening this application. Restore
            // only its visibility; do not request activation or a window raise.
            let visibility = unhide_requested_application(pid, expected_bundle_id.as_deref());

            // Retry loop: LaunchServices returns before WindowServer has
            // registered the new windows. Poll up to 5x100ms.
            let windows = resolve_windows_for_pid(pid);

            let app_info: Option<crate::apps::AppInfo> = {
                let apps = crate::apps::list_running_apps();
                apps.into_iter().find(|a| a.pid == pid)
            };

            Ok::<_, anyhow::Error>((pid, app_info, windows, visibility))
        })
        .await;

        // Narrow to the real pid with the original deadline and input
        // generation. If that lease already expired, a new one must obtain
        // fresh evidence; it cannot revive the expired restoration proof.
        //
        // Report the actual foreground state at the end of suppression.
        // This does not prove who caused an activation or retry a demotion.
        let mut self_activation_suppressed: Option<bool> = None;
        if let Ok(Ok((pid, _, _, _))) = &launch_result {
            if let Some(prior) = prior_frontmost {
                if *pid != prior {
                    let mut targeted_lease = wildcard_lease
                        .and_then(|lease| lease.narrow_to(*pid, "LaunchAppTool.post"))
                        .unwrap_or_else(|| {
                            crate::focus_steal::FocusStealPreventer::begin_suppression(
                                Some(*pid),
                                prior,
                                "LaunchAppTool.post_fresh",
                            )
                        });
                    if let Some((expected_pid, mut ordering)) = reopen_ordering {
                        // Never transfer a prior process's window evidence to
                        // a replacement instance returned by LaunchServices.
                        if expected_pid == *pid {
                            targeted_lease.start_polling(move |deadline, diagnostics| {
                                ordering.poll(deadline, diagnostics)
                            });
                        }
                    }
                    // Cold launches and file/argument delivery get a bounded
                    // 2.5s settle period. A simple reopen of the same running
                    // pid keeps the shorter 500ms period. This covers the
                    // observed late activation without extending the lease's
                    // original deadline or deferring restoration after return.
                    let cold_launch = !previously_running_pids.contains(pid);
                    let window_ms: u64 = if slow_launch_path || cold_launch {
                        2500
                    } else {
                        500
                    };
                    tracing::debug!(target: "cua_focus_restore", target_pid = *pid,
                        cold_launch, settle_ms = window_ms,
                        "Holding bounded post-launch focus protection");
                    tokio::time::sleep(std::time::Duration::from_millis(window_ms)).await;
                    drop(targeted_lease);

                    // Do not re-activate an old application after the bounded
                    // operation, and never run a detached demotion watchdog.
                    // The observer's exact-window evidence is revoked by any
                    // human/unattributed input. Report the actual final state.
                    self_activation_suppressed = Some(crate::apps::frontmost_pid() != Some(*pid));
                } else {
                    // pid == prior frontmost (re-launch of an already-
                    // frontmost app). Just drop the wildcard.
                    drop(wildcard_lease);
                }
            }
        } else {
            // Launch failed; just drop the lease.
            drop(wildcard_lease);
        }

        match launch_result {
            Ok(Ok((pid, app_info, windows, visibility))) => {
                let (app_name, bid) = response_identity(
                    app_info.as_ref(),
                    response_bundle_id.as_deref(),
                    response_requested_name.as_deref(),
                );

                let mut summary =
                    format!("Launched {app_name} (pid {pid}) in background.{port_summary}");

                if !windows.is_empty() {
                    summary.push_str("\n\nWindows:");
                    for w in &windows {
                        let title = if w.title.is_empty() {
                            "(no title)".to_owned()
                        } else {
                            format!("\"{}\"", w.title)
                        };
                        summary.push_str(&format!("\n- {title} [window_id: {}]", w.window_id));
                    }
                    summary.push_str(&format!(
                        "\n→ Call get_window_state(pid: {pid}, window_id) to inspect."
                    ));
                }

                let windows_json: Vec<Value> = windows
                    .iter()
                    .map(super::list_windows::window_record_json)
                    .collect();

                let mut structured = serde_json::json!({
                    "pid": pid,
                    "bundle_id": bid,
                    "name": app_name,
                    "windows": windows_json,
                    "launch_state": launch_state(true, true, !windows.is_empty()),
                    "visibility_request": visibility,
                });
                // Only emit `self_activation_suppressed` when the
                // bounded suppression window actually ran. `None`
                // means the launch didn't enter the focus-steal path
                // (no prior frontmost, or pid == prior) — surfacing
                // a stale `false` would be misleading.
                if let Some(suppressed) = self_activation_suppressed {
                    structured["self_activation_suppressed"] = serde_json::Value::Bool(suppressed);
                }
                ToolResult::text(summary).with_structured(structured)
            }
            Ok(Err(e)) => structured_launch_failure(&e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

fn contains_remote_debugging_flag(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("--remote-debugging-port") || lower.contains("--remote-debugging-pipe")
}

fn is_cua_driver_bundle_id(bundle_id: &str) -> bool {
    matches!(
        bundle_id,
        "com.meta.musecode.cua.driver"
            | "com.meta.musecode.cua.driver.local"
            | "com.trycua.driver"
            | "com.trycua.driver.local"
            | "com.trycua.cuadriverrs"
    )
}

fn protected_host_launch_refusal() -> ToolResult {
    structured_launch_error(
        "PROTECTED_HOST_ENTRYPOINT",
        "launch_app cannot launch Cua Driver's protected host; operating-system permission UI must originate outside the agent tool stream".to_owned(),
        serde_json::json!({}),
    )
}

// ── Blocking helpers ──────────────────────────────────────────────────────────

fn existing_reopen_pid(pids: &[i32], creates_new_instance: bool) -> Option<i32> {
    match pids {
        [pid] if *pid > 0 && !creates_new_instance => Some(*pid),
        _ => None,
    }
}

/// Capture only an existing, uniquely identified process before the handoff.
/// Cold launches remain outside this existing-process guard. An explicit file
/// request can separately enroll newly created AX standard document windows,
/// bound to this process lifetime and the unchanged original foreground.
fn capture_reopen_ordering(
    pids: &[i32],
    expected_bundle_id: Option<&str>,
    creates_new_instance: bool,
    opens_file: bool,
) -> Option<(i32, crate::background_order::BackgroundOrderGuard)> {
    use objc2_app_kit::NSRunningApplication;
    tracing::debug!(target: "cua_window_order", ?pids, creates_new_instance,
        bundle_identity_available=expected_bundle_id.is_some(),
        "Checking pre-reopen window-order admission");
    let pid = existing_reopen_pid(pids, creates_new_instance)?;
    let expected = expected_bundle_id?;
    let hidden = unsafe {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        if app.isTerminated()
            || app.bundleIdentifier().map(|id| id.to_string()).as_deref() != Some(expected)
        {
            return None;
        }
        app.isHidden()
    };
    let before = crate::windows::all_windows_with_space_snapshot();
    if !before.succeeded {
        tracing::debug!(target: "cua_window_order", pid, hidden,
            reason="window_enumeration_unavailable", "Reopen ordering capture unavailable");
        return None;
    }
    let visible = crate::windows::visible_windows_with_space_snapshot();
    let ordering = crate::background_order::BackgroundOrderGuard::capture_before_file_open(
        pid, &before, &visible, hidden, opens_file,
    )?;
    Some((pid, ordering))
}

/// Ask only the exact launched application to become visible, without activation.
fn unhide_requested_application(pid: i32, expected_bundle_id: Option<&str>) -> Value {
    use objc2_app_kit::NSRunningApplication;
    let Some(expected_bundle_id) = expected_bundle_id else {
        return serde_json::json!({"requested": false, "reason": "bundle_identity_unavailable"});
    };
    unsafe {
        let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
            return serde_json::json!({"requested": false, "reason": "process_unavailable"});
        };
        if app.isTerminated()
            || app.bundleIdentifier().map(|bid| bid.to_string()).as_deref()
                != Some(expected_bundle_id)
        {
            return serde_json::json!({"requested": false, "reason": "process_identity_changed"});
        }
        if !app.isHidden() {
            return serde_json::json!({"requested": false, "was_hidden": false});
        }
        if crate::foreground_activity::check_request().is_err() {
            return serde_json::json!({"requested": false, "was_hidden": true, "reason": "request_interrupted"});
        }
        // unhide() acknowledges only that AppKit accepted the request. Its
        // asynchronous effect must be checked by the next observation; never
        // advertise it as verified visibility or successful document opening.
        let accepted = app.unhide();
        tracing::debug!(target: "cua_focus_restore", pid, accepted,
            "Requested background application visibility");
        serde_json::json!({"requested": true, "accepted": accepted, "was_hidden": true})
    }
}

/// Poll for the pid's layer-0 windows, retrying up to 5x100ms to absorb
/// LaunchServices → WindowServer latency (mirrors the Swift reference).
fn resolve_windows_for_pid(pid: i32) -> Vec<crate::windows::WindowInfo> {
    for attempt in 0..5 {
        let found: Vec<_> = crate::windows::all_automation_windows()
            .into_iter()
            .filter(|w| w.pid == pid && w.layer == 0)
            .filter(|w| w.bounds.width > 1.0 && w.bounds.height > 1.0)
            .collect();
        if !found.is_empty() {
            return found;
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    vec![]
}

fn structured_launch_error(code: &str, message: String, details: serde_json::Value) -> ToolResult {
    let mut payload = serde_json::json!({
        "error": code,
    });

    match details {
        serde_json::Value::Object(details) => {
            if let serde_json::Value::Object(payload) = &mut payload {
                payload.extend(details);
            }
        }
        details => {
            if let serde_json::Value::Object(payload) = &mut payload {
                payload.insert("details".to_string(), details);
            }
        }
    }

    ToolResult::error(message).with_structured(payload)
}

fn launch_state(requested: bool, process_running: bool, window_ready: bool) -> serde_json::Value {
    serde_json::json!({
        "requested": requested,
        "process_running": process_running,
        "window_ready": window_ready,
    })
}

fn response_identity(
    app_info: Option<&crate::apps::AppInfo>,
    requested_bundle_id: Option<&str>,
    requested_name: Option<&str>,
) -> (String, String) {
    let bundle_id = app_info
        .and_then(|app| app.bundle_id.as_deref())
        .filter(|value| !value.is_empty())
        .or(requested_bundle_id)
        .unwrap_or("?")
        .to_owned();

    let name = app_info
        .map(|app| app.name.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| requested_app_name(requested_name, requested_bundle_id));

    (name, bundle_id)
}

fn requested_app_name(requested_name: Option<&str>, requested_bundle_id: Option<&str>) -> String {
    if let Some(name) = requested_name.filter(|name| Some(*name) != requested_bundle_id) {
        let file_name = std::path::Path::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(name);
        return file_name
            .strip_suffix(".app")
            .unwrap_or(file_name)
            .to_owned();
    }

    requested_bundle_id
        .and_then(|bundle_id| bundle_id.rsplit('.').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("?")
        .to_owned()
}

fn structured_launch_failure(error: &anyhow::Error) -> ToolResult {
    use crate::apps::nsworkspace::LaunchError;

    let (code, requested) = if let Some(launch_error) = error.downcast_ref::<LaunchError>() {
        match launch_error {
            LaunchError::Cocoa(_) => ("NSWORKSPACE_LAUNCH_FAILED", true),
            LaunchError::NoApp => ("LAUNCH_RESULT_MISSING", true),
            LaunchError::Timeout => ("LAUNCH_CALLBACK_TIMEOUT", true),
            LaunchError::BadUrl(_) => ("APP_URL_INVALID", false),
        }
    } else {
        ("LAUNCH_FAILED", false)
    };

    structured_launch_error(
        code,
        format!("Launch failed: {error:#}"),
        serde_json::json!({
            "launch_state": launch_state(requested, false, false),
        }),
    )
}

fn preflight_file_urls(urls: &[String]) -> Option<ToolResult> {
    for raw in urls {
        let Some(path) = local_file_target(raw) else {
            continue;
        };
        if !path.exists() {
            return Some(structured_launch_error(
                "FILE_NOT_FOUND",
                format!(
                    "Local launch_app url target does not exist: {}",
                    path.display()
                ),
                serde_json::json!({
                    "url": raw,
                    "path": path.display().to_string(),
                }),
            ));
        }
    }
    None
}

fn normalize_launch_url(raw: String) -> String {
    local_file_target(&raw)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(raw)
}

fn local_file_target(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return Some(PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("file://") {
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        let decoded = percent_decode_path(path);
        return Some(expand_tilde(&decoded));
    }
    let looks_like_url = raw.contains(':') && !raw.starts_with('/') && !raw.starts_with('~');
    if looks_like_url {
        return None;
    }
    Some(expand_tilde(raw))
}

fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn percent_decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                decoded.push((high << 4) | low);
                i += 3;
                continue;
            }
        }

        decoded.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        contains_remote_debugging_flag, existing_reopen_pid, is_cua_driver_bundle_id,
        local_file_target, normalize_launch_url, preflight_file_urls, response_identity,
        structured_launch_failure, LaunchAppTool,
    };
    use cua_driver_core::tool::Tool;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn reopen_ordering_requires_one_existing_process_and_no_new_instance_request() {
        assert_eq!(existing_reopen_pid(&[42], false), Some(42));
        for pids in [&[][..], &[0], &[-1], &[41, 42], &[42, 42]] {
            assert_eq!(existing_reopen_pid(pids, false), None);
        }
        assert_eq!(existing_reopen_pid(&[42], true), None);
    }

    #[test]
    fn local_file_target_treats_plain_paths_as_files() {
        assert_eq!(
            local_file_target("/tmp/does-not-exist.md"),
            Some(PathBuf::from("/tmp/does-not-exist.md"))
        );
        assert_eq!(
            local_file_target("relative/path.md"),
            Some(PathBuf::from("relative/path.md"))
        );
    }

    #[test]
    fn launch_url_normalization_expands_home_relative_paths() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME must be set for macOS"));

        assert_eq!(
            PathBuf::from(normalize_launch_url("~/Desktop/BenchInbox".to_owned())),
            home.join("Desktop/BenchInbox")
        );
        assert_eq!(
            normalize_launch_url("https://example.com".to_owned()),
            "https://example.com"
        );
    }

    #[test]
    fn local_file_target_skips_remote_and_custom_schemes() {
        assert_eq!(local_file_target("https://example.com"), None);
        assert_eq!(local_file_target("about:blank"), None);
        assert_eq!(local_file_target("myapp://open/item"), None);
    }

    #[test]
    fn preflight_file_urls_returns_structured_file_not_found() {
        let missing = "/tmp/cua-driver-definitely-missing-file-for-test.md".to_string();
        let result = preflight_file_urls(&[missing]).expect("missing file should error");
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured error");
        assert_eq!(structured["error"], "FILE_NOT_FOUND");
        assert_eq!(
            structured["path"],
            "/tmp/cua-driver-definitely-missing-file-for-test.md"
        );
        assert!(structured.get("details").is_none());
    }

    #[test]
    fn local_file_target_percent_decodes_file_urls_before_path_checks() {
        assert_eq!(
            local_file_target("file:///tmp/My%20Doc.txt"),
            Some(PathBuf::from("/tmp/My Doc.txt"))
        );
        assert_eq!(
            local_file_target("file://localhost/tmp/%E2%9C%93.txt"),
            Some(PathBuf::from("/tmp/✓.txt"))
        );
    }

    #[test]
    fn rejects_all_chromium_remote_debugging_spellings() {
        assert!(contains_remote_debugging_flag("--remote-debugging-port=0"));
        assert!(contains_remote_debugging_flag("--REMOTE-DEBUGGING-PIPE"));
        assert!(!contains_remote_debugging_flag(
            "--user-data-dir=/tmp/profile"
        ));
    }

    #[test]
    fn recognizes_release_and_local_protected_host_bundle_ids() {
        assert!(is_cua_driver_bundle_id("com.meta.musecode.cua.driver"));
        assert!(is_cua_driver_bundle_id(
            "com.meta.musecode.cua.driver.local"
        ));
        assert!(is_cua_driver_bundle_id("com.trycua.driver"));
        assert!(is_cua_driver_bundle_id("com.trycua.driver.local"));
        assert!(is_cua_driver_bundle_id("com.trycua.cuadriverrs"));
        assert!(!is_cua_driver_bundle_id("com.trycua.harness.tauri"));
    }

    #[test]
    fn launch_timeout_reports_requested_without_process_or_window() {
        let error = anyhow::Error::new(crate::apps::nsworkspace::LaunchError::Timeout)
            .context("Failed to launch com.example.App");
        let result = structured_launch_failure(&error);
        let structured = result.structured_content.expect("structured error");

        assert_eq!(result.is_error, Some(true));
        assert_eq!(structured["error"], "LAUNCH_CALLBACK_TIMEOUT");
        assert_eq!(structured["launch_state"]["requested"], true);
        assert_eq!(structured["launch_state"]["process_running"], false);
        assert_eq!(structured["launch_state"]["window_ready"], false);
    }

    #[test]
    fn invalid_url_reports_request_was_not_sent() {
        let error = anyhow::Error::new(crate::apps::nsworkspace::LaunchError::BadUrl(
            "bad url".to_owned(),
        ))
        .context("Failed to launch com.example.App");
        let result = structured_launch_failure(&error);
        let structured = result.structured_content.expect("structured error");

        assert_eq!(structured["error"], "APP_URL_INVALID");
        assert_eq!(structured["launch_state"]["requested"], false);
        assert_eq!(structured["launch_state"]["process_running"], false);
        assert_eq!(structured["launch_state"]["window_ready"], false);
    }

    #[test]
    fn process_only_response_falls_back_to_requested_identity() {
        assert_eq!(
            response_identity(None, Some("com.apple.Safari"), None),
            ("Safari".to_owned(), "com.apple.Safari".to_owned())
        );
        assert_eq!(
            response_identity(
                None,
                Some("com.example.Editor"),
                Some("/Applications/Example Editor.app"),
            ),
            ("Example Editor".to_owned(), "com.example.Editor".to_owned())
        );
    }

    #[tokio::test]
    async fn launch_app_cannot_reach_private_permission_host_entrypoint() {
        for private_argument in [
            "__permissions-host-request",
            crate::permissions::onboarding::ONBOARDING_CONTRACT_ARG,
            crate::permissions::onboarding::ONBOARDING_LAUNCH_ARG,
            crate::permissions::onboarding::ONBOARDING_HOST_ARG,
        ] {
            let result = LaunchAppTool
                .invoke(json!({
                    "bundle_id": "com.example.not-installed",
                    "additional_arguments": [
                        private_argument,
                        "--result-file",
                        "/tmp/cua-driver-permissions-forged.json"
                    ]
                }))
                .await;
            assert_eq!(result.is_error, Some(true));
            assert_eq!(
                result.structured_content.unwrap()["error"],
                "PROTECTED_HOST_ENTRYPOINT"
            );
        }

        let result = LaunchAppTool
            .invoke(json!({ "bundle_id": "com.meta.musecode.cua.driver" }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"],
            "PROTECTED_HOST_ENTRYPOINT"
        );
    }
}
