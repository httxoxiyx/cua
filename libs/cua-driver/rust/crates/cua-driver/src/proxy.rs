//! Stdio MCP adapters for direct SDK-owned and service-owned runtimes.
//!
//! The client side always sees a normal stdio server. Depending on platform
//! and explicit launch options, this adapter either owns the SDK runtime
//! directly or forwards to a service that owns it.
//!
//! On macOS the CLI can ensure a daemon is running under `LaunchServices`
//! (which gives it the right TCC attribution). Embedded hosts may also start a
//! private service explicitly. The MCP client never sees that ownership
//! boundary — it receives the same JSON-RPC envelope.
//!
//! Why this lives in `cua-driver` and not `mcp-server`:
//!   `cua_driver_core::server` defines the shared JSON-RPC protocol. The
//!   proxy speaks that protocol on the client side, while the server side is the daemon's
//!   line-delimited JSON UDS protocol, owned by `crate::serve`.
//!   Putting the proxy here avoids `mcp-server → cua-driver` reverse
//!   coupling.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use cua_driver_core::policy::{authorize_tool_call, validate_configured_policy};
use cua_driver_core::protocol::{initialize_result, Request, Response};
use cua_driver_core::server::{
    observe_proxy_session_started, observe_proxy_tool_completed, tool_observation_timer,
    StdioExecutionPath,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, error, warn};

use crate::serve::{
    is_daemon_listening, send_request, DaemonRequest, DaemonResponse, ToolObservationOrigin,
};

const CONTROL_SESSION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const CONTROL_SESSION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const CANCELLATION_SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_QUEUED_PROXY_REQUESTS: usize = 8;

#[derive(Clone, Copy)]
struct ControlConnectionTiming {
    heartbeat_interval: Duration,
    response_timeout: Duration,
}

impl Default for ControlConnectionTiming {
    fn default() -> Self {
        Self {
            heartbeat_interval: CONTROL_SESSION_HEARTBEAT_INTERVAL,
            response_timeout: CONTROL_SESSION_RESPONSE_TIMEOUT,
        }
    }
}

/// Run stdio MCP directly over an SDK-owned runtime.
///
/// Windows and Linux use this when no explicit service endpoint was selected.
/// The runtime lives exactly as long as stdin: EOF ends every observed public
/// session, drains admitted work through `shutdown`, and releases process
/// ownership before returning.
pub async fn run_direct(driver: Arc<cua_driver_sdk::CuaDriver>) -> anyhow::Result<()> {
    // Direct stdio is an action endpoint just like `serve`; enforce the same
    // admin lock, bounded-manifest approval/expiry, and legacy-approval
    // consistency before the first request can be read.
    cua_driver_core::authorization::validate_startup_authorization()?;
    validate_configured_policy()?;
    let sdk = crate::sdk_adapter::SdkAdapter::load(driver.clone()).await?;
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = tokio::io::BufWriter::new(stdout);
    let mut line = String::new();
    let mut session_observed = false;
    let transport_session = format!("mcp-{}", uuid::Uuid::new_v4());
    struct DirectTransportCleanup {
        sdk: Arc<crate::sdk_adapter::SdkAdapter>,
        transport_session: String,
    }
    impl Drop for DirectTransportCleanup {
        fn drop(&mut self) {
            self.sdk.end_transport_sessions(&self.transport_session);
        }
    }
    let _cleanup = DirectTransportCleanup {
        sdk: sdk.clone(),
        transport_session: transport_session.clone(),
    };

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(trimmed) {
            Err(error) => {
                error!("JSON parse error: {error}");
                Response::parse_error()
            }
            Ok(request) if request.is_notification() => continue,
            Ok(mut request) => {
                apply_direct_session_identity(&mut request, &transport_session);
                let initialize_metadata = (!session_observed)
                    .then(|| request.initialize_metadata())
                    .flatten();
                let session_context = request.tool_call().ok().and_then(|call| {
                    sdk.begin_tool_call(
                        &call.name,
                        &call.args,
                        cua_driver_core::session::SessionTransport::McpStdio,
                        cua_driver_core::session::SessionClientKind::Mcp,
                    )
                });
                let timer = tool_observation_timer(
                    &request,
                    |name| sdk.is_known_tool(name),
                    StdioExecutionPath::DirectDaemon,
                );
                let id = request.id.clone().unwrap_or(serde_json::Value::Null);
                let response = cua_driver_core::server::handle_request_with_transport_session(
                    request,
                    id,
                    sdk.as_ref(),
                    &transport_session,
                )
                .await;
                if let Some(metadata) = initialize_metadata {
                    observe_proxy_session_started(metadata);
                    session_observed = true;
                }
                if let Some(timer) = timer {
                    let outcome = timer.finish(&response);
                    if let Some(context) = session_context {
                        context.complete(&outcome);
                    }
                    observe_proxy_tool_completed(outcome);
                }
                response
            }
        };
        let serialized = serde_json::to_string(&response).unwrap_or_else(|error| {
            format!(
                r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize error: {error}"}}}}"#
            )
        });
        writer.write_all(serialized.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    sdk.shutdown().await.map_err(anyhow::Error::msg)
}

fn apply_direct_session_identity(request: &mut Request, transport_session: &str) {
    let Some(arguments) = request
        .params
        .as_mut()
        .and_then(|params| params.get_mut("arguments"))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let effective = arguments
        .get("session")
        .and_then(serde_json::Value::as_str)
        .filter(|session| !session.is_empty())
        .unwrap_or(transport_session)
        .to_owned();
    arguments.insert("_session_id".into(), serde_json::Value::String(effective));
    arguments.insert(
        "_transport_session_id".into(),
        serde_json::Value::String(transport_session.to_owned()),
    );
}

/// Run the MCP stdio proxy. Reads JSON-RPC lines from stdin, forwards
/// the body of each `tools/list` / `tools/call` to the daemon at
/// `socket_path`, and writes the daemon's response back as a proper
/// JSON-RPC envelope.
///
/// Implements the core protocol's EOF, parse-error, notification, and
/// response rules while forwarding method dispatch to the daemon.
///
/// Fails fast if the daemon isn't reachable, so MCP clients see a
/// clear startup error instead of a "successful" handshake that
/// advertises zero tools and then errors on every call. Matches
/// Swift `makeProxy`'s `fetchProxyToolList` pre-check.
pub async fn run_proxy(socket_path: String) -> anyhow::Result<()> {
    validate_configured_policy()?;
    if !is_daemon_listening(&socket_path) {
        anyhow::bail!(
            "cua-driver-rs daemon not reachable on {socket_path}. Start it \
             with `open -n -g -a CuaDriver --args serve` and retry."
        );
    }
    // A selected service may outlive the CLI package that launched this
    // proxy. Refuse an incompatible contract before creating the control
    // binding or forwarding any action.
    let compatibility_client = cua_driver_sdk::CuaDriver::connect(Some(socket_path.clone()))?;
    compatibility_client.metadata().await?;

    // Mint this MCP session's identity once at proxy startup. One proxy process
    // == one MCP session; the daemon outlives it. We stamp this id on every
    // forwarded request so the daemon can OWN and CLEAN UP this session's
    // state (recording, config overrides) and tear it down on disconnect via
    // the control socket's EOF. Dep-free `pid + start-nanos` is sufficient for
    // daemon-local uniqueness over this proxy's lifetime (no `uuid` crate dep
    // for one mint).
    let session_id = mint_session_id();
    debug!(session_id = %session_id, "proxy session minted");

    // Open ONE long-lived "control" connection to the daemon and hold it open
    // for this proxy's entire lifetime (separate from the per-call connections
    // that `send_request` opens and closes per tool call). It sends
    // `session_begin`, then renews the transport-owned lifecycle sessions every
    // minute while the MCP proxy remains alive.
    //
    // This is the reaper: when the proxy exits (graceful stdin EOF) OR is
    // SIGKILLed/crashes, the kernel closes this socket; the daemon's
    // per-connection reader hits EOF and fires `session_end` for `session_id`,
    // tearing down every piece of state this session owns (overlay cursor,
    // config overrides, recording). The bounded heartbeat keeps already-created
    // lifecycle sessions fresh; before the first Computer Use call there is
    // nothing to create or renew.
    //
    // The daemon must acknowledge `session_begin` before the proxy accepts tool
    // calls. Besides lifecycle cleanup, that registered control channel is the
    // trust boundary used by destructive `browser_prepare` calls.
    let (control_ready_tx, control_ready_rx) = tokio::sync::oneshot::channel();
    let socket = socket_path.clone();
    let sid = session_id.clone();
    let control_task =
        tokio::spawn(async move { run_control_connection(socket, sid, control_ready_tx).await });
    match tokio::time::timeout(Duration::from_secs(4), control_ready_rx).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            let result = control_task.await;
            return Err(control_task_failure(
                result,
                "daemon control session closed before acknowledgement",
            ));
        }
        Err(_) => {
            control_task.abort();
            let _ = control_task.await;
            anyhow::bail!("daemon did not acknowledge the MCP control session");
        }
    }

    // Cache the tool list once at startup. The daemon's registry is
    // static for the lifetime of the daemon, so polling on every
    // `tools/list` would waste a round-trip per call. Swift does the
    // same caching in `fetchProxyToolList`.
    let (cached_tools_list, daemon_observes_tool_calls) =
        fetch_tools_list_from_daemon(&socket_path, &session_id)?;
    let cached_tools_list = Arc::new(cached_tools_list);

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (stop_control_tx, stop_control_rx) = tokio::sync::oneshot::channel();
    supervise_proxy_io(
        run_proxy_io(
            BufReader::new(stdin),
            tokio::io::BufWriter::new(stdout),
            &socket_path,
            &cached_tools_list,
            &session_id,
            daemon_observes_tool_calls,
            stop_control_tx,
        ),
        control_task,
        stop_control_rx,
    )
    .await
}

fn control_task_failure(
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
    fallback: &str,
) -> anyhow::Error {
    match result {
        Ok(Ok(())) => anyhow::anyhow!(fallback.to_owned()),
        Ok(Err(error)) => error,
        Err(error) => anyhow::anyhow!("MCP control-session task failed: {error}"),
    }
}

async fn supervise_proxy_io<F>(
    proxy_io: F,
    control_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    stop_control: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<()>>,
{
    supervise_proxy_io_with_timeout(
        proxy_io,
        control_task,
        stop_control,
        CANCELLATION_SETTLEMENT_TIMEOUT,
    )
    .await
}

async fn supervise_proxy_io_with_timeout<F>(
    proxy_io: F,
    mut control_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    mut stop_control: tokio::sync::oneshot::Receiver<()>,
    settlement_timeout: Duration,
) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<()>>,
{
    tokio::pin!(proxy_io);
    tokio::select! {
        biased;
        stop = &mut stop_control => {
            // Deliberate cancellation/EOF ends only this proxy's transport
            // owner. The per-call socket remains open until its normal result
            // proves native dispatch and cleanup have settled. A failed or
            // timed-out exchange provides no such proof and must not be replayed.
            control_task.abort();
            let _ = control_task.await;
            if stop.is_err() {
                return proxy_io.await;
            }
            tokio::time::timeout(settlement_timeout, &mut proxy_io)
                .await
                .map_err(|_| anyhow::anyhow!(
                    "native cancellation did not settle before the deadline; transport retired without settlement proof; do not replay"
                ))?
        }
        result = &mut proxy_io => {
            control_task.abort();
            let _ = control_task.await;
            result
        }
        result = &mut control_task => Err(control_task_failure(
            result,
            "daemon control session closed while the MCP proxy was still running",
        )),
    }
}

/// Run the service-owned stdio loop over caller-provided I/O.
///
/// Reader EOF or matching cancellation closes the control owner immediately,
/// but an in-flight call is still read through its normal response before this
/// proxy retires. It must never reuse an ended transport for another request.
async fn run_proxy_io<R, W>(
    reader: R,
    mut writer: W,
    socket_path: &str,
    cached_tools_list: &Arc<serde_json::Value>,
    session_id: &str,
    daemon_observes_tool_calls: bool,
    stop_control: tokio::sync::oneshot::Sender<()>,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Lines::next_line is cancellation-safe. Repeated select! against
    // read_line would lose a partially read JSON notification when the tool
    // response wins the race.
    let mut lines = reader.lines();
    let mut queued = VecDeque::new();
    let mut stop_control = Some(stop_control);
    let mut session_observed = false;

    loop {
        let line = match queued.pop_front() {
            Some(line) => line,
            None => match lines.next_line().await? {
                Some(line) => line,
                None => {
                    stop_proxy_control(&mut stop_control);
                    break;
                }
            },
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        debug!(raw = trimmed, "→ proxy request");

        let mut retire = false;
        let response = match serde_json::from_str::<Request>(trimmed) {
            Err(e) => {
                error!("JSON parse error: {e}");
                Response::parse_error()
            }
            Ok(req) if req.is_notification() => {
                // No call is active here. Late/unknown cancellation and other
                // notifications cannot retire or revive an unrelated request.
                continue;
            }
            Ok(req) => {
                let initialize_metadata = (!session_observed)
                    .then(|| req.initialize_metadata())
                    .flatten();
                let session_context = (!daemon_observes_tool_calls)
                    .then(|| {
                        req.tool_call().ok().and_then(|call| {
                            let known_tool = proxy_knows_tool(cached_tools_list, &call.name);
                            cua_driver_core::session::begin_tool_call(
                                &call.name,
                                &call.args,
                                known_tool,
                                cua_driver_core::session::SessionTransport::McpStdio,
                                cua_driver_core::session::SessionClientKind::Mcp,
                            )
                        })
                    })
                    .flatten();
                let tool_timer = (!daemon_observes_tool_calls)
                    .then(|| {
                        tool_observation_timer(
                            &req,
                            |name| proxy_knows_tool(cached_tools_list, name),
                            StdioExecutionPath::DaemonProxy,
                        )
                    })
                    .flatten();
                let id = req.id.clone().unwrap_or(serde_json::Value::Null);
                let response_future = handle_proxy_request(
                    req,
                    id.clone(),
                    socket_path,
                    cached_tools_list,
                    session_id,
                    daemon_observes_tool_calls,
                );
                let (response, retiring) = await_proxy_response(
                    &mut lines,
                    &mut queued,
                    &id,
                    response_future,
                    &mut stop_control,
                )
                .await?;
                retire = retiring;
                if let Some(metadata) = initialize_metadata {
                    observe_proxy_session_started(metadata);
                    session_observed = true;
                }
                if let Some(timer) = tool_timer {
                    let outcome = timer.finish(&response);
                    if let Some(context) = session_context {
                        context.complete(&outcome);
                    }
                    observe_proxy_tool_completed(outcome);
                }
                response
            }
        };

        let serialized = serde_json::to_string(&response).unwrap_or_else(|e| {
            format!(
                r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize error: {e}"}}}}"#
            )
        });
        debug!(raw = %serialized, "← proxy response");

        writer.write_all(serialized.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        if retire {
            break;
        }
    }

    // Teardown is still owned by the control connection's EOF. Explicit
    // cancellation only closes it sooner, while retaining this call's response
    // channel long enough to distinguish settlement from uncertain transport loss.
    Ok(())
}

fn stop_proxy_control(stop_control: &mut Option<tokio::sync::oneshot::Sender<()>>) {
    if let Some(stop) = stop_control.take() {
        let _ = stop.send(());
    }
}

fn cancels_request(request: &Request, active_id: &serde_json::Value) -> bool {
    request.is_notification()
        && request.method == "notifications/cancelled"
        && request
            .params
            .as_ref()
            .and_then(|params| params.get("requestId"))
            .is_some_and(|id| id == active_id && (id.is_string() || id.is_number()))
}

/// Observe cancellation without dropping the admitted request future. Ordinary
/// pipelined messages retain their order, with the same sequential dispatch and
/// bounded lookahead/backpressure; notifications do not occupy the queue.
async fn await_proxy_response<R, F>(
    lines: &mut tokio::io::Lines<R>,
    queued: &mut VecDeque<String>,
    active_id: &serde_json::Value,
    response: F,
    stop_control: &mut Option<tokio::sync::oneshot::Sender<()>>,
) -> anyhow::Result<(Response, bool)>
where
    R: AsyncBufRead + Unpin,
    F: Future<Output = Response>,
{
    tokio::pin!(response);
    loop {
        tokio::select! {
            biased;
            response = &mut response => return Ok((response, false)),
            line = lines.next_line(), if queued.len() < MAX_QUEUED_PROXY_REQUESTS => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        stop_proxy_control(stop_control);
                        return Ok((response.await, true));
                    }
                    Err(error) => {
                        stop_proxy_control(stop_control);
                        // Even failed input must not drop the native response
                        // waiter until settlement or the supervisor deadline.
                        let _ = response.await;
                        return Err(error.into());
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(request) = serde_json::from_str::<Request>(&line) {
                    if cancels_request(&request, active_id) {
                        stop_proxy_control(stop_control);
                        return Ok((response.await, true));
                    }
                    if request.is_notification() {
                        continue;
                    }
                }
                queued.push_back(line);
            }
        }
    }
}

fn proxy_knows_tool(cached_tools_list: &serde_json::Value, name: &str) -> bool {
    if name == "type_text_chars" {
        return true;
    }
    cached_tools_list
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some(name))
        })
}

/// Own the proxy's single long-lived control connection. Connects directly to
/// the daemon socket (its own async open — `send_request` is sync, blocking,
/// and one-shot, so it cannot be reused here), binds `session_id`, and renews
/// the transport-owned lifecycle sessions at a bounded interval. The daemon
/// fires session cleanup when this connection reaches EOF, which the kernel
/// triggers on graceful proxy exit and on process death.
///
/// Any control-channel loss is terminal for the proxy. Continuing to forward
/// per-call requests after the daemon has reaped their owner would produce a
/// stream of misleading ended-session failures.
async fn run_control_connection(
    socket_path: String,
    session_id: String,
    control_ready: tokio::sync::oneshot::Sender<()>,
) -> anyhow::Result<()> {
    run_control_connection_with_timing(
        socket_path,
        session_id,
        control_ready,
        ControlConnectionTiming::default(),
    )
    .await
}

async fn run_control_connection_with_timing(
    socket_path: String,
    session_id: String,
    control_ready: tokio::sync::oneshot::Sender<()>,
    timing: ControlConnectionTiming,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use tokio::net::UnixStream;
        // Retry the connect briefly — the daemon may still be spinning up
        // (mirrors the windows pipe-open retry below). The is_daemon_listening
        // precheck makes the window tiny, but keep both paths symmetric.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let stream = loop {
            match UnixStream::connect(&socket_path).await {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => {
                    debug!(session_id = %session_id, "control connect failed (daemon starting?): {e}");
                    return Err(anyhow::anyhow!(
                        "connect MCP control session to daemon: {e}"
                    ));
                }
            }
        };
        return maintain_control_connection(stream, session_id, control_ready, timing).await;
    }

    #[cfg(all(not(unix), target_os = "windows"))]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        // Retry the pipe open briefly — the daemon may still be spinning up its
        // next instance (mirrors send_request's open-retry).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let client = loop {
            match ClientOptions::new().open(&socket_path) {
                Ok(c) => break c,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => {
                    debug!(session_id = %session_id, "control pipe open failed (daemon starting?): {e}");
                    return Err(anyhow::anyhow!("open MCP control session named pipe: {e}"));
                }
            }
        };
        return maintain_control_connection(client, session_id, control_ready, timing).await;
    }

    #[cfg(all(not(unix), not(target_os = "windows")))]
    {
        let _ = (session_id, socket_path, control_ready, timing);
        anyhow::bail!("daemon-backed MCP control sessions are not supported on this platform");
    }
}

async fn maintain_control_connection<S>(
    stream: S,
    session_id: String,
    control_ready: tokio::sync::oneshot::Sender<()>,
    timing: ControlConnectionTiming,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if timing.heartbeat_interval.is_zero() || timing.response_timeout.is_zero() {
        anyhow::bail!("MCP control-session timing must be greater than zero");
    }

    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let begin = control_request("session_begin", &session_id);
    write_control_request(&mut writer, &begin).await?;
    let response = read_control_response(&mut lines, timing.response_timeout).await?;
    validate_control_ack(&response, "session_begin")?;
    control_ready
        .send(())
        .map_err(|_| anyhow::anyhow!("MCP control-session owner stopped before acknowledgement"))?;
    debug!(session_id = %session_id, "control connection established and acknowledged");

    let heartbeat = control_request("session_heartbeat", &session_id);
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + timing.heartbeat_interval,
        timing.heartbeat_interval,
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(None) => anyhow::bail!("daemon closed the MCP control session"),
                    Err(error) => return Err(anyhow::anyhow!(
                        "read MCP control session: {error}"
                    )),
                    Ok(Some(_)) => anyhow::bail!(
                        "daemon sent an unsolicited MCP control-session response"
                    ),
                }
            }
            _ = interval.tick() => {
                write_control_request(&mut writer, &heartbeat).await?;
                let response = read_control_response(&mut lines, timing.response_timeout).await?;
                validate_control_ack(&response, "session_heartbeat")?;
                debug!(session_id = %session_id, "MCP control session renewed");
            }
        }
    }
}

fn control_request(method: &str, session_id: &str) -> DaemonRequest {
    DaemonRequest {
        method: method.to_owned(),
        name: None,
        args: None,
        session_id: Some(session_id.to_owned()),
        observation_origin: None,
        client_kind: None,
    }
}

async fn write_control_request<W>(writer: &mut W, request: &DaemonRequest) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let line = serde_json::to_string(request)
        .map_err(|error| anyhow::anyhow!("serialize MCP control request: {error}"))?;
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|error| anyhow::anyhow!("write MCP control request: {error}"))?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|error| anyhow::anyhow!("write MCP control request delimiter: {error}"))?;
    writer
        .flush()
        .await
        .map_err(|error| anyhow::anyhow!("flush MCP control request: {error}"))
}

async fn read_control_response<R>(
    lines: &mut tokio::io::Lines<BufReader<R>>,
    timeout: Duration,
) -> anyhow::Result<DaemonResponse>
where
    R: AsyncRead + Unpin,
{
    let line = tokio::time::timeout(timeout, lines.next_line())
        .await
        .map_err(|_| anyhow::anyhow!("daemon timed out acknowledging the MCP control session"))?
        .map_err(|error| anyhow::anyhow!("read MCP control-session response: {error}"))?
        .ok_or_else(|| anyhow::anyhow!("daemon closed the MCP control session"))?;
    serde_json::from_str(&line)
        .map_err(|error| anyhow::anyhow!("decode MCP control-session response: {error}"))
}

fn validate_control_ack(response: &DaemonResponse, method: &str) -> anyhow::Result<()> {
    if !response.ok {
        anyhow::bail!(
            "daemon rejected MCP control method `{method}`: {}",
            response
                .error
                .as_deref()
                .unwrap_or("daemon reported failure")
        );
    }
    if response
        .result
        .as_ref()
        .and_then(|result| result.get(method))
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        anyhow::bail!("daemon returned an invalid `{method}` acknowledgement");
    }
    Ok(())
}

/// Mint a session id unique among the live proxies sharing one daemon, for the
/// lifetime of this proxy process. `pid + process-start nanos` is dep-free and
/// sufficient: two proxies can't share a pid concurrently, and the nanos guard
/// disambiguates pid reuse across the daemon's lifetime. We deliberately avoid
/// the `uuid` crate — a single v4 mint isn't worth a new dependency.
fn mint_session_id() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("mcp-{pid}-{nanos}")
}

/// One-shot daemon `list` over the UDS, reshaped into a MCP
/// `tools/list` result. The daemon now returns the full ToolDef
/// (`name`, `description`, `input_schema`, annotation hints) per
/// commit 3's `serve.rs` change.
fn fetch_tools_list_from_daemon(
    socket_path: &str,
    session_id: &str,
) -> anyhow::Result<(serde_json::Value, bool)> {
    let req = DaemonRequest {
        method: "list".into(),
        name: None,
        args: None,
        session_id: Some(session_id.to_owned()),
        observation_origin: None,
        client_kind: None,
    };
    let resp = send_request(socket_path, &req)?;
    if !resp.ok {
        anyhow::bail!(
            "daemon refused tool list on {socket_path}: {}",
            resp.error.unwrap_or_else(|| "(no error message)".into())
        );
    }
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("daemon list response missing `result` field"))?;
    let tools_array = result
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("daemon list response missing `tools` array"))?;

    // Reshape the daemon's `{name, description, input_schema, output_schema,
    // read_only, ..., capabilities}` envelope into MCP's `{name, description,
    // inputSchema, outputSchema, annotations: {...}, capabilities}` shape.
    // Same translation `ToolDef::to_list_entry` defines for the core protocol.
    //
    // `capabilities` is passed through verbatim when the daemon
    // provides it; older daemons that don't emit the field fall back
    // to a name-keyed lookup so the proxy still surfaces capability
    // metadata without an extra round-trip.
    let mcp_tools: Vec<serde_json::Value> = tools_array
        .iter()
        .map(|t| {
            let name = t.get("name").cloned().unwrap_or(serde_json::Value::Null);
            let description = t
                .get("description")
                .cloned()
                .unwrap_or(serde_json::Value::String(String::new()));
            let input_schema = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
            let read_only = t
                .get("read_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let destructive = t
                .get("destructive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let idempotent = t
                .get("idempotent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let open_world = t
                .get("open_world")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let capabilities = t
                .get("capabilities")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_else(|| {
                    // Fallback: derive from the centralised name + schema
                    // resolver. Keeps the proxy compatible with daemon
                    // builds that pre-date the capabilities field.
                    name.as_str()
                        .map(|name| {
                            cua_driver_core::tool::advertised_capabilities_for(name, &input_schema)
                        })
                        .unwrap_or_default()
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect()
                });
            let risk = t.get("risk").cloned().unwrap_or_else(|| {
                name.as_str()
                    .map(cua_driver_core::authorization::risk_metadata_json)
                    .unwrap_or_else(|| {
                        serde_json::json!({
                            "class": "unclassified",
                            "enforcement": "metadata_only",
                            "operation_sensitive": false,
                            "version": cua_driver_core::authorization::RISK_METADATA_VERSION,
                        })
                    })
            });
            let mut tool = serde_json::json!({
                "name": name,
                "description": description,
                "inputSchema": input_schema,
                "annotations": {
                    "readOnlyHint": read_only,
                    "destructiveHint": destructive,
                    "idempotentHint": idempotent,
                    "openWorldHint": open_world,
                },
                "capabilities": capabilities,
                "risk": risk,
            });
            // Do not derive a new schema when an older daemon omitted it:
            // mixed-version proxies must advertise only the result contract
            // that the executing daemon actually owns.
            if let Some(output_schema) = t.get("output_schema") {
                tool.as_object_mut()
                    .expect("MCP tool entry is an object")
                    .insert("outputSchema".into(), output_schema.clone());
            }
            tool
        })
        .collect();

    // `capability_version` and `schema_version` are passed through
    // when the daemon emits them; older daemons fall back to the
    // proxy's compiled-in `CAPABILITY_VERSION` so MCP clients always
    // see the envelope keys.
    let capability_version = result
        .get("capability_version")
        .cloned()
        .unwrap_or_else(|| {
            serde_json::Value::String(cua_driver_core::tool::CAPABILITY_VERSION.to_owned())
        });
    let schema_version = result.get("schema_version").cloned().unwrap_or_else(|| {
        serde_json::Value::String(cua_driver_core::tool::TOOLS_LIST_SCHEMA_VERSION.to_owned())
    });

    let daemon_observes_tool_calls = daemon_owns_tool_observation(&result);

    Ok((
        serde_json::json!({
            "tools": mcp_tools,
            "capability_version": capability_version,
            "schema_version": schema_version,
        }),
        daemon_observes_tool_calls,
    ))
}

fn daemon_owns_tool_observation(result: &serde_json::Value) -> bool {
    result
        .get("tool_observation_owner")
        .and_then(serde_json::Value::as_str)
        == Some("daemon")
}

/// JSON-RPC method dispatcher for the proxy. Mirrors
/// `cua_driver_core::server::handle_request`:
/// - `initialize` → static `initialize_result()` (same envelope as the core
///   protocol server; the daemon's identity is hidden from the MCP client).
/// - `tools/list` → return the cached daemon tool list.
/// - `tools/call` → forward to the daemon and reshape the response into MCP's
///   `CallTool.Result`.
/// - other → method-not-found.
async fn handle_proxy_request(
    req: Request,
    id: serde_json::Value,
    socket_path: &str,
    cached_tools_list: &Arc<serde_json::Value>,
    session_id: &str,
    daemon_observes_tool_calls: bool,
) -> Response {
    match req.method.as_str() {
        "initialize" => {
            let mut result = initialize_result();
            result["capabilities"]["experimental"] = serde_json::json!({
                "cua/native-request-cancellation": {
                    "version": 1,
                    "method": "notifications/cancelled",
                    "settlement": "response_then_exit",
                    "retires_transport": true,
                }
            });
            Response::ok(id, result)
        }

        "tools/list" => Response::ok(id, (**cached_tools_list).clone()),

        "tools/call" => match req.tool_call() {
            Err(e) => Response::error(id, -32602, format!("Invalid params: {e}")),
            Ok(call) => {
                if let Err(error) = authorize_tool_call(&call.name, &call.args) {
                    return Response::error(id, -32603, error.to_string());
                }
                forward_tool_call(
                    id,
                    call.name,
                    call.args,
                    socket_path,
                    session_id,
                    daemon_observes_tool_calls,
                )
                .await
            }
        },

        other => {
            warn!(method = other, "unknown method");
            Response::method_not_found(id, other)
        }
    }
}

/// Forward a single MCP `tools/call` to the daemon as a `call`
/// request, then translate the `DaemonResponse` back into an MCP
/// `CallTool.Result` envelope.
///
/// Error mapping:
///   - Tool ran and reported failure (`!resp.ok`, including unknown
///     tool / bad params) → JSON-RPC success with `result.isError =
///     true`. Mirrors the core protocol's tool-error envelope.
///   - Transport failure (UDS unreachable, decode error, blocking
///     task panic) → JSON-RPC error (`-32603`), because the MCP
///     client really does need to distinguish "tool said no" from
///     "I couldn't reach the tool at all."
async fn forward_tool_call(
    id: serde_json::Value,
    name: String,
    mut args: serde_json::Value,
    socket_path: &str,
    session_id: &str,
    daemon_observes_tool_calls: bool,
) -> Response {
    cua_driver_core::tool_args::sanitize_reserved_args(&mut args);
    let req = DaemonRequest {
        method: "call".into(),
        name: Some(name.clone()),
        args: Some(args),
        session_id: Some(session_id.to_owned()),
        observation_origin: daemon_observes_tool_calls.then_some(ToolObservationOrigin::McpProxy),
        client_kind: None,
    };

    // Keep the per-call response channel asynchronous: cancellation closes only
    // the separate control owner and awaits this response. A settlement timeout
    // can then retire the proxy without leaving a 120s detached blocking reader
    // that would keep Tokio shutdown alive. No response means no settlement proof.
    let resp = match send_proxy_request(socket_path, &req).await {
        Err(e) => {
            return Response::error(
                id,
                -32603,
                format!("daemon transport error forwarding `{name}`: {e}"),
            );
        }
        Ok(r) => r,
    };

    if !resp.ok {
        // MCP separates two failure modes:
        //   - JSON-RPC errors → `Response::error(...)`, used for
        //     transport / protocol failures (unknown method, bad
        //     params shape, server crash).
        //   - Tool-level errors → `Response::ok(...)` carrying a
        //     `CallTool.Result` with `isError: true` and the error
        //     message in `content[]`. The tool ran, returned a
        //     well-formed result that says "I failed."
        //
        // A non-`ok` daemon response means the tool call reached the
        // daemon and the daemon decided the tool returned an error
        // (or rejected the call). That's tool-level, not transport-
        // level, so the core protocol surfaces it as `Response::ok` with
        // `isError: true`. Mirror that shape here — CodeRabbit #2.
        let msg = resp
            .error
            .unwrap_or_else(|| "daemon reported failure".into());
        let exit_code = resp.exit_code.unwrap_or(1);
        let result = serde_json::json!({
            "content": [{ "type": "text", "text": msg }],
            "isError": true,
            "structuredContent": { "exit_code": exit_code }
        });
        return Response::ok(id, result);
    }

    let result = resp.result.unwrap_or_else(|| {
        serde_json::json!({
            "content": [],
            "isError": false
        })
    });
    Response::ok(id, result)
}

async fn send_proxy_request(
    socket_path: &str,
    request: &DaemonRequest,
) -> anyhow::Result<DaemonResponse> {
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(socket_path).await?;
        return exchange_proxy_request(stream, request).await;
    }
    #[cfg(all(not(unix), target_os = "windows"))]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let stream = loop {
            match ClientOptions::new().open(socket_path) {
                Ok(stream) => break stream,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        return exchange_proxy_request(stream, request).await;
    }
    #[cfg(all(not(unix), not(target_os = "windows")))]
    {
        let _ = (socket_path, request);
        anyhow::bail!("daemon proxy is not supported on this platform");
    }
}

async fn exchange_proxy_request<S>(
    mut stream: S,
    request: &DaemonRequest,
) -> anyhow::Result<DaemonResponse>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let line = serde_json::to_string(request)? + "\n";
    tokio::time::timeout(Duration::from_secs(120), async {
        stream.write_all(line.as_bytes()).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out writing daemon request"))??;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    let bytes = tokio::time::timeout(Duration::from_secs(120), reader.read_line(&mut response))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for daemon response"))??;
    if bytes == 0 {
        anyhow::bail!("daemon closed connection without response");
    }
    Ok(serde_json::from_str(response.trim_end())?)
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// The daemon-backed integration harness exercises the full proxy lifecycle.
// These tests lock in the I/O loop's transport contract and the per-branch
// response reshaping without requiring a live daemon.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::DaemonResponse;

    #[tokio::test]
    async fn proxy_loop_returns_promptly_on_clean_eof() {
        let reader = BufReader::new(&b""[..]);
        let mut writer = Vec::new();
        let cached_tools = Arc::new(serde_json::json!({"tools": []}));
        let (stop_control, _stop_rx) = tokio::sync::oneshot::channel();

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            run_proxy_io(
                reader,
                &mut writer,
                "unused.sock",
                &cached_tools,
                "eof-test-session",
                false,
                stop_control,
            ),
        )
        .await
        .expect("clean EOF must return promptly");

        assert!(result.is_ok(), "clean EOF must not error: {result:?}");
        assert!(writer.is_empty(), "no request means no output");
    }

    #[tokio::test]
    async fn proxy_loop_serves_initialize_before_eof() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n";
        let reader = BufReader::new(&input[..]);
        let mut writer = Vec::new();
        let cached_tools = Arc::new(serde_json::json!({"tools": []}));
        let (stop_control, _stop_rx) = tokio::sync::oneshot::channel();

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            run_proxy_io(
                reader,
                &mut writer,
                "unused.sock",
                &cached_tools,
                "initialize-test-session",
                false,
                stop_control,
            ),
        )
        .await
        .expect("initialize followed by EOF must return promptly");

        assert!(
            result.is_ok(),
            "initialize exchange must not error: {result:?}"
        );
        let response: serde_json::Value =
            serde_json::from_slice(&writer).expect("response must be JSON");
        assert_eq!(response["id"], 1);
        assert!(response.get("result").is_some());
        assert_eq!(
            response["result"]["capabilities"]["experimental"]["cua/native-request-cancellation"]
                ["settlement"],
            "response_then_exit"
        );
    }

    #[tokio::test]
    async fn control_connection_renews_after_the_interval_and_fails_on_eof() {
        let (client, server) = tokio::io::duplex(4096);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let timing = ControlConnectionTiming {
            heartbeat_interval: Duration::from_millis(40),
            response_timeout: Duration::from_millis(250),
        };
        let control = tokio::spawn(maintain_control_connection(
            client,
            "heartbeat-test-session".to_owned(),
            ready_tx,
            timing,
        ));

        let (server_reader, mut server_writer) = tokio::io::split(server);
        let mut server_lines = BufReader::new(server_reader).lines();
        let begin_line = server_lines
            .next_line()
            .await
            .expect("read session_begin")
            .expect("session_begin line");
        let begin: DaemonRequest = serde_json::from_str(&begin_line).expect("decode session_begin");
        assert_eq!(begin.method, "session_begin");
        assert_eq!(begin.session_id.as_deref(), Some("heartbeat-test-session"));
        server_writer
            .write_all(
                (serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "session_begin": true
                })))
                .unwrap()
                    + "\n")
                    .as_bytes(),
            )
            .await
            .expect("write session_begin ack");
        server_writer
            .flush()
            .await
            .expect("flush session_begin ack");
        ready_rx.await.expect("control connection ready");

        assert!(
            tokio::time::timeout(Duration::from_millis(10), server_lines.next_line())
                .await
                .is_err(),
            "the heartbeat must not fire immediately"
        );
        let heartbeat_line =
            tokio::time::timeout(Duration::from_millis(200), server_lines.next_line())
                .await
                .expect("heartbeat deadline")
                .expect("read heartbeat")
                .expect("heartbeat line");
        let heartbeat: DaemonRequest =
            serde_json::from_str(&heartbeat_line).expect("decode heartbeat");
        assert_eq!(heartbeat.method, "session_heartbeat");
        assert_eq!(
            heartbeat.session_id.as_deref(),
            Some("heartbeat-test-session")
        );
        server_writer
            .write_all(
                (serde_json::to_string(&DaemonResponse::ok(serde_json::json!({
                    "session_heartbeat": true,
                    "renewed_sessions": 1
                })))
                .unwrap()
                    + "\n")
                    .as_bytes(),
            )
            .await
            .expect("write heartbeat ack");
        server_writer.flush().await.expect("flush heartbeat ack");
        drop(server_writer);
        drop(server_lines);

        let error = tokio::time::timeout(Duration::from_millis(250), control)
            .await
            .expect("control task must notice EOF")
            .expect("control task join")
            .expect_err("control EOF must be terminal");
        assert!(error.to_string().contains("closed"));
    }

    #[tokio::test]
    async fn control_loss_fails_the_proxy_supervisor_closed() {
        let proxy_io = std::future::pending::<anyhow::Result<()>>();
        let control_task =
            tokio::spawn(async { Err::<(), _>(anyhow::anyhow!("test control channel lost")) });
        let (_stop_tx, stop_rx) = tokio::sync::oneshot::channel();

        let error = tokio::time::timeout(
            Duration::from_millis(250),
            supervise_proxy_io(proxy_io, control_task, stop_rx),
        )
        .await
        .expect("supervisor must stop promptly")
        .expect_err("control loss must fail the proxy");
        assert!(error.to_string().contains("test control channel lost"));
    }

    #[test]
    fn cancellation_requires_notification_and_exact_typed_request_id() {
        for (wire, expected) in [
            (
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#,
                true,
            ),
            (
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"7"}}"#,
                false,
            ),
            (
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":8}}"#,
                false,
            ),
            (
                r#"{"jsonrpc":"2.0","id":8,"method":"notifications/cancelled","params":{"requestId":7}}"#,
                false,
            ),
            (
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#,
                false,
            ),
        ] {
            let request = serde_json::from_str(wire).unwrap();
            assert_eq!(cancels_request(&request, &serde_json::json!(7)), expected);
        }
    }

    struct SignalControlClosed(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for SignalControlClosed {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn matching_cancel_closes_control_but_waits_for_native_response() {
        let (proxy_input, mut client_input) = tokio::io::duplex(4096);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let control_guard = SignalControlClosed(Some(closed_tx));
        let control_task = tokio::spawn(async move {
            let _control_guard = control_guard;
            std::future::pending::<anyhow::Result<()>>().await
        });
        let proxy_io = async move {
            let mut lines = BufReader::new(proxy_input).lines();
            let mut queued = VecDeque::new();
            let mut stop_tx = Some(stop_tx);
            let (response, retired) = await_proxy_response(
                &mut lines,
                &mut queued,
                &serde_json::json!(7),
                async { response_rx.await.unwrap() },
                &mut stop_tx,
            )
            .await?;
            assert!(retired);
            assert_eq!(response.id, serde_json::json!(7));
            assert!(matches!(
                response.body,
                cua_driver_core::protocol::ResponseBody::Result { .. }
            ));
            Ok(())
        };
        let supervisor = tokio::spawn(supervise_proxy_io(proxy_io, control_task, stop_rx));
        client_input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":7}}\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(250), closed_rx)
            .await
            .expect("cancel must close the control connection promptly")
            .unwrap();
        assert!(
            !supervisor.is_finished(),
            "control EOF is not native settlement"
        );
        response_tx
            .send(Response::ok(
                serde_json::json!(7),
                serde_json::json!({"isError": true}),
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_millis(250), supervisor)
            .await
            .expect("normal native response permits proxy retirement")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn eof_during_request_signals_end_and_preserves_response_waiter() {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let pending = tokio::spawn(async move {
            let mut lines = BufReader::new(&b""[..]).lines();
            await_proxy_response(
                &mut lines,
                &mut VecDeque::new(),
                &serde_json::json!("request"),
                async { response_rx.await.unwrap() },
                &mut Some(stop_tx),
            )
            .await
        });
        tokio::time::timeout(Duration::from_millis(250), stop_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(!pending.is_finished());
        response_tx
            .send(Response::ok(
                serde_json::json!("request"),
                serde_json::json!({}),
            ))
            .unwrap();
        let (response, retired) = pending.await.unwrap().unwrap();
        assert!(retired);
        assert_eq!(response.id, serde_json::json!("request"));
    }

    #[tokio::test]
    async fn cancellation_discards_queued_work_instead_of_reusing_ended_owner() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"tools/call\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":7}}\n";
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let pending = tokio::spawn(async move {
            let mut lines = BufReader::new(&input[..]).lines();
            let mut queued = VecDeque::new();
            let (response, retired) = await_proxy_response(
                &mut lines,
                &mut queued,
                &serde_json::json!(7),
                async { response_rx.await.unwrap() },
                &mut Some(stop_tx),
            )
            .await?;
            assert!(
                retired,
                "caller must retire instead of dispatching queued work"
            );
            assert_eq!(queued.len(), 1);
            Ok::<_, anyhow::Error>(response)
        });
        stop_rx.await.unwrap();
        response_tx
            .send(Response::ok(serde_json::json!(7), serde_json::json!({})))
            .unwrap();
        assert_eq!(pending.await.unwrap().unwrap().id, serde_json::json!(7));
    }

    #[tokio::test]
    async fn deliberate_stop_timeout_is_not_a_settled_response() {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let control_task = tokio::spawn(std::future::pending::<anyhow::Result<()>>());
        let proxy_io = async move {
            stop_tx.send(()).unwrap();
            std::future::pending::<anyhow::Result<()>>().await
        };
        let error = supervise_proxy_io_with_timeout(
            proxy_io,
            control_task,
            stop_rx,
            Duration::from_millis(20),
        )
        .await
        .expect_err("missing native response must not be treated as cancellation settlement");
        assert!(error.to_string().contains("without settlement proof"));
    }

    #[tokio::test]
    async fn async_daemon_exchange_preserves_normal_response_envelope() {
        let (client, server) = tokio::io::duplex(4096);
        let request = control_request("call", "test-session");
        let exchange = tokio::spawn(async move { exchange_proxy_request(client, &request).await });
        let (reader, mut writer) = tokio::io::split(server);
        let mut lines = BufReader::new(reader).lines();
        let sent: DaemonRequest =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(sent.session_id.as_deref(), Some("test-session"));
        let expected = DaemonResponse::ok(serde_json::json!({"message":"settled 已结束"}));
        let payload = serde_json::to_vec(&expected).unwrap();
        for chunk in payload.chunks(3) {
            writer.write_all(chunk).await.unwrap();
        }
        writer.write_all(b"\n").await.unwrap();
        let response = exchange.await.unwrap().unwrap();
        assert!(response.ok);
        assert_eq!(response.result, expected.result);
    }

    /// Reconstruct the `!resp.ok` branch in isolation so we can assert
    /// on the serialized shape without spinning up a real daemon /
    /// tokio runtime. Keep this in sync with `forward_tool_call`.
    fn build_tool_error_response(id: serde_json::Value, resp: DaemonResponse) -> Response {
        let msg = resp
            .error
            .unwrap_or_else(|| "daemon reported failure".into());
        let exit_code = resp.exit_code.unwrap_or(1);
        let result = serde_json::json!({
            "content": [{ "type": "text", "text": msg }],
            "isError": true,
            "structuredContent": { "exit_code": exit_code }
        });
        Response::ok(id, result)
    }

    #[test]
    fn daemon_tool_failure_wraps_as_jsonrpc_success_with_iserror_true() {
        let daemon_resp = DaemonResponse {
            ok: false,
            result: None,
            error: Some("missing required field `pid`".into()),
            exit_code: Some(64),
        };
        let resp = build_tool_error_response(serde_json::json!(7), daemon_resp);
        let value = serde_json::to_value(&resp).expect("serialize");

        // Top-level JSON-RPC envelope: success (`result`), not error.
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], serde_json::json!(7));
        assert!(
            value.get("error").is_none(),
            "tool-level failure must NOT surface as JSON-RPC error: got {value}"
        );
        assert!(
            value.get("result").is_some(),
            "tool-level failure must carry a `result` payload: got {value}"
        );

        // CallTool.Result inside `result`: isError + content text.
        let result = &value["result"];
        assert_eq!(result["isError"], serde_json::json!(true));
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "missing required field `pid`");
        assert_eq!(result["structuredContent"]["exit_code"], 64);
    }

    #[test]
    fn daemon_failure_with_no_error_message_uses_fallback_text() {
        let daemon_resp = DaemonResponse {
            ok: false,
            result: None,
            error: None,
            exit_code: None,
        };
        let resp = build_tool_error_response(serde_json::json!("abc"), daemon_resp);
        let value = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(value["result"]["isError"], serde_json::json!(true));
        assert_eq!(
            value["result"]["content"][0]["text"],
            "daemon reported failure"
        );
        assert_eq!(value["result"]["structuredContent"]["exit_code"], 1);
    }

    #[test]
    fn cached_proxy_tool_allowlist_is_exact() {
        let cached = serde_json::json!({
            "tools": [
                {"name":"click"},
                {"name":"type_text"}
            ]
        });
        assert!(proxy_knows_tool(&cached, "click"));
        assert!(
            proxy_knows_tool(&cached, "type_text_chars"),
            "deprecated alias stays bounded/known"
        );
        assert!(!proxy_knows_tool(&cached, "click/private-user-value"));
        assert!(!proxy_knows_tool(&cached, ""));
    }

    #[test]
    fn observation_ownership_requires_the_daemon_capability() {
        assert!(daemon_owns_tool_observation(&serde_json::json!({
            "tool_observation_owner": "daemon"
        })));
        assert!(!daemon_owns_tool_observation(&serde_json::json!({})));
        assert!(!daemon_owns_tool_observation(&serde_json::json!({
            "tool_observation_owner": "proxy"
        })));
    }
}
