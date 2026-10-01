#![recursion_limit = "256"]

//! cua-driver-rs — cross-platform background computer-use automation daemon.
//!
//! Runs a daemon-backed MCP JSON-RPC 2.0 proxy over stdio. The platform
//! backend lives in the `serve` daemon selected at compile time.
//!
//! Extra CLI flags (consumed here, not by MCP):
//!   --cursor-theme <installed-theme-id>   installed cursor theme
//!   --cursor-reduced-motion <auto|on|off> accessibility motion preference
//!   --no-overlay                          start with overlay disabled
//!   --async-click-feedback                default: do not wait for decorative glides
//!   --sync-click-feedback                 wait for decorative glides (demonstrations)
//!   --glide-ms     <f64>                  glide duration override
//!   --dwell-ms     <f64>                  post-click dwell override
//!   --idle-hide-ms <f64>                  idle-hide timeout override
//!
//! On macOS, `serve` keeps AppKit work on the main thread while its socket loop
//! runs in the background. MCP and CLI client processes never initialize the
//! platform tool registry.

mod autostart;
mod bundle;
mod check_update_tool;
mod cli;
mod doctor;
mod history_runtime;
mod mcp_http;
mod private_worker;
mod proxy;
mod release_channel;
mod responsibility;
mod sdk_adapter;
mod serve;
mod skills;
mod stop;
mod telemetry;
mod version_check;

use std::sync::Arc;

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_env("CUA_LOG").add_directive(tracing::Level::WARN.into()))
        .init();
}

fn configure_startup_permission_mode(
    permission_mode: Option<&str>,
    dangerously_bypass_approvals: bool,
    capability_manifest: Option<&str>,
    approve_capability_manifest: bool,
    grants: &[String],
) -> anyhow::Result<()> {
    if let Some(mode) = permission_mode {
        std::env::set_var(cua_driver_core::authorization::PERMISSION_MODE_ENV, mode);
    } else if dangerously_bypass_approvals
        && std::env::var_os(cua_driver_core::authorization::PERMISSION_MODE_ENV).is_none()
    {
        // The alarming CLI flag is both the unrestricted-mode selector and
        // the user's explicit launch-time risk acknowledgement. Embedded and
        // environment-driven launchers retain the two-part mode + acceptance
        // contract because they do not pass through this CLI normalization.
        std::env::set_var(
            cua_driver_core::authorization::PERMISSION_MODE_ENV,
            "unrestricted",
        );
    }
    if dangerously_bypass_approvals {
        std::env::set_var(cua_driver_core::authorization::DANGEROUS_BYPASS_ENV, "1");
    }
    if let Some(path) = capability_manifest {
        std::env::set_var(
            cua_driver_core::session_manifest::CAPABILITY_MANIFEST_FILE_ENV,
            path,
        );
    }
    if approve_capability_manifest {
        std::env::set_var(
            cua_driver_core::session_manifest::CAPABILITY_MANIFEST_APPROVED_ENV,
            "1",
        );
    }
    let mode =
        cua_driver_core::authorization::configured_permission_mode().map_err(anyhow::Error::msg)?;
    if !grants.is_empty() && mode != cua_driver_core::authorization::PermissionMode::Standard {
        anyhow::bail!("--grant is valid only in standard permission mode");
    }
    cua_driver_core::authorization::configure_launch_grants(grants).map_err(anyhow::Error::msg)?;
    cua_driver_core::authorization::validate_startup_authorization()?;
    if cua_driver_core::authorization::configured_permission_mode()
        .is_ok_and(|mode| mode == cua_driver_core::authorization::PermissionMode::Unrestricted)
    {
        eprintln!(
            "DANGER: Cua Driver is running in unrestricted mode. Runtime approval prompts are disabled; prompt injection or unintended input may act with every capability allowed by the built-in, managed, and user policy ceilings. Use only in a disposable or fully trusted environment."
        );
    }
    Ok(())
}

/// `cua-driver telemetry ...`. Telemetry is removed from this build, so every
/// subcommand reports that. `reset-id` still deletes telemetry and update-check
/// files an earlier build may have left in any Cua Driver home directory; it is
/// a local-only operation.
fn run_telemetry_command(command: cli::TelemetryCommand) {
    match command {
        cli::TelemetryCommand::Status { json } => {
            let status = telemetry::status();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status).expect("serialize telemetry status")
                );
            } else {
                println!("{}", telemetry::REMOVED_NOTICE);
                if status.legacy_state_present {
                    println!("Telemetry or update-check files from an earlier build are still on disk; run `cua-driver telemetry reset-id` to delete them.");
                }
            }
        }
        cli::TelemetryCommand::ResetId => match telemetry::remove_legacy_state() {
            Ok(removed) => {
                println!("{}", telemetry::REMOVED_NOTICE);
                if removed.is_empty() {
                    println!(
                        "No telemetry or update-check files from an earlier build were found."
                    );
                }
                for path in removed {
                    println!("Removed {}", path.display());
                }
            }
            Err(error) => {
                eprintln!("cua-driver: failed to remove legacy telemetry files: {error}");
                std::process::exit(1);
            }
        },
        cli::TelemetryCommand::Enable => {
            eprintln!("{} It cannot be enabled.", telemetry::REMOVED_NOTICE);
            std::process::exit(1);
        }
        cli::TelemetryCommand::Disable
        | cli::TelemetryCommand::InstallEvent
        | cli::TelemetryCommand::Inspect => {
            println!("{}", telemetry::REMOVED_NOTICE);
        }
    }
}

fn run_cursor_theme_command(args: &[String]) -> ! {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cua-driver: cannot locate cursor-theme compiler: {error}");
            std::process::exit(1);
        }
    };
    let binary_name = if cfg!(target_os = "windows") {
        "cua-cursor-theme.exe"
    } else {
        "cua-cursor-theme"
    };
    let sidecar = executable
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(binary_name);
    let status = match std::process::Command::new(&sidecar).args(args).status() {
        Ok(status) => status,
        Err(error) => {
            eprintln!(
                "cua-driver: cursor-theme compiler is unavailable at {}: {error}",
                sidecar.display()
            );
            eprintln!("Reinstall Cua Driver so the matching authoring sidecar is present.");
            std::process::exit(1);
        }
    };
    std::process::exit(status.code().unwrap_or(1));
}

/// Wire up the experimental picture-in-picture preview window.
///
/// Called from every long-running entry point (Serve and Mcp on all
/// platforms; the `Call` arm intentionally skips PiP since the
/// per-call binaries don't keep an AppKit/event loop alive long
/// enough to be useful).
///
/// No-op when `--experimental-pip` is not on argv. On Windows / Linux
/// the factory returns "not yet implemented" — we log and continue
/// without a window so the rest of the daemon keeps working.
fn maybe_init_pip() {
    let cfg = match pip_preview::default_config_path() {
        Some(p) => pip_preview::PipConfig::from_args_and_file(&p),
        None => pip_preview::PipConfig::from_args(),
    };
    if !cfg.enabled {
        return;
    }

    // Register the platform factory. The set is idempotent so multiple
    // entry points calling this in the same process is safe.
    #[cfg(target_os = "macos")]
    pip_preview::set_pip_backend_factory(Box::new(platform_macos::pip::MacosPipBackendFactory));
    #[cfg(target_os = "windows")]
    pip_preview::set_pip_backend_factory(Box::new(platform_windows::pip::WindowsPipBackendFactory));
    #[cfg(target_os = "linux")]
    pip_preview::set_pip_backend_factory(Box::new(platform_linux::pip::LinuxPipBackendFactory));

    match pip_preview::start_pip(&cfg) {
        Ok(backend) => {
            // Bridge: when the tool dispatcher in cua-driver-core wants
            // to push a frame, forward to the live backend handle.
            // We move the Box into a static Mutex<Option<...>> so the
            // closure can re-borrow on every call without taking
            // ownership of the trait object.
            use std::sync::Mutex as StdMutex;
            static BACKEND: std::sync::OnceLock<
                StdMutex<Option<Box<dyn pip_preview::PipBackend>>>,
            > = std::sync::OnceLock::new();
            let _ = BACKEND.set(StdMutex::new(Some(backend)));
            cua_driver_core::pip_hook::set_pip_event_fn(|event| {
                if let Some(slot) = BACKEND.get() {
                    if let Some(b) = slot.lock().unwrap().as_ref() {
                        match event {
                            cua_driver_core::pip_hook::PipHookEvent::Upsert(frame) => {
                                b.push_frame(pip_preview::PipFrame {
                                    target: pip_preview::PipTarget {
                                        logical_pid: frame.target.logical_pid,
                                        delegation: frame.target.delegation.map(|delegation| {
                                            pip_preview::PipDelegation {
                                                kind: delegation.kind,
                                                host_pid: delegation.host_pid,
                                                panel_kind: delegation.panel_kind,
                                                expected_bundle_id: delegation.expected_bundle_id,
                                                expected_app_name: delegation.expected_app_name,
                                            }
                                        }),
                                        pid: frame.target.pid,
                                        window_id: frame.target.window_id,
                                        session_id: frame.target.session_id,
                                        app_name: String::new(),
                                        window_title: None,
                                    },
                                    png_bytes: frame.png_bytes,
                                    timestamp_ms: frame.timestamp_ms,
                                });
                            }
                            cua_driver_core::pip_hook::PipHookEvent::Ensure(target) => {
                                b.ensure_target(pip_preview::PipTarget {
                                    logical_pid: target.logical_pid,
                                    delegation: target.delegation.map(|delegation| {
                                        pip_preview::PipDelegation {
                                            kind: delegation.kind,
                                            host_pid: delegation.host_pid,
                                            panel_kind: delegation.panel_kind,
                                            expected_bundle_id: delegation.expected_bundle_id,
                                            expected_app_name: delegation.expected_app_name,
                                        }
                                    }),
                                    pid: target.pid,
                                    window_id: target.window_id,
                                    session_id: target.session_id,
                                    app_name: String::new(),
                                    window_title: None,
                                });
                            }
                            cua_driver_core::pip_hook::PipHookEvent::Observe(target) => {
                                b.observe_target(pip_preview::PipTarget {
                                    logical_pid: target.logical_pid,
                                    delegation: target.delegation.map(|delegation| {
                                        pip_preview::PipDelegation {
                                            kind: delegation.kind,
                                            host_pid: delegation.host_pid,
                                            panel_kind: delegation.panel_kind,
                                            expected_bundle_id: delegation.expected_bundle_id,
                                            expected_app_name: delegation.expected_app_name,
                                        }
                                    }),
                                    pid: target.pid,
                                    window_id: target.window_id,
                                    session_id: target.session_id,
                                    app_name: String::new(),
                                    window_title: None,
                                });
                            }
                            cua_driver_core::pip_hook::PipHookEvent::EndSession(session_id) => {
                                b.end_session(&session_id);
                            }
                            cua_driver_core::pip_hook::PipHookEvent::SetInputPassthrough {
                                passthrough,
                            } => {
                                return b
                                    .set_input_passthrough(passthrough)
                                    .map_err(|error| error.to_string());
                            }
                        }
                    }
                }
                Ok(())
            });
            eprintln!(
                "⚗️  PiP preview enabled (experimental — macOS only today; \
                 see https://github.com/trycua/cua/issues for follow-up)"
            );
        }
        Err(e) => {
            eprintln!("⚗️  PiP preview requested but unavailable: {e}");
        }
    }
}

// ── Public SDK runtime host ──────────────────────────────────────────────

/// Construct the canonical SDK-owned runtime for the CLI or daemon host.
/// The private socket and MCP layers consume this object downstream.
fn build_driver(
    cursor: cursor_overlay::CursorConfig,
    compatibility_mode: bool,
    host_owns_permission_ux: bool,
) -> Result<Arc<cua_driver_sdk::CuaDriver>, cua_driver_sdk::DriverError> {
    cua_driver_sdk::CuaDriver::try_create_service_for_host(cua_driver_sdk::DriverHostOptions {
        cursor,
        host_owns_permission_ux,
        host_bundle_id: std::env::var(cua_driver_core::HOST_BUNDLE_ID_ENV).ok(),
        claude_code_compatibility: compatibility_mode,
        prepare_desktop_environment: true,
        register_host_tools: Some(history_runtime::register_host_tools),
        authorization_host: None,
        activity_observer: None,
    })
}

#[cfg(test)]
fn build_driver_without_cursor() -> Arc<cua_driver_sdk::CuaDriver> {
    build_driver(
        cursor_overlay::CursorConfig {
            enabled: false,
            ..cursor_overlay::CursorConfig::default()
        },
        false,
        false,
    )
    .expect("test host requires an available desktop runtime")
}

/// Load the canonical SDK inventory without constructing an action runtime.
///
/// Finite metadata commands remain usable from non-interactive Windows
/// sessions, while `serve`, MCP, and direct SDK creation still fail closed
/// before accepting desktop actions.
fn inspect_tools_without_runtime() -> serde_json::Value {
    cua_driver_sdk::CuaDriver::inspect_host_tools(cua_driver_sdk::DriverHostOptions {
        cursor: cursor_overlay::CursorConfig {
            enabled: false,
            ..cursor_overlay::CursorConfig::default()
        },
        host_owns_permission_ux: false,
        host_bundle_id: None,
        claude_code_compatibility: false,
        prepare_desktop_environment: false,
        register_host_tools: Some(history_runtime::register_host_tools),
        authorization_host: None,
        activity_observer: None,
    })
}

#[cfg(test)]
fn test_runtime_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn run_mcp_direct(compatibility_mode: bool) -> anyhow::Result<()> {
    // Validate immutable process policy before platform initialization. The
    // adapter repeats this check before reading stdin as defense in depth.
    cua_driver_core::authorization::validate_startup_authorization()?;
    cua_driver_core::policy::validate_configured_policy()?;
    let cursor = cursor_overlay::CursorConfig::from_args();
    // A plain stdio MCP process does not provide the required AppKit
    // main-thread host adapter. Explicit direct mode on macOS must therefore
    // expose facility_unavailable instead of initializing an overlay that can
    // report success without a usable UI owner. Private-worker and app-service
    // hosts keep the full facility.
    #[cfg(target_os = "macos")]
    let cursor = {
        let mut cursor = cursor;
        cursor.enabled = false;
        cursor
    };
    let driver = build_driver(cursor, compatibility_mode, true)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(proxy::run_direct(driver))
}

fn history_admission_requested(explicit: bool, persisted: bool) -> bool {
    explicit || persisted
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacosAppKitHost {
    CursorOverlay,
    PipOnly,
    ReturnOnly,
    None,
}

#[cfg(target_os = "macos")]
fn macos_appkit_host(
    cursor_enabled: bool,
    pip_enabled: bool,
    return_host_enabled: bool,
) -> MacosAppKitHost {
    if cursor_enabled {
        MacosAppKitHost::CursorOverlay
    } else if pip_enabled {
        MacosAppKitHost::PipOnly
    } else if return_host_enabled {
        MacosAppKitHost::ReturnOnly
    } else {
        MacosAppKitHost::None
    }
}

#[cfg(test)]
mod history_admission_tests {
    use super::history_admission_requested;

    #[test]
    fn persisted_preview_admission_survives_a_relaunch_without_the_cli_flag() {
        assert!(history_admission_requested(false, true));
    }

    #[test]
    fn admission_requires_an_explicit_or_persisted_request() {
        assert!(!history_admission_requested(false, false));
        assert!(history_admission_requested(true, false));
        assert!(history_admission_requested(true, true));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_appkit_host_tests {
    use super::{macos_appkit_host, MacosAppKitHost};

    #[test]
    fn cursor_renderer_owns_the_shared_appkit_loop_when_pip_is_also_enabled() {
        for return_host_enabled in [false, true] {
            assert_eq!(
                macos_appkit_host(true, true, return_host_enabled),
                MacosAppKitHost::CursorOverlay
            );
            assert_eq!(
                macos_appkit_host(true, false, return_host_enabled),
                MacosAppKitHost::CursorOverlay
            );
            assert_eq!(
                macos_appkit_host(false, true, return_host_enabled),
                MacosAppKitHost::PipOnly
            );
        }
    }

    #[test]
    fn return_host_requires_opt_in_and_does_not_enable_observer_ui() {
        assert_eq!(
            macos_appkit_host(false, false, false),
            MacosAppKitHost::None
        );
        assert_eq!(
            macos_appkit_host(false, false, true),
            MacosAppKitHost::ReturnOnly
        );
    }
}

fn mcp_uses_direct_runtime(socket: Option<&str>, direct: bool) -> anyhow::Result<bool> {
    mcp_uses_direct_runtime_for(
        cua_driver_core::embedded_mode(),
        socket,
        cfg!(target_os = "macos"),
        direct,
        history_runtime::preview_admitted_preference(),
    )
}

fn mcp_uses_direct_runtime_for(
    embedded: bool,
    socket: Option<&str>,
    macos: bool,
    direct: bool,
    history_preview_admitted: bool,
) -> anyhow::Result<bool> {
    if direct && socket.is_some() {
        anyhow::bail!("--direct and --socket are mutually exclusive");
    }
    if direct {
        return Ok(true);
    }
    if embedded && socket.is_none() {
        anyhow::bail!("embedded hosts must provide their private service endpoint with --socket");
    }
    if macos {
        // Preserve LaunchServices/TCC attribution for normal macOS clients.
        Ok(false)
    } else {
        Ok(socket.is_none() && !history_preview_admitted)
    }
}

#[cfg(test)]
mod mcp_runtime_selection_tests {
    use super::mcp_uses_direct_runtime_for;

    #[test]
    fn embedded_host_without_private_endpoint_fails_closed() {
        let error = mcp_uses_direct_runtime_for(true, None, false, false, false).unwrap_err();
        assert!(error.to_string().contains("--socket"));
        let error = mcp_uses_direct_runtime_for(true, None, true, false, false).unwrap_err();
        assert!(error.to_string().contains("--socket"));
    }

    #[test]
    fn normal_linux_and_windows_stdio_own_the_runtime() {
        assert!(mcp_uses_direct_runtime_for(false, None, false, false, false).unwrap());
        assert!(!mcp_uses_direct_runtime_for(false, Some("service"), false, false, false).unwrap());
        assert!(!mcp_uses_direct_runtime_for(false, None, false, false, true).unwrap());
    }

    #[test]
    fn normal_macos_stdio_preserves_the_service_boundary() {
        assert!(!mcp_uses_direct_runtime_for(false, None, true, false, false).unwrap());
        assert!(!mcp_uses_direct_runtime_for(false, Some("service"), true, false, false).unwrap());
    }

    #[test]
    fn explicit_direct_owns_the_runtime_on_macos_and_in_embedded_hosts() {
        assert!(mcp_uses_direct_runtime_for(false, None, true, true, false).unwrap());
        assert!(mcp_uses_direct_runtime_for(true, None, true, true, false).unwrap());
        assert!(mcp_uses_direct_runtime_for(true, None, false, true, false).unwrap());
        let error =
            mcp_uses_direct_runtime_for(false, Some("service"), true, true, false).unwrap_err();
        assert!(error.to_string().contains("mutually exclusive"));
    }
}

/// Regression guard for the removed phone-home paths (telemetry, update
/// checks, remote skill downloads): no crate that is linked into, or shipped
/// beside, a driver binary may regain an analytics endpoint or key, an HTTP
/// client, or a GitHub API/raw/release-asset URL. Every workspace member is
/// scanned except the test-only harness, so a crate added later is covered
/// without editing this list. Needles are assembled at runtime so this test's
/// own source text cannot match them.
#[cfg(test)]
mod no_phone_home_tests {
    use std::path::{Path, PathBuf};

    /// The one workspace member that never ships.
    const TEST_ONLY_MEMBER: &str = "crates/cua-driver-testkit";

    /// Members of the Rust workspace, read from its manifest.
    fn workspace_members(workspace: &Path) -> Vec<String> {
        let manifest = std::fs::read_to_string(workspace.join("Cargo.toml")).unwrap();
        let list = manifest
            .split_once("members = [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(list, _)| list)
            .expect("workspace manifest lists its members");
        list.split(',')
            .map(|member| member.trim().trim_matches('"').to_owned())
            .filter(|member| !member.is_empty())
            .collect()
    }

    /// Every file under `path`: Rust sources and the data files they embed.
    fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                collect_files(&entry.unwrap().path(), files);
            }
        } else if path.is_file() {
            files.push(path.to_owned());
        }
    }

    #[test]
    fn shipped_crates_have_no_analytics_http_client_or_github_endpoints() {
        let needles = [
            ["post", "hog"].concat(),
            ["ph", "c_"].concat(),
            ["ure", "q"].concat(),
            ["req", "west"].concat(),
            ["api.", "github.com"].concat(),
            ["raw.", "githubusercontent.com"].concat(),
            ["releases/", "download"].concat(),
        ];
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let members = workspace_members(&workspace);
        // Linked into a cua-driver binary on some platform, or shipped beside it
        // (the Windows UIA helper, the cursor-theme sidecar). A rename must
        // update this list rather than silently drop a crate from the scan.
        for shipped in [
            "crates/cua-driver",
            "crates/cua-driver-core",
            "crates/cua-driver-sdk",
            "crates/cua-driver-contract",
            "crates/platform-macos",
            "crates/platform-windows",
            "crates/platform-linux",
            "crates/cua-driver-uia",
            "crates/cursor-overlay",
            "crates/cursor-theme-cli",
            "crates/pip-preview",
        ] {
            assert!(
                members.iter().any(|member| member == shipped),
                "{shipped} is not a workspace member: {members:?}"
            );
        }
        let mut files = Vec::new();
        for member in members.iter().filter(|member| *member != TEST_ONLY_MEMBER) {
            let crate_dir = workspace.join(member);
            let manifest = crate_dir.join("Cargo.toml");
            assert!(manifest.is_file(), "{} is missing", manifest.display());
            files.push(manifest);
            let build_script = crate_dir.join("build.rs");
            if build_script.is_file() {
                files.push(build_script);
            }
            collect_files(&crate_dir.join("src"), &mut files);
        }
        assert!(
            files.len() > 300,
            "source walk found only {} files",
            files.len()
        );
        for file in files {
            let bytes = std::fs::read(&file)
                .unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
            let source = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
            for needle in &needles {
                assert!(
                    !source.contains(needle.as_str()),
                    "{} contains {needle:?}",
                    file.display()
                );
            }
        }
    }

    #[test]
    fn telemetry_and_update_checks_answer_statically() {
        assert!(!crate::telemetry::is_enabled());
        let state = crate::version_check::update_state();
        assert!(!state.update_checks_enabled);
        assert!(!state.update_available);
        assert_eq!(state.latest_version, None);
    }
}

// ── macOS entry-point ─────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn main() {
    if let Some(code) = cli::run_build_attestation_if_requested() {
        std::process::exit(code);
    }
    if let Some(code) = cli::run_permissions_onboarding_contract_if_requested() {
        std::process::exit(code);
    }
    if let Some(code) = platform_macos::permissions::gate::run_permission_probe_if_requested() {
        std::process::exit(code);
    }
    // The packaged uninstaller needs a truly offline purge path that runs
    // before any other initialization, while this exact signed executable
    // still exists on disk.
    if let Some(code) = history_runtime::run_offline_purge_if_requested() {
        std::process::exit(code);
    }
    if let Some(code) = cli::run_plugin_managed_bare_launch_guard_if_requested() {
        std::process::exit(code);
    }
    init_logging();
    if let Some(code) = cli::run_permissions_onboarding_if_requested() {
        std::process::exit(code);
    }
    if let Some(code) = cli::run_permissions_host_request_if_requested() {
        std::process::exit(code);
    }
    if let Some(generation) = private_worker::requested_generation() {
        let (initialized_tx, initialized_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let code = match private_worker::run(generation, Some(initialized_tx)) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("cua-driver private worker: {error}");
                    1
                }
            };
            // AppKit's event loop is process-long. This directly supervised
            // child has no reusable endpoint, so protocol completion owns the
            // worker process lifetime.
            std::process::exit(code);
        });
        if initialized_rx.recv().unwrap_or(false) {
            platform_macos::cursor::overlay::run_on_main_thread();
        }
        return;
    }

    // ── CLI subcommand dispatch ──────────────────────────────────────────────
    // Handled before AppKit init so `list-tools` / `describe` / `call` exit
    // cleanly without starting the overlay or NSApplication.
    let command = cli::parse_command();
    match command {
        cli::Command::Telemetry(command) => {
            run_telemetry_command(command);
        }
        cli::Command::ListTools => {
            let tools = inspect_tools_without_runtime();
            cli::run_list_tools(&tools);
        }
        cli::Command::Describe(name) => {
            let tools = inspect_tools_without_runtime();
            cli::run_describe(&tools, &name);
        }
        cli::Command::McpConfig { client } => {
            cli::run_mcp_config(client.as_deref());
        }
        cli::Command::Manifest { pretty } => {
            // Surface 8: machine-readable CLI manifest. Read-only — no
            // registry build needed, no daemon contact.
            cli::run_manifest(pretty);
        }
        cli::Command::Call {
            tool,
            json_args,
            screenshot_out_file,
            socket,
        } => {
            cli::run_call(&tool, json_args, screenshot_out_file, socket);
        }
        cli::Command::Serve {
            socket,
            pid_file,
            permission_mode,
            dangerously_bypass_approvals,
            capability_manifest,
            approve_capability_manifest,
            no_permissions_gate,
            claude_code_compat,
            grants,
            experimental_history,
        } => {
            let cursor_cfg = cursor_overlay::CursorConfig::from_args();
            if let Err(error) = configure_startup_permission_mode(
                permission_mode.as_deref(),
                dangerously_bypass_approvals,
                capability_manifest.as_deref(),
                approve_capability_manifest,
                &grants,
            ) {
                eprintln!("cua-driver: authorization startup error: {error}");
                std::process::exit(64);
            }
            responsibility::reexec_disclaimed_if_needed();
            if let Err(error) = history_runtime::configure_admission(history_admission_requested(
                experimental_history,
                history_runtime::preview_admitted_preference(),
            )) {
                eprintln!("cua-driver: Computer History admission error: {error}");
                std::process::exit(1);
            }
            history_runtime::configure_daemon_launch_state(
                permission_mode.as_deref(),
                dangerously_bypass_approvals,
                capability_manifest.as_deref(),
                approve_capability_manifest,
                no_permissions_gate,
                claude_code_compat,
                cursor_cfg.async_click_feedback,
                &grants,
            );
            let gate_opts =
                platform_macos::permissions::GateOpts::from_env_and_flag(no_permissions_gate);
            // The gate records its episode start in process environment
            // variables. Initialize them here, before the serve thread exists,
            // so the gate never mutates the environment while other threads
            // run. The returned bounded context was only ever used for
            // telemetry, which this build does not have, so it is discarded.
            let _ = platform_macos::permissions::gate::prepare_telemetry_context(gate_opts.opt_out);
            // Fail closed until a fresh helper-process probe completes. This
            // also covers a probe launch failure without letting the serving
            // process perform and cache its own negative TCC preflight.
            serve::set_permission_gate_pending(!gate_opts.opt_out);
            let pip_cfg = match pip_preview::default_config_path() {
                Some(p) => pip_preview::PipConfig::from_args_and_file(&p),
                None => pip_preview::PipConfig::from_args(),
            };
            maybe_init_pip();

            // Agent-cursor overlay. The DAEMON is the process that actually
            // performs clicks / AX presses, so the overlay NSWindow + render
            // loop must run HERE. The MCP proxy never renders, so the daemon
            // owns every cursor command and window. Init the channel before spawning
            // the serve thread so `run_on_main_thread()` always finds it ready.
            // Honour the compat flag forwarded by the MCP proxy
            // (launch_daemon_and_wait passes `serve
            // --claude-code-computer-use-compat`). The Serve arm is the daemon
            // the proxy talks to, so without this the proxy path always served
            // the full screenshot tool regardless of the client's request.
            let driver = match build_driver(
                cursor_cfg.clone(),
                claude_code_compat,
                cua_driver_core::embedded_mode(),
            ) {
                Ok(driver) => driver,
                Err(error) => {
                    eprintln!("cua-driver: cannot create desktop runtime: {error}");
                    std::process::exit(1);
                }
            };
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            let pid_path = serve::pid_file_path_or_default(pid_file);

            // Bind the Unix socket FIRST, on a background thread, BEFORE
            // running the (blocking) permissions gate (#1761).
            //
            // The gate's `wait_for_grants` blocks while `com.meta.musecode.cua.driver`
            // is ungranted. Fresh helper processes poll TCC until the user
            // grants or the deadline elapses. If serve ran after the gate,
            // the daemon's socket wouldn't appear for minutes on first
            // launch, so `permissions grant` / MCP clients launched via
            // `open -n -g -a CuaDriver --args serve` (the correct-TCC-
            // attribution path) couldn't reach the daemon to even report
            // "pending". Binding the socket first makes the daemon
            // reachable within ~1s while the gate works toward the grant.
            //
            // A Unix socket + tokio accept loop has no main-thread
            // requirement, so serve runs on a background thread. The gate
            // stays on the MAIN thread for its NSPanel; short-lived helper
            // processes own prompt and status APIs. The serving process never performs
            // a negative TCC preflight, so its socket and accepted connections
            // remain stable while helper processes refresh permission state.
            let serve_handle = std::thread::Builder::new()
                .name("cua-serve".into())
                .spawn(move || {
                    serve::run_serve_cmd(driver, &sp, Some(&pid_path));
                    std::process::exit(0);
                })
                .expect("spawn serve thread");

            // Socket is binding/bound now → daemon reachable while we gate.
            //
            // First-launch permissions gate (Swift PermissionsGate parity).
            // Runs on every `serve` start; no-op when both grants are
            // already active.  Honors --no-permissions-gate and
            // CUA_DRIVER_RS_PERMISSIONS_GATE=0 for CI / headless.
            //
            let gate_result = platform_macos::permissions::run_if_needed(gate_opts);
            if gate_result.is_ok() {
                serve::set_permission_gate_pending(false);
            }
            if let Err(e) = gate_result {
                eprintln!("[cua-driver] permissions gate: {e}");
                eprintln!(
                    "[cua-driver] desktop tool calls remain gated; grant Accessibility and \
                     Screen Recording permissions, then restart the daemon."
                );
            }

            // Keep one AppKit main loop alive for every enabled observer UI.
            // The cursor renderer owns its own command-draining render pump;
            // choosing the PiP-only loop while cursor support is enabled would
            // leave physical actions waiting forever for cursor arrival. PiP
            // windows use the same NSApplication loop through main-queue
            // callbacks, so the cursor host can service both surfaces. The
            // experimental Return constructor also needs the real main queue
            // for TIS/TSM character readback, but never creates an observer UI.
            use platform_macos::input::return_main_thread;
            match macos_appkit_host(
                cursor_cfg.enabled,
                pip_cfg.enabled,
                return_main_thread::experiment_enabled(),
            ) {
                MacosAppKitHost::CursorOverlay => {
                    if pip_cfg.enabled {
                        platform_macos::pip::prepare_for_shared_appkit_main_loop();
                    }
                    return_main_thread::host_ready_on_main();
                    platform_macos::cursor::overlay::run_on_main_thread();
                    return_main_thread::clear_host_ready_on_main();
                    let _ = serve_handle.join();
                }
                MacosAppKitHost::PipOnly => {
                    return_main_thread::host_ready_on_main();
                    platform_macos::pip::run_appkit_main_loop();
                    return_main_thread::clear_host_ready_on_main();
                }
                MacosAppKitHost::ReturnOnly => {
                    if let Err(error) = return_main_thread::headless_main_loop() {
                        eprintln!("[cua-driver] experimental Return main-thread host: {error}");
                    }
                    let _ = serve_handle.join();
                }
                MacosAppKitHost::None => {
                    let _ = serve_handle.join();
                }
            }
        }
        cli::Command::Stop {
            socket,
            expected_pid,
        } => {
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            match expected_pid {
                Some(pid) => stop::run_pid_bound_stop_cmd(&sp, pid),
                None => serve::run_stop_cmd(&sp),
            }
        }
        cli::Command::Revoke {
            socket,
            session,
            all,
        } => {
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            serve::run_revoke_cmd(&sp, session.as_deref(), all);
        }
        cli::Command::Status { socket, pid_file } => {
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            let pid_path = serve::pid_file_path_or_default(pid_file);
            serve::run_status_cmd(&sp, &pid_path);
        }
        cli::Command::Sessions { json, socket } => {
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            serve::run_sessions_list_cmd(&sp, json);
        }
        cli::Command::Recording {
            subcommand,
            args,
            socket,
        } => {
            cli::run_recording_cmd(&subcommand, &args, socket.as_deref());
        }
        cli::Command::History {
            subcommand,
            args,
            socket,
            json,
            confirmed,
        } => {
            cli::run_history_cmd(&subcommand, &args, socket.as_deref(), json, confirmed);
        }
        cli::Command::DumpDocs { pretty, doc_type } => {
            let tools = inspect_tools_without_runtime();
            cli::run_dump_docs_with_type(&tools, pretty, &doc_type);
        }
        cli::Command::Update { apply, json } => {
            cli::run_update_cmd(apply, json);
        }
        cli::Command::CheckUpdate { json, no_cache } => {
            cli::run_check_update_cmd(json, no_cache);
        }
        cli::Command::Channel {
            subcommand,
            value,
            json,
        } => {
            cli::run_channel_cmd(&subcommand, value.as_deref(), json);
        }
        cli::Command::Doctor { json } => {
            cli::run_doctor_cmd(json);
        }
        cli::Command::Diagnose => {
            cli::run_diagnose_cmd();
        }
        cli::Command::Permissions { subcommand, json } => {
            cli::run_permissions_cmd(&subcommand, json);
        }
        cli::Command::Autostart { subcommand } => {
            autostart::run_autostart_cmd(&subcommand);
        }
        cli::Command::Skills { subcommand, flags } => {
            skills::run(&subcommand, &flags);
        }
        cli::Command::CursorTheme { args } => {
            run_cursor_theme_command(&args);
        }
        cli::Command::Config {
            subcommand,
            key,
            value,
            socket,
        } => {
            cli::run_config_cmd(
                subcommand.as_deref(),
                key.as_deref(),
                value.as_deref(),
                socket.as_deref(),
            );
        }
        cli::Command::Mcp {
            socket,
            direct,
            claude_code_compat,
            grants,
            experimental_pip,
            expected_pid,
        } => {
            let feedback_override = cursor_overlay::CursorConfig::click_feedback_override(
                &std::env::args().skip(1).collect::<Vec<_>>(),
            );
            let result = if expected_pid.is_some() && (direct || socket.is_none()) {
                Err(anyhow::anyhow!(
                    "--expected-pid requires daemon-backed `mcp --socket <path>`"
                ))
            } else {
                match mcp_uses_direct_runtime(socket.as_deref(), direct) {
                    Ok(true) => {
                        configure_startup_permission_mode(None, false, None, false, &grants)
                            .and_then(|()| run_mcp_direct(claude_code_compat))
                    }
                    Err(error) => Err(error),
                    Ok(false) => cli::run_mcp_via_daemon_proxy(
                        socket,
                        expected_pid,
                        claude_code_compat,
                        &grants,
                        experimental_pip,
                        feedback_override,
                    ),
                }
            };
            if let Err(e) = result {
                eprintln!("cua-driver-rs: {e}");
                std::process::exit(1);
            }
        }
    }
}

// ── Non-macOS entry-point ─────────────────────────────────────────────────

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    if let Some(code) = history_runtime::run_offline_purge_if_requested() {
        std::process::exit(code);
    }
    init_logging();
    if let Some(generation) = private_worker::requested_generation() {
        return private_worker::run(generation, None);
    }

    // ── CLI subcommand dispatch ──────────────────────────────────────────────
    // These commands create their own tokio runtimes internally, so they must
    // run on a plain OS thread — not inside a #[tokio::main] context which
    // would cause nested block_on panics.
    let command = cli::parse_command();
    match command {
        cli::Command::Telemetry(command) => {
            run_telemetry_command(command);
            return Ok(());
        }
        cli::Command::ListTools => {
            let tools = inspect_tools_without_runtime();
            cli::run_list_tools(&tools);
            return Ok(());
        }
        cli::Command::Describe(name) => {
            let tools = inspect_tools_without_runtime();
            cli::run_describe(&tools, &name);
            return Ok(());
        }
        cli::Command::McpConfig { client } => {
            cli::run_mcp_config(client.as_deref());
            return Ok(());
        }
        cli::Command::Manifest { pretty } => {
            // Surface 8: machine-readable CLI manifest. Read-only — no
            // registry build needed.
            cli::run_manifest(pretty);
            return Ok(());
        }
        cli::Command::Call {
            tool,
            json_args,
            screenshot_out_file,
            socket,
        } => {
            cli::run_call(&tool, json_args, screenshot_out_file, socket);
            return Ok(());
        }
        cli::Command::Serve {
            socket,
            pid_file,
            permission_mode,
            dangerously_bypass_approvals,
            capability_manifest,
            approve_capability_manifest,
            no_permissions_gate,
            claude_code_compat,
            grants,
            experimental_history,
        } => {
            let cursor_cfg = cursor_overlay::CursorConfig::from_args();
            configure_startup_permission_mode(
                permission_mode.as_deref(),
                dangerously_bypass_approvals,
                capability_manifest.as_deref(),
                approve_capability_manifest,
                &grants,
            )?;
            responsibility::reexec_disclaimed_if_needed();
            history_runtime::configure_admission(history_admission_requested(
                experimental_history,
                history_runtime::preview_admitted_preference(),
            ))?;
            history_runtime::configure_daemon_launch_state(
                permission_mode.as_deref(),
                dangerously_bypass_approvals,
                capability_manifest.as_deref(),
                approve_capability_manifest,
                no_permissions_gate,
                claude_code_compat,
                cursor_cfg.async_click_feedback,
                &grants,
            );
            // The Rust permissions gate is macOS-only (TCC concept).
            // On Windows / Linux the flag is silently accepted for
            // CLI uniformity and ignored. The Claude-Code compat screenshot
            // surface is accepted on every platform for CLI uniformity.
            let _ = no_permissions_gate;
            // Serve mode needs the cursor overlay just like MCP mode.
            let driver = build_driver(
                cursor_cfg,
                claude_code_compat,
                cua_driver_core::embedded_mode(),
            )?;
            maybe_init_pip();
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            let pid_path = serve::pid_file_path_or_default(pid_file);
            // run_serve_cmd builds its own runtime; must run on a fresh thread.
            std::thread::spawn(move || {
                serve::run_serve_cmd(driver, &sp, Some(&pid_path));
            })
            .join()
            .ok();
            return Ok(());
        }
        cli::Command::Stop {
            socket,
            expected_pid,
        } => {
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            match expected_pid {
                Some(pid) => stop::run_pid_bound_stop_cmd(&sp, pid),
                None => serve::run_stop_cmd(&sp),
            }
            return Ok(());
        }
        cli::Command::Revoke {
            socket,
            session,
            all,
        } => {
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            serve::run_revoke_cmd(&sp, session.as_deref(), all);
            return Ok(());
        }
        cli::Command::Status { socket, pid_file } => {
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            let pid_path = serve::pid_file_path_or_default(pid_file);
            serve::run_status_cmd(&sp, &pid_path);
            return Ok(());
        }
        cli::Command::Sessions { json, socket } => {
            let sp = socket.unwrap_or_else(serve::default_socket_path);
            serve::run_sessions_list_cmd(&sp, json);
            return Ok(());
        }
        cli::Command::Recording {
            subcommand,
            args,
            socket,
        } => {
            cli::run_recording_cmd(&subcommand, &args, socket.as_deref());
            return Ok(());
        }
        cli::Command::History {
            subcommand,
            args,
            socket,
            json,
            confirmed,
        } => {
            cli::run_history_cmd(&subcommand, &args, socket.as_deref(), json, confirmed);
            return Ok(());
        }
        cli::Command::DumpDocs { pretty, doc_type } => {
            let tools = inspect_tools_without_runtime();
            cli::run_dump_docs_with_type(&tools, pretty, &doc_type);
            return Ok(());
        }
        cli::Command::Update { apply, json } => {
            cli::run_update_cmd(apply, json);
            return Ok(());
        }
        cli::Command::CheckUpdate { json, no_cache } => {
            cli::run_check_update_cmd(json, no_cache);
            return Ok(());
        }
        cli::Command::Channel {
            subcommand,
            value,
            json,
        } => {
            cli::run_channel_cmd(&subcommand, value.as_deref(), json);
            return Ok(());
        }
        cli::Command::Doctor { json } => {
            cli::run_doctor_cmd(json);
            return Ok(());
        }
        cli::Command::Diagnose => {
            cli::run_diagnose_cmd();
            return Ok(());
        }
        cli::Command::Permissions { subcommand, json } => {
            cli::run_permissions_cmd(&subcommand, json);
            return Ok(());
        }
        cli::Command::Autostart { subcommand } => {
            autostart::run_autostart_cmd(&subcommand);
            return Ok(());
        }
        cli::Command::Skills { subcommand, flags } => {
            skills::run(&subcommand, &flags);
            return Ok(());
        }
        cli::Command::CursorTheme { args } => {
            run_cursor_theme_command(&args);
        }
        cli::Command::Config {
            subcommand,
            key,
            value,
            socket,
        } => {
            cli::run_config_cmd(
                subcommand.as_deref(),
                key.as_deref(),
                value.as_deref(),
                socket.as_deref(),
            );
            return Ok(());
        }
        cli::Command::Mcp {
            socket,
            direct,
            claude_code_compat,
            grants,
            experimental_pip,
            expected_pid,
        } => {
            let feedback_override = cursor_overlay::CursorConfig::click_feedback_override(
                &std::env::args().skip(1).collect::<Vec<_>>(),
            );
            let result = if expected_pid.is_some() && (direct || socket.is_none()) {
                Err(anyhow::anyhow!(
                    "--expected-pid requires daemon-backed `mcp --socket <path>`"
                ))
            } else {
                match mcp_uses_direct_runtime(socket.as_deref(), direct) {
                    Ok(true) => {
                        configure_startup_permission_mode(None, false, None, false, &grants)?;
                        run_mcp_direct(claude_code_compat)
                    }
                    Err(error) => Err(error),
                    Ok(false) => cli::run_mcp_via_daemon_proxy(
                        socket,
                        expected_pid,
                        claude_code_compat,
                        &grants,
                        experimental_pip,
                        feedback_override,
                    ),
                }
            };
            if let Err(e) = result {
                eprintln!("cua-driver-rs: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }
    }
}
