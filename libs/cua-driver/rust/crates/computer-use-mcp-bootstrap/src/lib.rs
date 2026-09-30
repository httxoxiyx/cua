//! Persistent MCP bootstrap for the Computer Use plugin.
//!
//! The process exposes the plugin's fixed public tool catalog before the
//! downloadable runtime exists. The first valid public tool call starts one
//! trusted setup command and returns a structured pending result immediately.
//! Once setup reports verified readiness and exits successfully, this process
//! starts the guarded Python wrapper, initializes and attests its tool catalog,
//! and forwards later tool calls without replacing the client MCP connection.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const SETUP_STATUS_SCHEMA_VERSION: u64 = 1;
const SETUP_PENDING_CODE: &str = "computer_use_setup_pending";
const SETUP_READY_CODE: &str = "computer_use_setup_ready";
const SETUP_FAILED_CODE: &str = "computer_use_setup_failed";
const INTERNAL_INITIALIZE_ID: &str = "computer-use-bootstrap/internal/initialize";
const INTERNAL_TOOLS_LIST_ID: &str = "computer-use-bootstrap/internal/tools-list";
const BACKEND_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(45);
const BACKEND_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const AUTONOMOUS_CALL_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
// The marketplace setup relay reserves up to 20 seconds to stop an exact
// daemon it launched. Keep the outer process-group supervisor alive beyond
// that inner budget before escalating to SIGKILL.
const CHILD_TERMINATION_GRACE: Duration = Duration::from_secs(25);
const PUBLIC_INITIALIZE_INSTRUCTIONS: &str = include_str!("initialize_instructions.txt");

#[cfg(unix)]
static RECEIVED_SIGNAL: AtomicI32 = AtomicI32::new(0);

pub const USAGE: &str = concat!(
    "Usage: computer-use-mcp-bootstrap --plugin-version VERSION\n",
    "  --setup-program ABS [--setup-arg ARG]...\n",
    "  --backend-program ABS [--backend-arg ARG]...",
);

const PUBLIC_TOOL_NAMES: [&str; 11] = [
    "list_apps",
    "launch_app",
    "get_app_state",
    "click",
    "perform_secondary_action",
    "scroll",
    "drag",
    "type_text",
    "press_key",
    "set_value",
    "batch_actions",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub plugin_version: String,
    pub setup: CommandSpec,
    pub backend: CommandSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandLine {
    Run(Config),
    Help,
    Version,
    BuildAttestation,
}

impl CommandLine {
    pub fn parse<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut args = args.into_iter();
        let mut plugin_version = None;
        let mut setup_program = None;
        let mut setup_args = Vec::new();
        let mut backend_program = None;
        let mut backend_args = Vec::new();
        let mut saw_any = false;

        while let Some(argument) = args.next() {
            saw_any = true;
            match argument.to_str() {
                Some("--help") | Some("-h") => {
                    if plugin_version.is_some()
                        || setup_program.is_some()
                        || backend_program.is_some()
                        || !setup_args.is_empty()
                        || !backend_args.is_empty()
                        || args.next().is_some()
                    {
                        return Err("--help must be used alone".into());
                    }
                    return Ok(Self::Help);
                }
                Some("--version") | Some("-V") => {
                    if plugin_version.is_some()
                        || setup_program.is_some()
                        || backend_program.is_some()
                        || !setup_args.is_empty()
                        || !backend_args.is_empty()
                        || args.next().is_some()
                    {
                        return Err("--version must be used alone".into());
                    }
                    return Ok(Self::Version);
                }
                Some("__build-attestation") => {
                    if plugin_version.is_some()
                        || setup_program.is_some()
                        || backend_program.is_some()
                        || !setup_args.is_empty()
                        || !backend_args.is_empty()
                        || args.next().is_some()
                    {
                        return Err("__build-attestation must be used alone".into());
                    }
                    return Ok(Self::BuildAttestation);
                }
                Some("--plugin-version") => {
                    let value = next_utf8(&mut args, "--plugin-version")?;
                    if plugin_version.replace(value).is_some() {
                        return Err("--plugin-version may be supplied only once".into());
                    }
                }
                Some("--setup-program") => {
                    let value = next_os(&mut args, "--setup-program")?;
                    if setup_program.replace(PathBuf::from(value)).is_some() {
                        return Err("--setup-program may be supplied only once".into());
                    }
                }
                Some("--setup-arg") => setup_args.push(next_os(&mut args, "--setup-arg")?),
                Some("--backend-program") => {
                    let value = next_os(&mut args, "--backend-program")?;
                    if backend_program.replace(PathBuf::from(value)).is_some() {
                        return Err("--backend-program may be supplied only once".into());
                    }
                }
                Some("--backend-arg") => backend_args.push(next_os(&mut args, "--backend-arg")?),
                Some(other) => return Err(format!("unknown argument: {other}")),
                None => return Err("arguments must be valid UTF-8 option names".into()),
            }
        }

        if !saw_any {
            return Err("missing required arguments".into());
        }
        let plugin_version = plugin_version.ok_or("missing --plugin-version")?;
        if !valid_version(&plugin_version) {
            return Err("--plugin-version must match [0-9][A-Za-z0-9.+-]*".into());
        }
        let setup_program = setup_program.ok_or("missing --setup-program")?;
        let backend_program = backend_program.ok_or("missing --backend-program")?;
        if !setup_program.is_absolute() || !backend_program.is_absolute() {
            return Err("setup and backend programs must be absolute paths".into());
        }

        Ok(Self::Run(Config {
            plugin_version,
            setup: CommandSpec {
                program: setup_program,
                args: setup_args,
            },
            backend: CommandSpec {
                program: backend_program,
                args: backend_args,
            },
        }))
    }
}

/// Immutable build provenance consumed by the downstream signed-plugin
/// assembler. A production artifact is rejected unless both stamped values
/// match its reviewed dependency manifest exactly.
pub fn build_attestation() -> Value {
    json!({
        "schema_version": 1,
        "binary_version": option_env!("CUA_DRIVER_RELEASE_VERSION")
            .unwrap_or(env!("CARGO_PKG_VERSION")),
        "source_sha": option_env!("CUA_DRIVER_SOURCE_SHA"),
    })
}

fn next_os<I>(args: &mut I, option: &str) -> Result<OsString, String>
where
    I: Iterator<Item = OsString>,
{
    args.next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn next_utf8<I>(args: &mut I, option: &str) -> Result<String, String>
where
    I: Iterator<Item = OsString>,
{
    next_os(args, option)?
        .into_string()
        .map_err(|_| format!("{option} must be valid UTF-8"))
}

fn valid_version(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'0'..=b'9'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
}

pub fn public_tools() -> Value {
    let tools: Value = serde_json::from_str(include_str!("tool_catalog.json"))
        .expect("embedded Computer Use tool catalog must be valid JSON");
    validate_tool_catalog(&tools).expect("embedded Computer Use tool catalog must be canonical");
    tools
}

fn validate_tool_catalog(tools: &Value) -> Result<(), String> {
    let tools = tools
        .as_array()
        .ok_or("Computer Use tool catalog is not an array")?;
    let names = tools
        .iter()
        .map(|tool| {
            let object = tool.as_object().ok_or("tool definition is not an object")?;
            if object.get("description").and_then(Value::as_str).is_none()
                || !object.get("inputSchema").is_some_and(Value::is_object)
                || !object.get("annotations").is_some_and(Value::is_object)
            {
                return Err("tool definition is incomplete".to_owned());
            }
            object
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "tool definition has no name".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if names != PUBLIC_TOOL_NAMES {
        return Err("Computer Use tool roster differs from the fixed public contract".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupStage {
    Installing,
    Accessibility,
    ScreenRecordingRegistration,
    ScreenRecording,
    DriverRestarting,
    TccPropagation,
    CaptureVerification,
    ServiceStarting,
}

impl SetupStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Installing => "installing",
            Self::Accessibility => "accessibility",
            Self::ScreenRecordingRegistration => "screen_recording_registration",
            Self::ScreenRecording => "screen_recording",
            Self::DriverRestarting => "driver_restarting",
            Self::TccPropagation => "tcc_propagation",
            Self::CaptureVerification => "capture_verification",
            Self::ServiceStarting => "service_starting",
        }
    }

    fn requires_user_action(self) -> bool {
        matches!(self, Self::Accessibility | Self::ScreenRecording)
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "installing" => Some(Self::Installing),
            "accessibility" => Some(Self::Accessibility),
            "screen_recording_registration" => Some(Self::ScreenRecordingRegistration),
            "screen_recording" => Some(Self::ScreenRecording),
            "driver_restarting" => Some(Self::DriverRestarting),
            "tcc_propagation" => Some(Self::TccPropagation),
            "capture_verification" => Some(Self::CaptureVerification),
            "service_starting" => Some(Self::ServiceStarting),
            _ => None,
        }
    }
}

fn setup_stage_transition_allowed(previous: SetupStage, next: SetupStage) -> bool {
    use SetupStage::*;

    // The native onboarding supervisor publishes progress through a latest-value
    // status file, so an intermediate capture-verification event can be missed.
    // ServiceStarting still carries the independently validated, fully-ready
    // permission state, making it a safe handoff from any pre-service stage.
    if next == ServiceStarting {
        return true;
    }

    match previous {
        Installing => true,
        Accessibility => matches!(
            next,
            Accessibility
                | ScreenRecordingRegistration
                | ScreenRecording
                | DriverRestarting
                | TccPropagation
                | CaptureVerification
        ),
        ScreenRecordingRegistration => matches!(
            next,
            ScreenRecordingRegistration
                | ScreenRecording
                | DriverRestarting
                | TccPropagation
                | CaptureVerification
        ),
        ScreenRecording => matches!(
            next,
            ScreenRecording | DriverRestarting | TccPropagation | CaptureVerification
        ),
        DriverRestarting => matches!(
            next,
            DriverRestarting
                | TccPropagation
                | Accessibility
                | ScreenRecordingRegistration
                | ScreenRecording
                | CaptureVerification
        ),
        TccPropagation => matches!(
            next,
            TccPropagation
                | DriverRestarting
                | Accessibility
                | ScreenRecordingRegistration
                | ScreenRecording
                | CaptureVerification
        ),
        CaptureVerification => matches!(next, CaptureVerification | DriverRestarting),
        ServiceStarting => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SetupSignal {
    Pending(SetupStage),
    Ready,
    Failed { detail: String, retryable: bool },
    Diagnostic,
}

fn parse_setup_signal(line: &str) -> Result<SetupSignal, String> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(SetupSignal::Diagnostic);
    };
    let Some(object) = value.as_object() else {
        return Ok(SetupSignal::Diagnostic);
    };
    if !object.contains_key("code") && !object.contains_key("schema_version") {
        return Ok(SetupSignal::Diagnostic);
    }
    if object.get("schema_version").and_then(Value::as_u64) != Some(SETUP_STATUS_SCHEMA_VERSION) {
        return Err("setup emitted an unsupported status schema".into());
    }
    let code = object
        .get("code")
        .and_then(Value::as_str)
        .ok_or("setup status has no code")?;
    let stage = object
        .get("stage")
        .and_then(Value::as_str)
        .ok_or("setup status has no stage")?;
    let retryable = object
        .get("retryable")
        .and_then(Value::as_bool)
        .ok_or("setup status has no retryable flag")?;
    let requires_user_action = object
        .get("requires_user_action")
        .and_then(Value::as_bool)
        .ok_or("setup status has no requires_user_action flag")?;

    match code {
        SETUP_PENDING_CODE => {
            let stage = SetupStage::parse(stage).ok_or("setup pending stage is invalid")?;
            if !retryable {
                return Err("setup pending status must be retryable".into());
            }
            if requires_user_action != stage.requires_user_action() {
                return Err("setup pending action policy disagrees with its stage".into());
            }
            let state = (
                object.get("accessibility").and_then(Value::as_bool),
                object.get("screen_recording").and_then(Value::as_bool),
                object
                    .get("screen_recording_capturable")
                    .and_then(Value::as_bool),
                object.get("screen_recording_capturable") == Some(&Value::Null),
            );
            let valid_state = match stage {
                SetupStage::Installing => true,
                SetupStage::Accessibility => matches!(
                    state,
                    (Some(false), Some(false), None, true) | (Some(false), Some(true), None, true)
                ),
                SetupStage::ScreenRecordingRegistration => {
                    state == (Some(true), Some(false), None, true)
                }
                SetupStage::ScreenRecording => state == (Some(true), Some(false), None, true),
                SetupStage::DriverRestarting => {
                    matches!(
                        state,
                        (Some(false), Some(false), None, true)
                            | (Some(false), Some(true), None, true)
                            | (Some(true), Some(false), None, true)
                            | (Some(true), Some(true), None, true)
                    )
                }
                SetupStage::TccPropagation => matches!(
                    state,
                    (Some(false), Some(false), None, true)
                        | (Some(false), Some(true), None, true)
                        | (Some(true), Some(false), None, true)
                ),
                SetupStage::CaptureVerification => state == (Some(true), Some(true), None, true),
                SetupStage::ServiceStarting => state == (Some(true), Some(true), Some(true), false),
            };
            if !valid_state {
                return Err("setup pending permission state disagrees with its stage".into());
            }
            Ok(SetupSignal::Pending(stage))
        }
        SETUP_READY_CODE => {
            if stage != "ready" || retryable || requires_user_action {
                return Err("setup ready status has invalid terminal fields".into());
            }
            if object.get("accessibility") != Some(&Value::Bool(true))
                || object.get("screen_recording") != Some(&Value::Bool(true))
                || object.get("screen_recording_capturable") != Some(&Value::Bool(true))
            {
                return Err("setup reported ready without all verified permissions".into());
            }
            Ok(SetupSignal::Ready)
        }
        SETUP_FAILED_CODE => {
            if stage != "failed" || requires_user_action {
                return Err("setup failed status has invalid terminal fields".into());
            }
            let detail = object
                .get("error")
                .and_then(Value::as_object)
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                .map(bounded_diagnostic)
                .unwrap_or_else(|| "setup_reported_failure".into());
            Ok(SetupSignal::Failed { detail, retryable })
        }
        _ => Err("setup emitted an unknown status code".into()),
    }
}

fn bounded_diagnostic(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect()
}

fn public_failure_reason(detail: &str) -> String {
    let bounded = bounded_diagnostic(detail);
    if !bounded.is_empty()
        && bounded.len() <= 128
        && bounded
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        bounded
    } else {
        "setup_internal_failure".into()
    }
}

struct SetupProcess {
    generation: u64,
    child: Child,
    stage: SetupStage,
    ready_seen: bool,
    reported_failure: Option<(String, bool)>,
    stdout_closed: bool,
    exit_status: Option<ExitStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendPhase {
    Initialize,
    Catalog,
    Ready,
}

struct BackendProcess {
    generation: u64,
    child: Child,
    stdin: ChildStdin,
    phase: BackendPhase,
    started_at: Instant,
    stdout_closed: bool,
    exit_status: Option<ExitStatus>,
    pending_request_ids: Vec<Value>,
    cancelled_request_ids: Vec<Value>,
    restart_on_retirement: bool,
    drain: Option<BackendDrain>,
}

struct BackendDrain {
    detail: String,
    retryable: bool,
    expired: bool,
}

struct Failure {
    reason: String,
    retryable: bool,
    report_on_next_call: bool,
}

struct DeferredToolCall {
    raw: String,
    id: Value,
    deadline: Instant,
}

enum RuntimeState {
    Dormant,
    Setup(SetupProcess),
    Backend(BackendProcess),
    Failed(Failure),
}

enum Event {
    ClientLine(String),
    ClientEof,
    ClientReadError(String),
    SetupLine { generation: u64, line: String },
    SetupOutputClosed { generation: u64 },
    BackendLine { generation: u64, line: String },
    BackendOutputClosed { generation: u64 },
    BackendDrainExpired { generation: u64 },
}

struct Server {
    config: Config,
    tools: Value,
    initialized: bool,
    initialize_params: Option<Value>,
    runtime: RuntimeState,
    deferred_tool_calls: Vec<DeferredToolCall>,
    next_generation: u64,
    stopping: bool,
}

impl Server {
    fn new(config: Config) -> Self {
        Self {
            config,
            tools: public_tools(),
            initialized: false,
            initialize_params: None,
            runtime: RuntimeState::Dormant,
            deferred_tool_calls: Vec::new(),
            next_generation: 1,
            stopping: false,
        }
    }

    fn next_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        generation
    }

    fn handle_client_line(
        &mut self,
        raw: String,
        events: &Sender<Event>,
        output: &mut dyn Write,
    ) -> io::Result<()> {
        let request: Value = match serde_json::from_str(raw.trim()) {
            Ok(value) => value,
            Err(_) => return write_json(output, &rpc_error(Value::Null, -32700, "Parse error")),
        };
        let Some(object) = request.as_object() else {
            return write_json(output, &rpc_error(Value::Null, -32600, "Invalid Request"));
        };
        let has_id = object.contains_key("id");
        let id = object.get("id").cloned().unwrap_or(Value::Null);
        let method = object.get("method").and_then(Value::as_str);
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_none() {
            if has_id {
                write_json(output, &rpc_error(id, -32600, "Invalid Request"))?;
            }
            return Ok(());
        }
        let method = method.expect("checked above");

        if !has_id {
            match method {
                "notifications/initialized" => {}
                "notifications/cancelled" => {
                    let request_id = object
                        .get("params")
                        .and_then(Value::as_object)
                        .and_then(|params| params.get("requestId"))
                        .cloned();
                    self.handle_cancellation(raw, request_id, events)?;
                }
                _ if self.backend_ready() => self.forward_to_backend(raw, None, events)?,
                _ => {}
            }
            return Ok(());
        }

        match method {
            "initialize" => {
                let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
                if !params.is_object() {
                    return write_json(output, &rpc_error(id, -32602, "invalid initialize params"));
                }
                if self.initialize_params.is_none() {
                    self.initialize_params = Some(params);
                }
                self.initialized = true;
                write_json(output, &rpc_result(id, self.initialize_result()))
            }
            "ping" => write_json(output, &rpc_result(id, json!({}))),
            "logging/setLevel" => {
                let valid = object
                    .get("params")
                    .and_then(|params| params.get("level"))
                    .and_then(Value::as_str)
                    .is_some_and(|level| {
                        matches!(
                            level,
                            "debug"
                                | "info"
                                | "notice"
                                | "warning"
                                | "error"
                                | "critical"
                                | "alert"
                                | "emergency"
                        )
                    });
                if valid {
                    write_json(output, &rpc_result(id, json!({})))
                } else {
                    write_json(output, &rpc_error(id, -32602, "invalid logging level"))
                }
            }
            "tools/list" => {
                if !self.initialized {
                    write_json(output, &rpc_error(id, -32002, "server not initialized"))
                } else {
                    write_json(
                        output,
                        &rpc_result(id, json!({"tools": self.tools.clone()})),
                    )
                }
            }
            "tools/call" => {
                if !self.initialized {
                    return write_json(output, &rpc_error(id, -32002, "server not initialized"));
                }
                let Some(name) = object
                    .get("params")
                    .and_then(Value::as_object)
                    .and_then(|params| params.get("name"))
                    .and_then(Value::as_str)
                else {
                    return write_json(output, &rpc_error(id, -32602, "invalid tools/call params"));
                };
                if !PUBLIC_TOOL_NAMES.contains(&name) {
                    return write_json(output, &rpc_result(id, unsupported_tool_result(name)));
                }
                match &self.runtime {
                    RuntimeState::Dormant => match self.start_setup(events) {
                        Ok(()) => write_json(
                            output,
                            &rpc_result(id, setup_pending_result(SetupStage::Installing)),
                        ),
                        Err(error) => {
                            self.fail(error, false, false);
                            write_json(
                                output,
                                &rpc_result(id, setup_failed_result(false, "setup_start_failed")),
                            )
                        }
                    },
                    RuntimeState::Setup(setup) if !setup.stage.requires_user_action() => {
                        self.defer_tool_call(raw, id);
                        Ok(())
                    }
                    RuntimeState::Setup(setup) => {
                        write_json(output, &rpc_result(id, setup_pending_result(setup.stage)))
                    }
                    RuntimeState::Backend(backend)
                        if backend.phase != BackendPhase::Ready || backend.drain.is_some() =>
                    {
                        self.defer_tool_call(raw, id);
                        Ok(())
                    }
                    RuntimeState::Backend(_) => self.forward_to_backend(raw, Some(id), events),
                    RuntimeState::Failed(failure) => {
                        let retryable = failure.retryable;
                        let report_on_next_call = failure.report_on_next_call;
                        let reason = failure.reason.clone();
                        if retryable && !report_on_next_call {
                            self.runtime = RuntimeState::Dormant;
                            match self.start_setup(events) {
                                Ok(()) => write_json(
                                    output,
                                    &rpc_result(id, setup_pending_result(SetupStage::Installing)),
                                ),
                                Err(error) => {
                                    self.fail(error, false, false);
                                    write_json(
                                        output,
                                        &rpc_result(
                                            id,
                                            setup_failed_result(false, "setup_start_failed"),
                                        ),
                                    )
                                }
                            }
                        } else {
                            if retryable {
                                self.runtime = RuntimeState::Dormant;
                            }
                            write_json(
                                output,
                                &rpc_result(id, setup_failed_result(retryable, &reason)),
                            )
                        }
                    }
                }
            }
            "shutdown" => {
                self.stopping = true;
                write_json(output, &rpc_result(id, Value::Null))
            }
            _ => write_json(output, &rpc_error(id, -32601, "Method not found")),
        }
    }

    fn initialize_result(&self) -> Value {
        let instructions = format!(
            "Computer Use setup starts on the first tool call. Pending results declare `requires_user_action`: complete a visible macOS step only when it is true. Autonomous installation, restart, propagation, capture verification, and service-starting stages continue in this MCP session.\n\n{}",
            PUBLIC_INITIALIZE_INSTRUCTIONS.trim_end()
        );
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"tools": {}, "logging": {}},
            "serverInfo": {
                "name": "tbh-computer-use",
                "version": self.config.plugin_version,
            },
            "instructions": instructions
        })
    }

    fn backend_ready(&self) -> bool {
        matches!(
            &self.runtime,
            RuntimeState::Backend(BackendProcess {
                phase: BackendPhase::Ready,
                drain: None,
                ..
            })
        )
    }

    fn defer_tool_call(&mut self, raw: String, id: Value) {
        self.deferred_tool_calls.push(DeferredToolCall {
            raw,
            id,
            deadline: Instant::now() + AUTONOMOUS_CALL_WAIT_TIMEOUT,
        });
    }

    fn current_pending_stage(&self) -> Option<SetupStage> {
        match &self.runtime {
            RuntimeState::Setup(setup) => Some(setup.stage),
            RuntimeState::Backend(backend)
                if backend.phase != BackendPhase::Ready || backend.drain.is_some() =>
            {
                Some(SetupStage::ServiceStarting)
            }
            _ => None,
        }
    }

    fn progress_deferred_tool_calls(
        &mut self,
        events: &Sender<Event>,
        output: &mut dyn Write,
    ) -> io::Result<()> {
        if self.deferred_tool_calls.is_empty() {
            return Ok(());
        }
        if matches!(
            &self.runtime,
            RuntimeState::Backend(BackendProcess { drain: Some(_), .. })
        ) {
            // Drain completion decides whether these never-written calls fail
            // with the retired transport or continue on its replacement.
            return Ok(());
        }
        if self.backend_ready() {
            let mut calls = VecDeque::from(std::mem::take(&mut self.deferred_tool_calls));
            while let Some(call) = calls.pop_front() {
                // Keep every not-yet-attempted call visible to terminal drain
                // handling. A closed backend pipe settles already-forwarded
                // IDs and either fails or replays only the untouched tail.
                self.deferred_tool_calls = calls.into();
                self.forward_to_backend(call.raw, Some(call.id), events)?;
                if !self.backend_ready() {
                    return Ok(());
                }
                calls = VecDeque::from(std::mem::take(&mut self.deferred_tool_calls));
            }
            return Ok(());
        }
        let Some(stage) = self.current_pending_stage() else {
            return Ok(());
        };
        let now = Instant::now();
        let mut still_waiting = Vec::new();
        let mut respond = Vec::new();
        for call in std::mem::take(&mut self.deferred_tool_calls) {
            if stage.requires_user_action() || now >= call.deadline {
                respond.push(call);
            } else {
                still_waiting.push(call);
            }
        }
        self.deferred_tool_calls = still_waiting;
        for call in respond {
            write_json(output, &rpc_result(call.id, setup_pending_result(stage)))?;
        }
        Ok(())
    }

    fn handle_cancellation(
        &mut self,
        raw: String,
        request_id: Option<Value>,
        events: &Sender<Event>,
    ) -> io::Result<()> {
        let cancelled_deferred = request_id.as_ref().is_some_and(|request_id| {
            let before = self.deferred_tool_calls.len();
            self.deferred_tool_calls
                .retain(|call| &call.id != request_id);
            self.deferred_tool_calls.len() != before
        });
        // Setup and backend initialization are shared session work. Cancelling
        // one deferred request only withdraws that request; it must not tear
        // down work needed by other or future calls.
        if cancelled_deferred {
            return Ok(());
        }
        match &mut self.runtime {
            RuntimeState::Setup(_) => Ok(()),
            RuntimeState::Backend(backend) if backend.phase == BackendPhase::Ready => {
                let can_forward = backend.drain.is_none();
                let cancelled_request = request_id.as_ref().and_then(|request_id| {
                    backend
                        .pending_request_ids
                        .iter()
                        .position(|pending| pending == request_id)
                        .map(|position| {
                            backend.pending_request_ids.remove(position);
                            request_id.clone()
                        })
                });
                let Some(cancelled_request) = cancelled_request else {
                    return Ok(());
                };
                // The backend may have committed a response before it reads
                // this cancellation. Retain the ID until that response arrives
                // so it can be discarded without poisoning the session.
                backend.cancelled_request_ids.push(cancelled_request);
                backend.restart_on_retirement = true;
                if can_forward {
                    self.forward_to_backend(raw, None, events)
                } else {
                    Ok(())
                }
            }
            RuntimeState::Backend(_) => Ok(()),
            RuntimeState::Dormant | RuntimeState::Failed(_) => Ok(()),
        }
    }

    fn start_setup(&mut self, events: &Sender<Event>) -> Result<(), String> {
        if !matches!(self.runtime, RuntimeState::Dormant) {
            return Err("setup start requested after runtime transition".into());
        }
        let generation = self.next_generation();
        let mut command = verified_command(&self.config.setup)?;
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("could not start setup: {error}"))?;
        let Some(stdout) = child.stdout.take() else {
            terminate_child(&mut child, None);
            return Err("setup stdout was not captured".into());
        };
        spawn_line_reader(
            stdout,
            events.clone(),
            move |line| Event::SetupLine { generation, line },
            move || Event::SetupOutputClosed { generation },
        );
        self.runtime = RuntimeState::Setup(SetupProcess {
            generation,
            child,
            stage: SetupStage::Installing,
            ready_seen: false,
            reported_failure: None,
            stdout_closed: false,
            exit_status: None,
        });
        Ok(())
    }

    fn start_backend(&mut self, events: &Sender<Event>) -> Result<(), String> {
        let generation = self.next_generation();
        let mut command = verified_command(&self.config.backend)?;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("could not start Computer Use backend: {error}"))?;
        let Some(mut stdin) = child.stdin.take() else {
            terminate_child(&mut child, None);
            return Err("backend stdin was not captured".into());
        };
        let Some(stdout) = child.stdout.take() else {
            drop(stdin);
            terminate_child(&mut child, None);
            return Err("backend stdout was not captured".into());
        };
        spawn_line_reader(
            stdout,
            events.clone(),
            move |line| Event::BackendLine { generation, line },
            move || Event::BackendOutputClosed { generation },
        );

        let initialize = json!({
            "jsonrpc": "2.0",
            "id": INTERNAL_INITIALIZE_ID,
            "method": "initialize",
            "params": self.backend_initialize_params(),
        });
        if let Err(error) = write_child_json(&mut stdin, &initialize) {
            drop(stdin);
            terminate_child(&mut child, None);
            return Err(format!(
                "could not initialize Computer Use backend: {error}"
            ));
        }
        self.runtime = RuntimeState::Backend(BackendProcess {
            generation,
            child,
            stdin,
            phase: BackendPhase::Initialize,
            started_at: Instant::now(),
            stdout_closed: false,
            exit_status: None,
            pending_request_ids: Vec::new(),
            cancelled_request_ids: Vec::new(),
            restart_on_retirement: false,
            drain: None,
        });
        Ok(())
    }

    fn backend_initialize_params(&self) -> Value {
        let mut params = self.initialize_params.clone().unwrap_or_else(|| json!({}));
        params
            .as_object_mut()
            .expect("initialize params are validated before storage")
            .insert(
                "protocolVersion".into(),
                Value::String(MCP_PROTOCOL_VERSION.into()),
            );
        params
    }

    fn handle_event(
        &mut self,
        event: Event,
        events: &Sender<Event>,
        output: &mut dyn Write,
    ) -> io::Result<()> {
        match event {
            Event::ClientLine(line) => self.handle_client_line(line, events, output),
            Event::ClientEof => {
                self.stopping = true;
                Ok(())
            }
            Event::ClientReadError(error) => {
                self.stopping = true;
                Err(io::Error::other(error))
            }
            Event::SetupLine { generation, line } => {
                let RuntimeState::Setup(setup) = &mut self.runtime else {
                    return Ok(());
                };
                if setup.generation != generation {
                    return Ok(());
                }
                match parse_setup_signal(&line) {
                    Ok(SetupSignal::Pending(stage)) => {
                        if setup_stage_transition_allowed(setup.stage, stage) {
                            setup.stage = stage;
                        } else {
                            eprintln!(
                                "computer-use setup: rejected stage transition {} -> {}",
                                setup.stage.as_str(),
                                stage.as_str()
                            );
                            setup.reported_failure =
                                Some(("setup_stage_transition_invalid".into(), false));
                        }
                    }
                    Ok(SetupSignal::Ready) => {
                        setup.stage = SetupStage::ServiceStarting;
                        setup.ready_seen = true;
                    }
                    Ok(SetupSignal::Failed { detail, retryable }) => {
                        setup.reported_failure = Some((detail, retryable))
                    }
                    Ok(SetupSignal::Diagnostic) => {
                        let diagnostic = bounded_diagnostic(&line);
                        if !diagnostic.is_empty() {
                            eprintln!("computer-use setup: {diagnostic}");
                        }
                    }
                    Err(error) => setup.reported_failure = Some((error, false)),
                }
                Ok(())
            }
            Event::SetupOutputClosed { generation } => {
                if let RuntimeState::Setup(setup) = &mut self.runtime {
                    if setup.generation == generation {
                        setup.stdout_closed = true;
                    }
                }
                Ok(())
            }
            Event::BackendLine { generation, line } => {
                self.handle_backend_line(generation, line, output)
            }
            Event::BackendOutputClosed { generation } => {
                if let RuntimeState::Backend(backend) = &mut self.runtime {
                    if backend.generation == generation {
                        backend.stdout_closed = true;
                    }
                }
                Ok(())
            }
            Event::BackendDrainExpired { generation } => {
                if let RuntimeState::Backend(backend) = &mut self.runtime {
                    if backend.generation == generation {
                        if let Some(drain) = backend.drain.as_mut() {
                            drain.expired = true;
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn handle_backend_line(
        &mut self,
        generation: u64,
        line: String,
        output: &mut dyn Write,
    ) -> io::Result<()> {
        let Some(active_phase) = (match &self.runtime {
            RuntimeState::Backend(backend) if backend.generation == generation => {
                Some(backend.phase)
            }
            _ => None,
        }) else {
            return Ok(());
        };
        let parsed = match serde_json::from_str::<Value>(line.trim()) {
            Ok(value)
                if value.is_object()
                    && value.get("jsonrpc").and_then(Value::as_str) == Some("2.0") =>
            {
                value
            }
            _ => {
                return self.fail_backend(
                    "Computer Use backend emitted invalid JSON-RPC".into(),
                    false,
                    output,
                );
            }
        };
        if active_phase == BackendPhase::Ready {
            let object = parsed.as_object().expect("validated object");
            if object.contains_key("id") {
                let has_result = object.contains_key("result");
                let has_error = object.contains_key("error");
                if has_result == has_error {
                    return self.fail_backend(
                        "Computer Use backend emitted an invalid response envelope".into(),
                        false,
                        output,
                    );
                }
                let id = object.get("id").expect("checked above");
                let (recognized, cancelled) =
                    if let RuntimeState::Backend(backend) = &mut self.runtime {
                        let recognized = backend
                            .pending_request_ids
                            .iter()
                            .position(|pending| pending == id)
                            .map(|position| {
                                backend.pending_request_ids.remove(position);
                            })
                            .is_some();
                        let cancelled = if recognized {
                            false
                        } else {
                            backend
                                .cancelled_request_ids
                                .iter()
                                .position(|cancelled| cancelled == id)
                                .map(|position| {
                                    backend.cancelled_request_ids.remove(position);
                                })
                                .is_some()
                        };
                        (recognized, cancelled)
                    } else {
                        (false, false)
                    };
                if cancelled {
                    return Ok(());
                }
                if !recognized {
                    return self.fail_backend(
                        "Computer Use backend emitted a response for an unknown or completed request"
                            .into(),
                        false,
                        output,
                    );
                }
            } else if object.get("method").and_then(Value::as_str).is_none() {
                return self.fail_backend(
                    "Computer Use backend emitted an invalid notification envelope".into(),
                    false,
                    output,
                );
            }
            return write_raw_line(output, &line);
        }
        let RuntimeState::Backend(backend) = &mut self.runtime else {
            return Ok(());
        };
        match backend.phase {
            BackendPhase::Initialize => {
                if parsed.get("id") != Some(&Value::String(INTERNAL_INITIALIZE_ID.into())) {
                    return Ok(());
                }
                let protocol = parsed
                    .get("result")
                    .and_then(|result| result.get("protocolVersion"))
                    .and_then(Value::as_str);
                if protocol != Some(MCP_PROTOCOL_VERSION) {
                    return self.fail_backend(
                        "Computer Use backend initialization contract mismatch".into(),
                        false,
                        output,
                    );
                }
                let initialized = write_child_json(
                    &mut backend.stdin,
                    &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                )
                .and_then(|_| {
                    write_child_json(
                        &mut backend.stdin,
                        &json!({
                            "jsonrpc":"2.0",
                            "id":INTERNAL_TOOLS_LIST_ID,
                            "method":"tools/list",
                            "params":{}
                        }),
                    )
                });
                if let Err(error) = initialized {
                    return self.fail_backend(
                        format!("Computer Use backend initialization failed: {error}"),
                        true,
                        output,
                    );
                }
                backend.phase = BackendPhase::Catalog;
                Ok(())
            }
            BackendPhase::Catalog => {
                if parsed.get("id") != Some(&Value::String(INTERNAL_TOOLS_LIST_ID.into())) {
                    return Ok(());
                }
                let backend_tools = parsed.get("result").and_then(|result| result.get("tools"));
                if backend_tools != Some(&self.tools) {
                    return self.fail_backend(
                        "Computer Use backend tool catalog mismatch".into(),
                        false,
                        output,
                    );
                }
                backend.phase = BackendPhase::Ready;
                Ok(())
            }
            BackendPhase::Ready => unreachable!("handled above"),
        }
    }

    fn forward_to_backend(
        &mut self,
        raw: String,
        request_id: Option<Value>,
        events: &Sender<Event>,
    ) -> io::Result<()> {
        let write_result = match &mut self.runtime {
            RuntimeState::Backend(backend)
                if backend.phase == BackendPhase::Ready && backend.drain.is_none() =>
            {
                if let Some(id) = request_id.as_ref() {
                    // A flush can report EPIPE after the backend consumed the
                    // complete line and queued a response. Track the ID before
                    // writing so a response received during drain can win.
                    backend.pending_request_ids.push(id.clone());
                }
                let result = backend
                    .stdin
                    .write_all(raw.trim().as_bytes())
                    .and_then(|_| {
                        backend.stdin.write_all(b"\n")?;
                        backend.stdin.flush()
                    });
                result
            }
            _ => return Ok(()),
        };
        if let Err(error) = write_result {
            self.begin_backend_drain(
                format!("Computer Use backend input closed: {error}"),
                true,
                events,
            );
        }
        Ok(())
    }

    fn begin_backend_drain(&mut self, detail: String, retryable: bool, events: &Sender<Event>) {
        if let RuntimeState::Backend(backend) = &mut self.runtime {
            if backend.drain.is_none() {
                let generation = backend.generation;
                backend.drain = Some(BackendDrain {
                    detail,
                    retryable,
                    expired: false,
                });
                let events = events.clone();
                thread::spawn(move || {
                    thread::sleep(BACKEND_DRAIN_TIMEOUT);
                    let _ = events.send(Event::BackendDrainExpired { generation });
                });
            }
        }
    }

    fn poll_processes(&mut self, events: &Sender<Event>, output: &mut dyn Write) -> io::Result<()> {
        let mut setup_finished = None;
        let mut backend_failure = None;
        let mut restart_retired_backend = false;
        match &mut self.runtime {
            RuntimeState::Setup(setup) => {
                if let Some(failure) = setup.reported_failure.take() {
                    // Settle and flush deferred client calls in fail_setup
                    // before waiting for the setup group's cleanup window.
                    setup_finished = Some(Err(failure));
                } else if setup.stdout_closed && setup.exit_status.is_none() {
                    // Keep an exited group leader unreaped while descendants
                    // still hold stdout. Its PID pins the PGID so later host
                    // shutdown can terminate that exact process group safely.
                    match setup.child.try_wait() {
                        Ok(status) => setup.exit_status = status,
                        Err(error) => {
                            setup.reported_failure =
                                Some((format!("setup wait failed: {error}"), false))
                        }
                    }
                }
                if setup.stdout_closed {
                    if let Some(status) = setup.exit_status {
                        setup_finished = Some(
                            if let Some((detail, retryable)) = setup.reported_failure.take() {
                                Err((detail, retryable))
                            } else if !status.success() {
                                Err((format!("setup exited with {status}"), false))
                            } else if !setup.ready_seen {
                                Err((
                                    "setup exited successfully without verified readiness".into(),
                                    false,
                                ))
                            } else {
                                Ok(())
                            },
                        );
                    }
                }
            }
            RuntimeState::Backend(backend) => {
                if backend.stdout_closed && backend.exit_status.is_none() {
                    // As with setup, do not reap a group leader while a
                    // descendant still owns the inherited output pipe.
                    match backend.child.try_wait() {
                        Ok(status) => backend.exit_status = status,
                        Err(error) => {
                            backend_failure = Some((format!("backend wait failed: {error}"), true))
                        }
                    }
                }
                // The stdout reader queues every complete response before its
                // closed event. Wait for that event so a final valid response
                // is retired before unresolved IDs are failed.
                if let Some(drain) = backend.drain.as_ref() {
                    if backend.stdout_closed || drain.expired {
                        if backend.restart_on_retirement {
                            restart_retired_backend = true;
                        } else {
                            backend_failure = Some((drain.detail.clone(), drain.retryable));
                        }
                    }
                } else if backend.stdout_closed {
                    if backend.restart_on_retirement {
                        restart_retired_backend = true;
                    } else {
                        backend_failure = Some(("Computer Use backend stopped".into(), true));
                    }
                } else if backend.phase != BackendPhase::Ready
                    && backend.started_at.elapsed() > BACKEND_INITIALIZATION_TIMEOUT
                {
                    backend_failure =
                        Some(("Computer Use backend initialization timed out".into(), true));
                }
            }
            RuntimeState::Dormant | RuntimeState::Failed(_) => {}
        }

        if let Some(result) = setup_finished {
            match result {
                Ok(()) => {
                    self.runtime = RuntimeState::Dormant;
                    if let Err(error) = self.start_backend(events) {
                        self.fail_setup(error, false, output)?;
                    }
                }
                Err((error, retryable)) => self.fail_setup(error, retryable, output)?,
            }
        }
        if let Some((error, retryable)) = backend_failure {
            self.fail_backend(error, retryable, output)?;
        }
        if restart_retired_backend {
            self.restart_retired_backend(events, output)?;
        }
        Ok(())
    }

    fn restart_retired_backend(
        &mut self,
        events: &Sender<Event>,
        output: &mut dyn Write,
    ) -> io::Result<()> {
        let pending_request_ids = match &mut self.runtime {
            RuntimeState::Backend(backend) => std::mem::take(&mut backend.pending_request_ids),
            _ => return Ok(()),
        };
        let mut retired = std::mem::replace(&mut self.runtime, RuntimeState::Dormant);
        let mut response_result = Ok(());
        for id in pending_request_ids {
            if response_result.is_ok() {
                response_result = write_json(output, &rpc_result(id, backend_failed_result(true)));
            }
        }
        retired.terminate_with_signal(None);
        response_result?;
        if let Err(error) = self.start_backend(events) {
            self.fail_backend(
                format!("could not restart retired Computer Use backend: {error}"),
                true,
                output,
            )?;
        }
        Ok(())
    }

    fn fail_setup(
        &mut self,
        detail: String,
        retryable: bool,
        output: &mut dyn Write,
    ) -> io::Result<()> {
        let deferred = std::mem::take(&mut self.deferred_tool_calls);
        let reason = public_failure_reason(&detail);
        let mut previous = self.enter_failure(detail, retryable, deferred.is_empty());
        let mut response_result = Ok(());
        for call in deferred {
            if response_result.is_ok() {
                response_result = write_json(
                    output,
                    &rpc_result(call.id, setup_failed_result(retryable, &reason)),
                );
            }
        }
        previous.terminate();
        response_result
    }

    fn fail_backend(
        &mut self,
        detail: String,
        retryable: bool,
        output: &mut dyn Write,
    ) -> io::Result<()> {
        let mut pending_request_ids = match &mut self.runtime {
            RuntimeState::Backend(backend) => std::mem::take(&mut backend.pending_request_ids),
            _ => Vec::new(),
        };
        pending_request_ids.extend(
            std::mem::take(&mut self.deferred_tool_calls)
                .into_iter()
                .map(|call| call.id),
        );
        let report_on_next_call = pending_request_ids.is_empty();
        let mut previous = self.enter_failure(detail, retryable, report_on_next_call);
        let mut response_result = Ok(());
        for id in pending_request_ids {
            if response_result.is_ok() {
                response_result =
                    write_json(output, &rpc_result(id, backend_failed_result(retryable)));
            }
        }
        previous.terminate();
        response_result
    }

    fn fail(&mut self, detail: String, retryable: bool, report_on_next_call: bool) {
        self.fail_with_signal(detail, retryable, report_on_next_call, None);
    }

    fn fail_with_signal(
        &mut self,
        detail: String,
        retryable: bool,
        report_on_next_call: bool,
        signal: Option<i32>,
    ) {
        self.fail_with_signal_and_grace(
            detail,
            retryable,
            report_on_next_call,
            signal,
            CHILD_TERMINATION_GRACE,
        );
    }

    fn enter_failure(
        &mut self,
        detail: String,
        retryable: bool,
        report_on_next_call: bool,
    ) -> RuntimeState {
        let reason = public_failure_reason(&detail);
        eprintln!(
            "computer-use-mcp-bootstrap: {}",
            bounded_diagnostic(&detail)
        );
        std::mem::replace(
            &mut self.runtime,
            RuntimeState::Failed(Failure {
                reason,
                retryable,
                report_on_next_call,
            }),
        )
    }

    fn fail_with_signal_and_grace(
        &mut self,
        detail: String,
        retryable: bool,
        report_on_next_call: bool,
        signal: Option<i32>,
        grace: Duration,
    ) {
        let mut previous = self.enter_failure(detail, retryable, report_on_next_call);
        previous.terminate_with_signal_and_grace(signal, grace);
    }

    fn shutdown_with_signal(&mut self, signal: i32) {
        let mut previous = std::mem::replace(&mut self.runtime, RuntimeState::Dormant);
        previous.terminate_with_signal(Some(signal));
        self.stopping = true;
    }
}

impl RuntimeState {
    fn terminate(&mut self) {
        self.terminate_with_signal_and_grace(None, CHILD_TERMINATION_GRACE);
    }

    fn terminate_with_signal(&mut self, signal: Option<i32>) {
        // Every owned-child retirement path gets the same bounded cleanup
        // window. Cancellation transport replacement and protocol failures
        // can otherwise SIGKILL the Driver before it removes its managed
        // daemon/socket generation.
        self.terminate_with_signal_and_grace(signal, CHILD_TERMINATION_GRACE);
    }

    fn terminate_with_signal_and_grace(&mut self, signal: Option<i32>, grace: Duration) {
        match self {
            Self::Setup(setup) => {
                terminate_child_impl(&mut setup.child, signal, setup.exit_status.is_some(), grace)
            }
            Self::Backend(backend) => {
                let _ = backend.stdin.flush();
                terminate_child_impl(
                    &mut backend.child,
                    signal,
                    backend.exit_status.is_some(),
                    grace,
                );
            }
            Self::Dormant | Self::Failed(_) => {}
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.runtime.terminate();
    }
}

pub fn run(config: Config) -> io::Result<()> {
    install_signal_forwarding()?;
    let (events_tx, events_rx) = mpsc::channel();
    spawn_client_reader(events_tx.clone());
    serve(Server::new(config), events_tx, events_rx, io::stdout())
}

fn serve<W: Write>(
    mut server: Server,
    events_tx: Sender<Event>,
    events_rx: Receiver<Event>,
    output: W,
) -> io::Result<()> {
    let mut output = BufWriter::new(output);
    while !server.stopping {
        match events_rx.recv_timeout(EVENT_POLL_INTERVAL) {
            Ok(event) => server.handle_event(event, &events_tx, &mut output)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        server.poll_processes(&events_tx, &mut output)?;
        server.progress_deferred_tool_calls(&events_tx, &mut output)?;
        if let Some(signal) = take_received_signal() {
            server.shutdown_with_signal(signal);
        }
    }
    Ok(())
}

fn spawn_client_reader(events: Sender<Event>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = events.send(Event::ClientEof);
                    return;
                }
                Ok(_) => {
                    if events.send(Event::ClientLine(line.clone())).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = events.send(Event::ClientReadError(error.to_string()));
                    return;
                }
            }
        }
    });
}

fn spawn_line_reader<R, L, C>(reader: R, events: Sender<Event>, line_event: L, close_event: C)
where
    R: io::Read + Send + 'static,
    L: Fn(String) -> Event + Send + 'static,
    C: Fn() -> Event + Send + 'static,
{
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    if events.send(line_event(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    eprintln!("computer-use-mcp-bootstrap: child output failed: {error}");
                    break;
                }
            }
        }
        let _ = events.send(close_event());
    });
}

fn verified_command(spec: &CommandSpec) -> Result<Command, String> {
    let program = canonical_executable(&spec.program)?;
    let mut command = Command::new(program);
    command.args(&spec.args);
    configure_child_process_group(&mut command);
    Ok(command)
}

#[cfg(unix)]
fn configure_child_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_child_process_group(_command: &mut Command) {}

#[cfg(unix)]
extern "C" fn record_signal(signal: libc::c_int) {
    RECEIVED_SIGNAL.store(signal, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_signal_forwarding() -> io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let previous =
            unsafe { libc::signal(signal, record_signal as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn install_signal_forwarding() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn take_received_signal() -> Option<i32> {
    let signal = RECEIVED_SIGNAL.swap(0, Ordering::SeqCst);
    (signal != 0).then_some(signal)
}

#[cfg(not(unix))]
fn take_received_signal() -> Option<i32> {
    None
}

#[cfg(unix)]
fn signal_process_group(process_group: i32, signal: i32) {
    unsafe {
        libc::kill(-process_group, signal);
    }
}

fn terminate_child(child: &mut Child, initial_signal: Option<i32>) {
    terminate_child_impl(child, initial_signal, false, CHILD_TERMINATION_GRACE);
}

fn terminate_child_impl(
    child: &mut Child,
    initial_signal: Option<i32>,
    leader_reaped: bool,
    grace: Duration,
) {
    #[cfg(unix)]
    let process_group = i32::try_from(child.id()).ok();
    #[cfg(unix)]
    if !leader_reaped {
        if let Some(process_group) = process_group {
            // The caller has not reaped this child, so its PID pins the PGID
            // through the complete TERM/grace/KILL sequence.
            signal_process_group(process_group, initial_signal.unwrap_or(libc::SIGTERM));
        }
    }
    #[cfg(not(unix))]
    if !leader_reaped {
        let _ = child.kill();
    }

    let deadline = Instant::now() + grace;
    #[cfg(unix)]
    if !leader_reaped {
        if let Some(process_group) = process_group {
            let mut drained_observations = 0;
            while drained_observations < 2 && Instant::now() < deadline {
                if process_group_has_live_members(process_group) {
                    drained_observations = 0;
                } else {
                    drained_observations += 1;
                }
                thread::sleep(Duration::from_millis(10));
            }
            if process_group_has_live_members(process_group) {
                signal_process_group(process_group, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    {
        if !leader_reaped {
            let _ = child.kill();
        }
    }
    if !leader_reaped {
        let _ = child.wait();
    }
}

#[cfg(target_os = "macos")]
fn process_group_has_live_members(process_group: i32) -> bool {
    const MAX_GROUP_PROCESSES: usize = 256;
    let mut pids = [0 as libc::pid_t; MAX_GROUP_PROCESSES];
    let buffer_bytes = std::mem::size_of_val(&pids);
    let returned_count = unsafe {
        libc::proc_listpgrppids(
            process_group,
            pids.as_mut_ptr().cast(),
            i32::try_from(buffer_bytes).expect("bounded PID buffer fits c_int"),
        )
    };
    if returned_count < 0 {
        return true;
    }
    let count = returned_count as usize;
    if count >= pids.len() {
        return true;
    }
    pids[..count].iter().copied().any(|pid| {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let info_bytes = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
                    .expect("proc_bsdinfo fits c_int"),
            )
        };
        if info_bytes as usize != std::mem::size_of::<libc::proc_bsdinfo>() {
            return false;
        }
        let info = unsafe { info.assume_init() };
        info.pbi_pgid == process_group as u32 && info.pbi_status != libc::SZOMB
    })
}

#[cfg(target_os = "linux")]
fn process_group_has_live_members(process_group: i32) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return true;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            continue;
        };
        let mut fields = fields.split_whitespace();
        let Some(state) = fields.next() else {
            continue;
        };
        let Some(_parent_pid) = fields.next() else {
            continue;
        };
        let Some(group) = fields.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        if group == process_group && !matches!(state, "Z" | "X" | "x") {
            return true;
        }
    }
    false
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_group_has_live_members(process_group: i32) -> bool {
    unsafe { libc::kill(-process_group, 0) == 0 }
}

fn canonical_executable(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("child program path is not absolute".into());
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        format!(
            "child program is unavailable at {}: {error}",
            path.display()
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        format!(
            "cannot inspect child program {}: {error}",
            canonical.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "child program is not a regular file: {}",
            canonical.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "child program is not executable: {}",
                canonical.display()
            ));
        }
    }
    Ok(canonical)
}

fn write_child_json(stdin: &mut ChildStdin, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdin, value)?;
    stdin.write_all(b"\n")?;
    stdin.flush()
}

fn write_json(output: &mut dyn Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn write_raw_line(output: &mut dyn Write, line: &str) -> io::Result<()> {
    output.write_all(line.trim().as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "result":result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "error":{"code":code, "message":message}
    })
}

fn setup_pending_result(stage: SetupStage) -> Value {
    let text = match stage {
        SetupStage::Installing => {
            "Computer Use is installing its verified runtime. No macOS action is required yet; setup will continue in this session."
        }
        SetupStage::Accessibility => {
            "Computer Use is waiting for Accessibility. Complete the visible macOS permission step, then retry this tool in the same session."
        }
        SetupStage::ScreenRecordingRegistration => {
            "Computer Use is registering its signed app with macOS Screen Recording. No manual app selection is required; setup will continue automatically after any macOS consent dialog."
        }
        SetupStage::ScreenRecording => {
            "Computer Use is waiting for Screen Recording. Complete the visible macOS permission step; setup owns any required Driver restart."
        }
        SetupStage::DriverRestarting => {
            "macOS is restarting the signed Computer Use Driver after a permission change. No action is required; setup will continue automatically."
        }
        SetupStage::TccPropagation => {
            "macOS is applying the Computer Use permission change. No action is required; setup will continue automatically."
        }
        SetupStage::CaptureVerification => {
            "Computer Use permissions are granted and live screen capture is being verified. No action is required unless macOS presents a consent dialog."
        }
        SetupStage::ServiceStarting => {
            "Computer Use permissions are verified and the managed backend is starting. No action is required; this call will continue automatically for a bounded interval."
        }
    };
    json!({
        "content":[{
            "type":"text",
            "text":text
        }],
        "structuredContent":{
            "schema_version":SETUP_STATUS_SCHEMA_VERSION,
            "code":SETUP_PENDING_CODE,
            "stage":stage.as_str(),
            "retryable":true,
            "requires_user_action":stage.requires_user_action()
        },
        "isError":true
    })
}

fn setup_failed_result(retryable: bool, reason: &str) -> Value {
    let text = if retryable {
        format!(
            "Computer Use setup did not complete ({reason}). Retry once in this session to restart the verified setup flow."
        )
    } else {
        format!(
            "Computer Use setup failed verification ({reason}). Use the plugin setup recovery command before retrying."
        )
    };
    json!({
        "content":[{
            "type":"text",
            "text":text.clone()
        }],
        "structuredContent":{
            "schema_version":SETUP_STATUS_SCHEMA_VERSION,
            "code":SETUP_FAILED_CODE,
            "stage":"failed",
            "retryable":retryable,
            "requires_user_action":false,
            "error":{
                "code":reason,
                "message":text
            }
        },
        "isError":true
    })
}

fn backend_failed_result(retryable: bool) -> Value {
    json!({
        "content":[{
            "type":"text",
            "text":"The verified Computer Use backend stopped before completing this call. No result was received."
        }],
        "structuredContent":{
            "schema_version":SETUP_STATUS_SCHEMA_VERSION,
            "code":"computer_use_backend_unavailable",
            "retryable":retryable,
            "requires_user_action":false
        },
        "isError":true
    })
}

fn unsupported_tool_result(name: &str) -> Value {
    json!({
        "content":[{"type":"text", "text":format!("unsupported Computer Use tool: {name}")}],
        "structuredContent":{"code":"unsupported_tool"},
        "isError":true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_is_the_fixed_eleven_tool_surface() {
        let tools = public_tools();
        let names = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, PUBLIC_TOOL_NAMES);
    }

    #[test]
    fn setup_ready_requires_live_capture_verification() {
        let incomplete = json!({
            "schema_version": 1,
            "code": SETUP_READY_CODE,
            "stage": "ready",
            "retryable": false,
            "requires_user_action": false,
            "accessibility": true,
            "screen_recording": true,
            "screen_recording_capturable": false
        });
        assert!(parse_setup_signal(&incomplete.to_string()).is_err());

        let complete = json!({
            "schema_version": 1,
            "code": SETUP_READY_CODE,
            "stage": "ready",
            "retryable": false,
            "requires_user_action": false,
            "accessibility": true,
            "screen_recording": true,
            "screen_recording_capturable": true
        });
        assert_eq!(
            parse_setup_signal(&complete.to_string()).unwrap(),
            SetupSignal::Ready
        );
    }

    #[test]
    fn pending_result_is_structured_and_retryable() {
        let result = setup_pending_result(SetupStage::Accessibility);
        assert_eq!(
            result["structuredContent"],
            json!({
                "schema_version": SETUP_STATUS_SCHEMA_VERSION,
                "code": SETUP_PENDING_CODE,
                "stage": "accessibility",
                "retryable": true,
                "requires_user_action": true
            })
        );
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn pending_result_distinguishes_user_and_autonomous_stages() {
        for stage in [SetupStage::Accessibility, SetupStage::ScreenRecording] {
            let result = setup_pending_result(stage);
            assert_eq!(result["structuredContent"]["requires_user_action"], true);
            assert!(result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("visible macOS permission"));
        }
        for stage in [
            SetupStage::Installing,
            SetupStage::ScreenRecordingRegistration,
            SetupStage::DriverRestarting,
            SetupStage::TccPropagation,
            SetupStage::CaptureVerification,
            SetupStage::ServiceStarting,
        ] {
            let result = setup_pending_result(stage);
            assert_eq!(result["structuredContent"]["requires_user_action"], false);
        }
    }

    #[test]
    fn pending_signal_rejects_action_policy_that_disagrees_with_stage() {
        let status = json!({
            "schema_version": 1,
            "code": SETUP_PENDING_CODE,
            "stage": "service_starting",
            "retryable": true,
            "requires_user_action": true
        });
        assert!(parse_setup_signal(&status.to_string()).is_err());
    }

    #[test]
    fn pending_signal_requires_complete_stage_appropriate_permission_state() {
        let missing = json!({
            "schema_version": 1,
            "code": SETUP_PENDING_CODE,
            "stage": "accessibility",
            "retryable": true,
            "requires_user_action": true
        });
        assert!(parse_setup_signal(&missing.to_string()).is_err());

        let service = json!({
            "schema_version": 1,
            "code": SETUP_PENDING_CODE,
            "stage": "service_starting",
            "retryable": true,
            "requires_user_action": false,
            "accessibility": true,
            "screen_recording": true,
            "screen_recording_capturable": true
        });
        assert_eq!(
            parse_setup_signal(&service.to_string()).unwrap(),
            SetupSignal::Pending(SetupStage::ServiceStarting)
        );

        let registration = json!({
            "schema_version": 1,
            "code": SETUP_PENDING_CODE,
            "stage": "screen_recording_registration",
            "retryable": true,
            "requires_user_action": false,
            "accessibility": true,
            "screen_recording": false,
            "screen_recording_capturable": null
        });
        assert_eq!(
            parse_setup_signal(&registration.to_string()).unwrap(),
            SetupSignal::Pending(SetupStage::ScreenRecordingRegistration)
        );

        for (stage, requires_user_action) in [
            ("accessibility", true),
            ("driver_restarting", false),
            ("tcc_propagation", false),
        ] {
            let screen_already_granted = json!({
                "schema_version": 1,
                "code": SETUP_PENDING_CODE,
                "stage": stage,
                "retryable": true,
                "requires_user_action": requires_user_action,
                "accessibility": false,
                "screen_recording": true,
                "screen_recording_capturable": null
            });
            assert!(parse_setup_signal(&screen_already_granted.to_string()).is_ok());
        }
    }

    #[test]
    fn setup_stage_transitions_allow_restart_cycles_and_verified_handoff() {
        assert!(setup_stage_transition_allowed(
            SetupStage::Accessibility,
            SetupStage::ScreenRecordingRegistration
        ));
        assert!(setup_stage_transition_allowed(
            SetupStage::ScreenRecordingRegistration,
            SetupStage::ScreenRecording
        ));
        assert!(setup_stage_transition_allowed(
            SetupStage::ScreenRecording,
            SetupStage::DriverRestarting
        ));
        assert!(setup_stage_transition_allowed(
            SetupStage::Accessibility,
            SetupStage::CaptureVerification
        ));
        assert!(setup_stage_transition_allowed(
            SetupStage::TccPropagation,
            SetupStage::ScreenRecording
        ));
        assert!(setup_stage_transition_allowed(
            SetupStage::CaptureVerification,
            SetupStage::DriverRestarting
        ));
        for stage in [
            SetupStage::Installing,
            SetupStage::Accessibility,
            SetupStage::ScreenRecordingRegistration,
            SetupStage::ScreenRecording,
            SetupStage::DriverRestarting,
            SetupStage::TccPropagation,
            SetupStage::CaptureVerification,
            SetupStage::ServiceStarting,
        ] {
            assert!(setup_stage_transition_allowed(
                stage,
                SetupStage::ServiceStarting
            ));
        }
        assert!(!setup_stage_transition_allowed(
            SetupStage::ServiceStarting,
            SetupStage::DriverRestarting
        ));
        assert!(!setup_stage_transition_allowed(
            SetupStage::CaptureVerification,
            SetupStage::ScreenRecording
        ));
    }

    #[test]
    fn service_starting_signal_requires_verified_capture() {
        for capture in [Value::Null, Value::Bool(false)] {
            let service = json!({
                "schema_version": 1,
                "code": SETUP_PENDING_CODE,
                "stage": "service_starting",
                "retryable": true,
                "requires_user_action": false,
                "accessibility": true,
                "screen_recording": true,
                "screen_recording_capturable": capture
            });
            assert!(parse_setup_signal(&service.to_string()).is_err());
        }
    }

    #[test]
    fn setup_failure_preserves_retryability() {
        for retryable in [false, true] {
            let status = json!({
                "schema_version": 1,
                "code": SETUP_FAILED_CODE,
                "stage": "failed",
                "retryable": retryable,
                "requires_user_action": false,
                "error": {"code": "fixture", "message": "not public"}
            });
            assert_eq!(
                parse_setup_signal(&status.to_string()).unwrap(),
                SetupSignal::Failed {
                    detail: "fixture".into(),
                    retryable,
                }
            );
            assert_eq!(
                setup_failed_result(retryable, "fixture")["structuredContent"]["retryable"],
                retryable
            );
            assert_eq!(
                setup_failed_result(retryable, "fixture")["structuredContent"]["error"]["code"],
                "fixture"
            );
        }
    }

    #[test]
    fn public_failure_reason_exposes_only_stable_codes() {
        assert_eq!(
            public_failure_reason("onboarding_host_restart_limit_reached"),
            "onboarding_host_restart_limit_reached"
        );
        assert_eq!(
            public_failure_reason("failed at /private/user/path"),
            "setup_internal_failure"
        );
    }

    #[test]
    fn child_termination_grace_contains_plugin_cleanup_budget() {
        assert_eq!(CHILD_TERMINATION_GRACE, Duration::from_secs(25));
        assert!(CHILD_TERMINATION_GRACE > Duration::from_secs(20));
    }

    #[cfg(unix)]
    #[test]
    fn termination_keeps_the_leader_unreaped_until_descendants_are_killed() {
        let temp = tempfile::TempDir::new().unwrap();
        let marker = temp.path().join("group");
        let script = r#"
import os, pathlib, signal, sys, time
root = pathlib.Path(sys.argv[1])

def stop_parent(_signal, _frame):
    raise SystemExit(0)

signal.signal(signal.SIGTERM, stop_parent)
pid = os.fork()
if pid == 0:
    def keep_running(_signal, _frame):
        root.with_suffix('.term').write_text('term\n')
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, keep_running)
    root.with_suffix('.child-pid').write_text(str(os.getpid()))
    while not root.with_suffix('.term').exists():
        signal.pause()
    time.sleep(0.5)
    root.with_suffix('.survived').write_text('orphaned\n')
    while True:
        signal.pause()

while not root.with_suffix('.child-pid').exists():
    time.sleep(0.01)
root.with_suffix('.ready').write_text('ready\n')
while True:
    signal.pause()
"#;
        let mut command = Command::new("/usr/bin/python3");
        command.arg("-c").arg(script).arg(&marker);
        configure_child_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        // Hosted macOS runners resolve /usr/bin/python3 through the selected
        // Xcode toolchain, whose first cold launch can take more than three
        // seconds. Keep this deadline bounded without making toolchain startup
        // part of the process-group behavior under test.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !marker.with_extension("ready").exists() {
            if Instant::now() >= deadline {
                terminate_child_impl(&mut child, None, false, Duration::ZERO);
                panic!("process group did not start");
            }
            thread::sleep(Duration::from_millis(10));
        }

        terminate_child_impl(&mut child, None, false, Duration::from_millis(100));

        let child_pid: i32 = fs::read_to_string(marker.with_extension("child-pid"))
            .unwrap()
            .parse()
            .unwrap();
        let descendant_saw_term = marker.with_extension("term").exists();
        thread::sleep(Duration::from_millis(550));
        let descendant_was_orphaned = marker.with_extension("survived").exists();
        unsafe {
            libc::kill(child_pid, libc::SIGKILL);
        }
        assert!(
            descendant_saw_term,
            "descendant did not receive initial SIGTERM"
        );
        assert!(
            !descendant_was_orphaned,
            "descendant survived after the original group leader exited"
        );
    }

    #[test]
    fn initialize_includes_the_complete_guarded_wrapper_instructions() {
        let config = Config {
            plugin_version: "0.6.0".into(),
            setup: CommandSpec {
                program: PathBuf::from("/setup"),
                args: Vec::new(),
            },
            backend: CommandSpec {
                program: PathBuf::from("/backend"),
                args: Vec::new(),
            },
        };
        let result = Server::new(config).initialize_result();
        let instructions = result["instructions"].as_str().unwrap();
        assert!(instructions.starts_with("Computer Use setup starts on the first tool call."));
        assert!(instructions.ends_with(PUBLIC_INITIALIZE_INSTRUCTIONS.trim_end()));
        assert!(instructions.contains("package_attestation"));
        assert!(instructions.contains("batch_actions"));
    }

    #[test]
    fn command_line_accepts_hyphen_leading_child_arguments() {
        let parsed = CommandLine::parse(
            [
                "--plugin-version",
                "0.6.0",
                "--setup-program",
                "/bin/sh",
                "--setup-arg",
                "--first-use",
                "--backend-program",
                "/bin/sh",
                "--backend-arg",
                "--driver",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .unwrap();
        let CommandLine::Run(config) = parsed else {
            panic!("expected run configuration");
        };
        assert_eq!(config.setup.args, [OsString::from("--first-use")]);
        assert_eq!(config.backend.args, [OsString::from("--driver")]);
    }

    #[test]
    fn private_build_attestation_is_exact_and_cannot_be_combined() {
        assert_eq!(
            CommandLine::parse([OsString::from("__build-attestation")]).unwrap(),
            CommandLine::BuildAttestation,
        );
        assert!(CommandLine::parse(
            ["__build-attestation", "--version"]
                .into_iter()
                .map(OsString::from),
        )
        .is_err());

        let attestation = build_attestation();
        assert_eq!(attestation["schema_version"], 1);
        assert_eq!(
            attestation["binary_version"],
            option_env!("CUA_DRIVER_RELEASE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
        );
        assert_eq!(
            attestation["source_sha"],
            serde_json::json!(option_env!("CUA_DRIVER_SOURCE_SHA")),
        );
    }
}
