//! Runtime management for outbound MCP client sessions.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    process::Stdio,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::future::join_all;
use http::{HeaderName, HeaderValue};
use mcp_host_core::{
    BatchToolCall, BatchToolCallOutcome, BatchToolCallResponse, BatchToolCallResult,
    ConnectDisposition, ConnectResult, DesiredConnection, DisconnectDisposition, DisconnectResult,
    Lifecycle, LifecycleState, MAX_BATCH_CALLS, McpServerRegistry, RegisteredServer,
    ResolvedTransportConfig, RuntimeError, RuntimeErrorCode, ServerId, ServerInspection,
    ServerSummary, ToolCallResult, ToolDefinition, ToolSnapshot, TransportKind,
};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{
    ClientHandler, Peer, ServiceError, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, ClientRequest, ServerInfo, ServerResult, Tool,
    },
    service::{PeerRequestOptions, RoleClient, RunningService},
    transport::{StreamableHttpClientTransport, TokioChildProcess},
};
use serde_json::{Value, json};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, Notify},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

static NEXT_INVOCATION_ID: AtomicU64 = AtomicU64::new(1);

/// Runtime limits for downstream MCP servers.
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    /// Maximum time allowed for a transport to connect and initialize.
    pub connect_timeout: Duration,
    /// Default timeout for an individual tool request.
    pub request_timeout: Duration,
    /// Maximum time allowed for graceful session shutdown.
    pub shutdown_grace: Duration,
    /// Largest caller-supplied tool request timeout.
    pub max_request_timeout: Duration,
    /// Number of latest stderr bytes retained for internal diagnostics.
    pub stderr_tail_bytes: usize,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(4),
            max_request_timeout: Duration::from_secs(300),
            stderr_tail_bytes: 8_192,
        }
    }
}

/// Owns the independent runtimes associated with one immutable registry snapshot.
pub struct RuntimeManager {
    registry: Arc<McpServerRegistry>,
    servers: BTreeMap<String, Arc<ServerRuntime>>,
}

impl RuntimeManager {
    /// Creates one runtime state machine per registered server.
    #[must_use]
    pub fn new(registry: Arc<McpServerRegistry>, settings: RuntimeSettings) -> Arc<Self> {
        let servers = registry
            .iter()
            .map(|server| {
                let id = server.id().as_str().to_owned();
                (
                    id.clone(),
                    Arc::new(ServerRuntime::new(id, server.clone(), settings.clone())),
                )
            })
            .collect();
        Arc::new(Self { registry, servers })
    }

    /// Returns the immutable registry used by this manager.
    #[must_use]
    pub fn registry(&self) -> Arc<McpServerRegistry> {
        Arc::clone(&self.registry)
    }

    /// Lists servers in normalized ID order.
    pub async fn list_servers(&self) -> Vec<ServerSummary> {
        let futures = self.servers.values().map(|runtime| runtime.summary());
        join_all(futures).await
    }

    /// Returns public, secret-free metadata and current runtime state for a server.
    pub async fn inspect_server(&self, server_id: &str) -> Result<ServerInspection, RuntimeError> {
        self.server(server_id, "inspect_server")?.inspection().await
    }

    /// Connects, initializes, and discovers all tools for a server.
    pub async fn connect_server(&self, server_id: &str) -> Result<ConnectResult, RuntimeError> {
        let started = Instant::now();
        let result = self.server(server_id, "connect_server")?.connect().await;
        trace_operation("connect_server", server_id, started, &result);
        result
    }

    /// Gracefully disconnects a server, cancelling an in-progress startup if necessary.
    pub async fn disconnect_server(
        &self,
        server_id: &str,
    ) -> Result<DisconnectResult, RuntimeError> {
        let started = Instant::now();
        let result = self
            .server(server_id, "disconnect_server")?
            .disconnect()
            .await;
        trace_operation("disconnect_server", server_id, started, &result);
        result
    }

    /// Returns the cached tools, optionally refreshing the cache from the server.
    pub async fn list_tools(
        &self,
        server_id: &str,
        refresh: bool,
    ) -> Result<ToolSnapshot, RuntimeError> {
        let runtime = self.server(server_id, "list_tools")?;
        if refresh {
            return runtime.refresh().await;
        }
        runtime.cached_tools().await
    }

    /// Refreshes the discovered tool cache.
    pub async fn refresh_server(&self, server_id: &str) -> Result<ToolSnapshot, RuntimeError> {
        let started = Instant::now();
        let result = self.server(server_id, "refresh_server")?.refresh().await;
        trace_operation("refresh_server", server_id, started, &result);
        result
    }

    /// Calls a discovered tool with object arguments.
    pub async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        arguments: Value,
        timeout_ms: Option<u64>,
    ) -> Result<ToolCallResult, RuntimeError> {
        let started = Instant::now();
        let invocation_id = NEXT_INVOCATION_ID.fetch_add(1, Ordering::Relaxed);
        let argument_bytes = serde_json::to_vec(&arguments).map_or(0, |value| value.len());
        let result = self
            .server(server_id, "call_tool")?
            .call_tool(tool_name, arguments, timeout_ms)
            .await;
        let result_bytes = result
            .as_ref()
            .ok()
            .and_then(|result| serde_json::to_vec(result.value()).ok())
            .map_or(0, |value| value.len());
        let error_code = result
            .as_ref()
            .err()
            .map_or("", |error| error.code.as_str());
        tracing::info!(
            operation = "call_tool",
            server_id,
            tool_name,
            invocation_id,
            argument_bytes,
            result_bytes,
            duration_ms = duration_ms(started.elapsed()),
            success = result.is_ok(),
            error_code
        );
        result
    }

    /// Calls up to 32 discovered tools concurrently while preserving input order.
    pub async fn call_tools(
        &self,
        calls: Vec<BatchToolCall>,
    ) -> Result<BatchToolCallResponse, RuntimeError> {
        if calls.is_empty() || calls.len() > MAX_BATCH_CALLS {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidArguments,
                "call_tools",
                format!("batch must contain between 1 and {MAX_BATCH_CALLS} tool calls"),
            ));
        }

        let results = join_all(calls.into_iter().map(|call| async move {
            let BatchToolCall {
                server_id,
                tool_name,
                arguments,
                timeout_ms,
            } = call;
            let outcome = match self
                .call_tool(&server_id, &tool_name, arguments, timeout_ms)
                .await
            {
                Ok(result) => BatchToolCallOutcome::Success { result },
                Err(error) => BatchToolCallOutcome::Error { error },
            };
            BatchToolCallResult {
                server_id,
                tool_name,
                outcome,
            }
        }))
        .await;

        Ok(BatchToolCallResponse { results })
    }

    /// Gracefully disconnects all registered servers concurrently.
    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        let results = join_all(self.servers.values().map(|runtime| runtime.disconnect())).await;
        if let Some(error) = results.into_iter().find_map(Result::err) {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ShutdownFailed,
                "shutdown",
                "one or more downstream servers did not shut down cleanly",
            )
            .with_source_summary(error.code.as_str()));
        }
        Ok(())
    }

    /// Returns the number of registered servers.
    #[must_use]
    pub fn server_count(&self) -> u64 {
        self.registry.len() as u64
    }

    /// Returns the number of currently connected servers.
    pub async fn connected_count(&self) -> u64 {
        join_all(self.servers.values().map(|runtime| runtime.state()))
            .await
            .into_iter()
            .filter(|state| *state == LifecycleState::Connected)
            .count() as u64
    }

    /// Returns the number of servers with a recorded runtime failure.
    pub async fn failed_count(&self) -> u64 {
        join_all(self.servers.values().map(|runtime| runtime.state()))
            .await
            .into_iter()
            .filter(|state| *state == LifecycleState::Failed)
            .count() as u64
    }

    fn server(&self, server_id: &str, operation: &str) -> Result<Arc<ServerRuntime>, RuntimeError> {
        let normalized = ServerId::parse(server_id)
            .map(|id| id.as_str().to_owned())
            .unwrap_or_else(|_| server_id.to_owned());
        self.servers.get(&normalized).cloned().ok_or_else(|| {
            RuntimeError::for_server(
                RuntimeErrorCode::ServerNotFound,
                operation,
                server_id,
                "the server is not registered",
            )
        })
    }
}

struct ServerRuntime {
    id: String,
    registered: RegisteredServer,
    settings: RuntimeSettings,
    state: Mutex<RuntimeState>,
    changed: Notify,
    tools: Arc<Mutex<Option<ToolSnapshot>>>,
}

struct RuntimeState {
    lifecycle: Lifecycle,
    session: Option<ManagedSession>,
    operation: Option<Operation>,
    epoch: u64,
    protocol: Value,
    upstream: Value,
    pid: Option<u32>,
    connected_at_unix_ms: Option<u64>,
    disconnected_at_unix_ms: Option<u64>,
    last_safe_error: Option<RuntimeError>,
}

struct Operation {
    kind: OperationKind,
    epoch: u64,
    cancel: CancellationToken,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OperationKind {
    Connect,
    Disconnect,
}

struct ManagedSession {
    service: RunningService<RoleClient, ClientEvents>,
    close_token: CancellationToken,
    monitor: JoinHandle<()>,
    stderr: Option<JoinHandle<()>>,
    _stderr_tail: Option<Arc<Mutex<VecDeque<u8>>>>,
}

#[derive(Clone)]
struct ClientEvents {
    tools: Arc<Mutex<Option<ToolSnapshot>>>,
}

impl ClientHandler for ClientEvents {
    async fn on_tool_list_changed(&self, _context: rmcp::service::NotificationContext<RoleClient>) {
        if let Some(snapshot) = self.tools.lock().await.as_mut() {
            snapshot.stale = true;
        }
    }
}

impl ServerRuntime {
    fn new(id: String, registered: RegisteredServer, settings: RuntimeSettings) -> Self {
        Self {
            id,
            registered,
            settings,
            state: Mutex::new(RuntimeState {
                lifecycle: Lifecycle::default(),
                session: None,
                operation: None,
                epoch: 0,
                protocol: Value::Null,
                upstream: Value::Null,
                pid: None,
                connected_at_unix_ms: None,
                disconnected_at_unix_ms: None,
                last_safe_error: None,
            }),
            changed: Notify::new(),
            tools: Arc::new(Mutex::new(None)),
        }
    }

    async fn summary(&self) -> ServerSummary {
        let state = self.state.lock().await;
        let tools = self.tools.lock().await;
        ServerSummary {
            id: self.id.clone(),
            name: self.registered.resolved_manifest().name.clone(),
            description: self.registered.resolved_manifest().description.clone(),
            enabled: self.registered.enabled(),
            transport: transport_kind(&self.registered),
            desired_state: state.lifecycle.desired(),
            observed_state: state.lifecycle.state(),
            tool_count: tools.as_ref().map_or(0, |snapshot| snapshot.tool_count),
            tools_stale: tools.as_ref().is_some_and(|snapshot| snapshot.stale),
        }
    }

    async fn inspection(&self) -> Result<ServerInspection, RuntimeError> {
        let state = self.state.lock().await;
        let tools = self.tools.lock().await.clone();
        Ok(ServerInspection {
            server_id: self.id.clone(),
            public_manifest: public_manifest(&self.registered),
            source: self.registered.source_path().display().to_string(),
            transport: transport_kind(&self.registered),
            enabled: self.registered.enabled(),
            desired_state: state.lifecycle.desired(),
            observed_state: state.lifecycle.state(),
            protocol: state.protocol.clone(),
            upstream: state.upstream.clone(),
            tool_snapshot: tools,
            last_safe_error: state.last_safe_error.clone(),
            pid: state.pid,
            connected_at_unix_ms: state.connected_at_unix_ms,
            disconnected_at_unix_ms: state.disconnected_at_unix_ms,
        })
    }

    async fn state(&self) -> LifecycleState {
        self.state.lock().await.lifecycle.state()
    }

    async fn connect(self: &Arc<Self>) -> Result<ConnectResult, RuntimeError> {
        if !self.registered.enabled() {
            return Err(self.error(
                RuntimeErrorCode::ServerDisabled,
                "connect_server",
                "the server is disabled",
            ));
        }

        loop {
            let action = {
                let mut state = self.state.lock().await;
                if let Some(operation) = &state.operation {
                    ConnectAction::Wait {
                        epoch: operation.epoch,
                        joined_connect: operation.kind == OperationKind::Connect,
                    }
                } else {
                    match state.lifecycle.request_connect() {
                        ConnectDisposition::AlreadyConnected => {
                            return self.connect_result(&state).await;
                        }
                        ConnectDisposition::JoinExisting => ConnectAction::Wait {
                            epoch: state.epoch,
                            joined_connect: true,
                        },
                        ConnectDisposition::Start => {
                            state.epoch = state.epoch.saturating_add(1);
                            let epoch = state.epoch;
                            let cancel = CancellationToken::new();
                            state.operation = Some(Operation {
                                kind: OperationKind::Connect,
                                epoch,
                                cancel: cancel.clone(),
                            });
                            state.last_safe_error = None;
                            ConnectAction::Start {
                                epoch,
                                cancel,
                                old_session: state.session.take(),
                            }
                        }
                    }
                }
            };

            match action {
                ConnectAction::Wait {
                    epoch,
                    joined_connect,
                } => {
                    self.wait_for_operation(epoch).await;
                    if joined_connect {
                        let state = self.state.lock().await;
                        return self.connect_result(&state).await;
                    }
                }
                ConnectAction::Start {
                    epoch,
                    cancel,
                    old_session,
                } => {
                    mark_stale(&self.tools).await;
                    if let Some(session) = old_session {
                        let _ = close_session(session, self.settings.shutdown_grace).await;
                    }
                    let result = self.start_session(cancel).await;
                    return self.finish_connect(epoch, result).await;
                }
            }
        }
    }

    async fn start_session(
        self: &Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<(ManagedSession, Value, Value, Option<u32>), RuntimeError> {
        let events = ClientEvents {
            tools: Arc::clone(&self.tools),
        };
        let (mut service, pid, mut stderr, stderr_tail) =
            match self.registered.resolved_manifest().transport.clone() {
                ResolvedTransportConfig::Stdio {
                    command,
                    args,
                    working_directory,
                    environment,
                } => {
                    let mut command_line = tokio::process::Command::new(command);
                    command_line.args(args);
                    if let Some(directory) = working_directory {
                        command_line.current_dir(directory);
                    }
                    for (name, value) in &environment {
                        command_line.env(name, value.expose_secret());
                    }
                    let secrets = environment
                        .values()
                        .map(|value| value.expose_secret().to_owned())
                        .filter(|value| !value.is_empty())
                        .collect::<Vec<_>>();
                    let (transport, stderr) = TokioChildProcess::builder(command_line)
                        .stderr(Stdio::piped())
                        .spawn()
                        .map_err(|_| {
                            self.error(
                                RuntimeErrorCode::ServerStartFailed,
                                "connect_server",
                                "failed to start the downstream process",
                            )
                        })?;
                    let pid = transport.id();
                    tracing::info!(
                        operation = "process_start",
                        server_id = self.id,
                        transport = "stdio",
                        pid = pid.unwrap_or_default()
                    );
                    let (mut stderr, stderr_tail) = match stderr {
                        Some(stderr) => {
                            let tail = Arc::new(Mutex::new(VecDeque::with_capacity(
                                self.settings.stderr_tail_bytes,
                            )));
                            (
                                Some(spawn_stderr_drain(
                                    stderr,
                                    secrets,
                                    self.settings.stderr_tail_bytes,
                                    Arc::clone(&tail),
                                )),
                                Some(tail),
                            )
                        }
                        None => (None, None),
                    };
                    if let Err(error) = self.begin_initializing().await {
                        cancellation.cancel();
                        drop(transport);
                        if let Some(task) = stderr.take() {
                            await_task(task, self.settings.shutdown_grace).await;
                        }
                        return Err(error);
                    }
                    let service = match timeout(
                        self.settings.connect_timeout,
                        events.serve_with_ct(transport, cancellation.clone()),
                    )
                    .await
                    {
                        Ok(Ok(service)) => service,
                        outcome => {
                            cancellation.cancel();
                            if let Some(task) = stderr.take() {
                                await_task(task, self.settings.shutdown_grace).await;
                            }
                            let message = if outcome.is_err() {
                                "downstream initialization timed out"
                            } else {
                                "downstream initialization failed"
                            };
                            return Err(self.error(
                                RuntimeErrorCode::ServerInitializationFailed,
                                "connect_server",
                                message,
                            ));
                        }
                    };
                    (service, pid, stderr, stderr_tail)
                }
                ResolvedTransportConfig::Http { url, headers } => {
                    let headers = resolve_headers(headers, &self.id)?;
                    let config = StreamableHttpClientTransportConfig::with_uri(url.to_string())
                        .custom_headers(headers);
                    let transport = StreamableHttpClientTransport::from_config(config);
                    tracing::info!(
                        operation = "transport_start",
                        server_id = self.id,
                        transport = "http"
                    );
                    if let Err(error) = self.begin_initializing().await {
                        cancellation.cancel();
                        drop(transport);
                        return Err(error);
                    }
                    let service = timeout(
                        self.settings.connect_timeout,
                        events.serve_with_ct(transport, cancellation.clone()),
                    )
                    .await
                    .map_err(|_| {
                        self.error(
                            RuntimeErrorCode::HttpConnectionFailed,
                            "connect_server",
                            "HTTP initialization timed out",
                        )
                    })?
                    .map_err(|_| {
                        self.error(
                            RuntimeErrorCode::HttpConnectionFailed,
                            "connect_server",
                            "HTTP connection or initialization failed",
                        )
                    })?;
                    (service, None, None, None)
                }
            };

        let peer = service.peer().clone();
        let tools = match timeout(self.settings.connect_timeout, peer.list_all_tools()).await {
            Ok(Ok(tools)) => tools,
            Ok(Err(_)) => {
                close_unmanaged_service(&mut service, stderr.take(), self.settings.shutdown_grace)
                    .await;
                return Err(self.error(
                    RuntimeErrorCode::ServerInitializationFailed,
                    "connect_server",
                    "initial tool discovery failed",
                ));
            }
            Err(_) => {
                close_unmanaged_service(&mut service, stderr.take(), self.settings.shutdown_grace)
                    .await;
                return Err(self.error(
                    RuntimeErrorCode::ServerInitializationFailed,
                    "connect_server",
                    "initial tool discovery timed out",
                ));
            }
        };
        let snapshot = match tool_snapshot(&self.id, tools) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                close_unmanaged_service(&mut service, stderr.take(), self.settings.shutdown_grace)
                    .await;
                return Err(error);
            }
        };
        *self.tools.lock().await = Some(snapshot);

        let peer_info = peer.peer_info();
        let (protocol, upstream) = peer_info
            .as_deref()
            .map(safe_peer_info)
            .unwrap_or((Value::Null, Value::Null));
        let close_token = CancellationToken::new();
        let monitor = spawn_monitor(Arc::downgrade(self), peer, close_token.clone());
        Ok((
            ManagedSession {
                service,
                close_token,
                monitor,
                stderr,
                _stderr_tail: stderr_tail,
            },
            protocol,
            upstream,
            pid,
        ))
    }

    async fn finish_connect(
        &self,
        epoch: u64,
        result: Result<(ManagedSession, Value, Value, Option<u32>), RuntimeError>,
    ) -> Result<ConnectResult, RuntimeError> {
        let mut unused_session = None;
        let output = {
            let mut state = self.state.lock().await;
            let current = state.operation.as_ref().is_some_and(|operation| {
                operation.kind == OperationKind::Connect && operation.epoch == epoch
            });
            if !current {
                if let Ok((session, ..)) = result {
                    unused_session = Some(session);
                }
                Err(self.error(
                    RuntimeErrorCode::ServerUnavailable,
                    "connect_server",
                    "connection attempt was superseded",
                ))
            } else {
                state.operation = None;
                match result {
                    Ok((session, protocol, upstream, pid))
                        if state.lifecycle.desired() == DesiredConnection::Connected =>
                    {
                        let _ = state.lifecycle.transition_to(LifecycleState::Connected);
                        state.protocol = protocol;
                        state.upstream = upstream;
                        state.pid = pid;
                        state.connected_at_unix_ms = Some(unix_ms());
                        state.session = Some(session);
                        tracing::info!(
                            operation = "lifecycle_transition",
                            server_id = self.id,
                            state = "connected",
                            success = true
                        );
                        self.connect_result(&state).await
                    }
                    Ok((session, ..)) => {
                        unused_session = Some(session);
                        let _ = state.lifecycle.transition_to(LifecycleState::Stopped);
                        state.disconnected_at_unix_ms = Some(unix_ms());
                        Err(self.error(
                            RuntimeErrorCode::ServerUnavailable,
                            "connect_server",
                            "connection was cancelled",
                        ))
                    }
                    Err(_error) if state.lifecycle.desired() == DesiredConnection::Disconnected => {
                        let _ = state.lifecycle.transition_to(LifecycleState::Stopped);
                        state.disconnected_at_unix_ms = Some(unix_ms());
                        Err(self.error(
                            RuntimeErrorCode::ServerNotConnected,
                            "connect_server",
                            "connection was cancelled",
                        ))
                    }
                    Err(error) => {
                        let _ = state.lifecycle.fail(error.message.clone());
                        if state.last_safe_error.is_none() {
                            state.last_safe_error = Some(error.clone());
                        }
                        Err(error)
                    }
                }
            }
        };
        if let Some(session) = unused_session {
            close_session(session, self.settings.shutdown_grace).await;
        }
        self.changed.notify_waiters();
        output
    }

    async fn connect_result(&self, state: &RuntimeState) -> Result<ConnectResult, RuntimeError> {
        if state.lifecycle.state() != LifecycleState::Connected {
            if state.lifecycle.desired() == DesiredConnection::Disconnected {
                return Err(self.error(
                    RuntimeErrorCode::ServerNotConnected,
                    "connect_server",
                    "connection was cancelled",
                ));
            }
            return Err(state.last_safe_error.clone().unwrap_or_else(|| {
                self.error(
                    RuntimeErrorCode::ServerUnavailable,
                    "connect_server",
                    "the server did not connect",
                )
            }));
        }
        let snapshot = self.tools.lock().await.clone();
        Ok(ConnectResult {
            server_id: self.id.clone(),
            state: LifecycleState::Connected,
            tool_count: snapshot.as_ref().map_or(0, |snapshot| snapshot.tool_count),
            protocol_version: state
                .protocol
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            connected_at_unix_ms: state.connected_at_unix_ms,
            tool_snapshot: snapshot,
        })
    }

    async fn disconnect(self: &Arc<Self>) -> Result<DisconnectResult, RuntimeError> {
        loop {
            let action =
                {
                    let mut state = self.state.lock().await;
                    if let Some((kind, epoch, cancel)) = state.operation.as_ref().map(|operation| {
                        (operation.kind, operation.epoch, operation.cancel.clone())
                    }) {
                        if kind == OperationKind::Connect {
                            let _ = state.lifecycle.request_disconnect();
                            cancel.cancel();
                        }
                        DisconnectAction::Wait(epoch)
                    } else {
                        match state.lifecycle.request_disconnect() {
                            DisconnectDisposition::AlreadyInactive => {
                                return self.disconnect_result(&state);
                            }
                            DisconnectDisposition::JoinExisting
                            | DisconnectDisposition::CancelStartup => {
                                DisconnectAction::Wait(state.epoch)
                            }
                            DisconnectDisposition::Stop => {
                                state.epoch = state.epoch.saturating_add(1);
                                let epoch = state.epoch;
                                state.operation = Some(Operation {
                                    kind: OperationKind::Disconnect,
                                    epoch,
                                    cancel: CancellationToken::new(),
                                });
                                DisconnectAction::Stop {
                                    epoch,
                                    session: state.session.take(),
                                }
                            }
                        }
                    }
                };

            match action {
                DisconnectAction::Wait(epoch) => self.wait_for_operation(epoch).await,
                DisconnectAction::Stop { epoch, session } => {
                    let closed_cleanly = match session {
                        Some(session) => close_session(session, self.settings.shutdown_grace).await,
                        None => true,
                    };
                    let mut state = self.state.lock().await;
                    if state
                        .operation
                        .as_ref()
                        .is_some_and(|operation| operation.epoch == epoch)
                    {
                        state.operation = None;
                        if closed_cleanly {
                            if state.lifecycle.state() == LifecycleState::Connected {
                                let _ = state.lifecycle.transition_to(LifecycleState::Disconnected);
                            } else if state.lifecycle.state() == LifecycleState::Failed {
                                let _ = state.lifecycle.transition_to(LifecycleState::Stopped);
                            }
                            tracing::info!(
                                operation = "lifecycle_transition",
                                server_id = self.id,
                                state = "disconnected",
                                success = true
                            );
                        } else {
                            let error = self.error(
                                RuntimeErrorCode::ServerDisconnectFailed,
                                "disconnect_server",
                                "the downstream session did not close within the grace period",
                            );
                            let _ = state.lifecycle.fail(error.message.clone());
                            if state.last_safe_error.is_none() {
                                state.last_safe_error = Some(error.clone());
                            }
                            self.changed.notify_waiters();
                            return Err(error);
                        }
                        state.pid = None;
                        state.disconnected_at_unix_ms = Some(unix_ms());
                    }
                    mark_stale(&self.tools).await;
                    let result = self.disconnect_result(&state);
                    drop(state);
                    self.changed.notify_waiters();
                    return result;
                }
            }
        }
    }

    async fn cached_tools(&self) -> Result<ToolSnapshot, RuntimeError> {
        let state = self.state.lock().await;
        let snapshot = self.tools.lock().await.clone();
        match (state.lifecycle.state(), snapshot) {
            (LifecycleState::Connected, Some(snapshot)) => Ok(snapshot),
            (_, Some(mut snapshot)) => {
                snapshot.stale = true;
                Ok(snapshot)
            }
            (LifecycleState::Connected, None) => Err(self.error(
                RuntimeErrorCode::ToolsNotDiscovered,
                "list_tools",
                "tools have not been discovered",
            )),
            _ => Err(self.error(
                RuntimeErrorCode::ServerNotConnected,
                "list_tools",
                "the server is not connected",
            )),
        }
    }

    async fn refresh(&self) -> Result<ToolSnapshot, RuntimeError> {
        let peer = {
            let state = self.state.lock().await;
            if state.lifecycle.state() != LifecycleState::Connected {
                return Err(self.error(
                    RuntimeErrorCode::ServerNotConnected,
                    "refresh_server",
                    "the server is not connected",
                ));
            }
            state
                .session
                .as_ref()
                .map(|session| session.service.peer().clone())
                .ok_or_else(|| {
                    self.error(
                        RuntimeErrorCode::ServerNotConnected,
                        "refresh_server",
                        "the server session is unavailable",
                    )
                })?
        };
        let result = timeout(self.settings.request_timeout, peer.list_all_tools()).await;
        match result {
            Ok(Ok(tools)) => {
                let snapshot = tool_snapshot(&self.id, tools)?;
                *self.tools.lock().await = Some(snapshot.clone());
                Ok(snapshot)
            }
            Ok(Err(error)) => {
                mark_stale(&self.tools).await;
                if matches!(error, ServiceError::TransportClosed) {
                    self.mark_transport_closed().await;
                }
                Err(self.error(
                    RuntimeErrorCode::ProtocolError,
                    "refresh_server",
                    "tool refresh failed",
                ))
            }
            Err(_) => {
                mark_stale(&self.tools).await;
                Err(self.error(
                    RuntimeErrorCode::ToolCallTimeout,
                    "refresh_server",
                    "tool refresh timed out",
                ))
            }
        }
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        timeout_ms: Option<u64>,
    ) -> Result<ToolCallResult, RuntimeError> {
        if !self.registered.enabled() {
            return Err(self.error(
                RuntimeErrorCode::ServerDisabled,
                "call_tool",
                "the server is disabled",
            ));
        }
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            _ => {
                return Err(self.error(
                    RuntimeErrorCode::InvalidArguments,
                    "call_tool",
                    "tool arguments must be a JSON object",
                ));
            }
        };
        let (peer, found) = {
            let state = self.state.lock().await;
            if state.lifecycle.state() != LifecycleState::Connected {
                return Err(self.error(
                    RuntimeErrorCode::ServerNotConnected,
                    "call_tool",
                    "the server is not connected",
                ));
            }
            let peer = state
                .session
                .as_ref()
                .map(|session| session.service.peer().clone())
                .ok_or_else(|| {
                    self.error(
                        RuntimeErrorCode::ServerNotConnected,
                        "call_tool",
                        "the server session is unavailable",
                    )
                })?;
            let found =
                self.tools.lock().await.as_ref().is_some_and(|snapshot| {
                    snapshot.tools.iter().any(|tool| tool.name == tool_name)
                });
            (peer, found)
        };
        if !found {
            return Err(self.error(
                RuntimeErrorCode::ToolNotFound,
                "call_tool",
                "the requested tool was not discovered",
            ));
        }
        let timeout = request_timeout(timeout_ms, &self.settings, &self.id)?;
        let params = CallToolRequestParams::new(tool_name.to_owned()).with_arguments(arguments);
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let handle = match peer
            .send_cancellable_request(request, PeerRequestOptions::with_timeout(timeout))
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                if matches!(error, ServiceError::TransportClosed) {
                    self.mark_transport_closed().await;
                }
                return Err(self.request_error("call_tool", error));
            }
        };
        match handle.await_response().await {
            Ok(ServerResult::CallToolResult(result)) => serde_json::to_value(result)
                .map(ToolCallResult::new)
                .map_err(|_| {
                    self.error(
                        RuntimeErrorCode::ProtocolError,
                        "call_tool",
                        "tool result could not be serialized",
                    )
                }),
            Ok(_) => Err(self.error(
                RuntimeErrorCode::ProtocolError,
                "call_tool",
                "received an unexpected tool response",
            )),
            Err(ServiceError::Timeout { .. }) => Err(self.error(
                RuntimeErrorCode::ToolCallTimeout,
                "call_tool",
                "the downstream tool call timed out",
            )),
            Err(ServiceError::TransportClosed) => {
                self.mark_transport_closed().await;
                Err(self.error(
                    RuntimeErrorCode::TransportClosed,
                    "call_tool",
                    "the downstream transport closed during the tool call",
                ))
            }
            Err(_) => Err(self.error(
                RuntimeErrorCode::ToolCallFailed,
                "call_tool",
                "the downstream tool call failed",
            )),
        }
    }

    fn disconnect_result(&self, state: &RuntimeState) -> Result<DisconnectResult, RuntimeError> {
        Ok(DisconnectResult {
            server_id: self.id.clone(),
            state: state.lifecycle.state(),
            disconnected_at_unix_ms: state.disconnected_at_unix_ms,
        })
    }

    async fn mark_transport_closed(&self) {
        let mut state = self.state.lock().await;
        if state.lifecycle.desired() == DesiredConnection::Connected
            && state.lifecycle.state() == LifecycleState::Connected
        {
            let error = self.error(
                RuntimeErrorCode::TransportClosed,
                "monitor",
                "the downstream transport closed unexpectedly",
            );
            let _ = state.lifecycle.fail(error.message.clone());
            if state.last_safe_error.is_none() {
                state.last_safe_error = Some(error.clone());
            }
            mark_stale(&self.tools).await;
            self.changed.notify_waiters();
            tracing::warn!(
                operation = "lifecycle_transition",
                server_id = self.id,
                state = "failed",
                success = false,
                error_code = error.code.as_str()
            );
        }
    }

    fn error(&self, code: RuntimeErrorCode, operation: &str, message: &str) -> RuntimeError {
        RuntimeError::for_server(code, operation, &self.id, message)
    }

    fn request_error(&self, operation: &str, error: ServiceError) -> RuntimeError {
        match error {
            ServiceError::TransportClosed => self.error(
                RuntimeErrorCode::TransportClosed,
                operation,
                "the downstream transport is closed",
            ),
            ServiceError::Timeout { .. } => self.error(
                RuntimeErrorCode::ToolCallTimeout,
                operation,
                "the downstream request timed out",
            ),
            _ => self.error(
                RuntimeErrorCode::ToolCallFailed,
                operation,
                "the downstream request could not be sent",
            ),
        }
    }

    async fn begin_initializing(&self) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().await;
        if state.lifecycle.desired() != DesiredConnection::Connected {
            return Err(self.error(
                RuntimeErrorCode::ServerNotConnected,
                "connect_server",
                "connection was cancelled",
            ));
        }
        state
            .lifecycle
            .transition_to(LifecycleState::Initializing)
            .map_err(|_| {
                self.error(
                    RuntimeErrorCode::ServerInitializationFailed,
                    "connect_server",
                    "the server could not enter initialization",
                )
            })
    }

    async fn wait_for_operation(&self, epoch: u64) {
        loop {
            let notified = self.changed.notified();
            let active = self
                .state
                .lock()
                .await
                .operation
                .as_ref()
                .is_some_and(|operation| operation.epoch == epoch);
            if !active {
                return;
            }
            notified.await;
        }
    }
}

enum ConnectAction {
    Wait {
        epoch: u64,
        joined_connect: bool,
    },
    Start {
        epoch: u64,
        cancel: CancellationToken,
        old_session: Option<ManagedSession>,
    },
}

enum DisconnectAction {
    Wait(u64),
    Stop {
        epoch: u64,
        session: Option<ManagedSession>,
    },
}

fn transport_kind(server: &RegisteredServer) -> TransportKind {
    match server.resolved_manifest().transport {
        ResolvedTransportConfig::Stdio { .. } => TransportKind::Stdio,
        ResolvedTransportConfig::Http { .. } => TransportKind::Http,
    }
}

fn public_manifest(server: &RegisteredServer) -> Value {
    let manifest = server.resolved_manifest();
    let transport = match &manifest.transport {
        ResolvedTransportConfig::Stdio {
            command,
            args,
            working_directory,
            environment,
        } => json!({
            "type": "stdio",
            "command": command,
            "args": args,
            "workingDirectory": working_directory,
            "environmentKeys": environment.keys().collect::<Vec<_>>(),
        }),
        ResolvedTransportConfig::Http { url, headers } => json!({
            "type": "http",
            "scheme": url.scheme(),
            "host": url.host_str(),
            "port": url.port(),
            "path": url.path(),
            "headerNames": headers.keys().collect::<Vec<_>>(),
        }),
    };
    json!({
        "id": manifest.id.as_str(),
        "name": manifest.name,
        "description": manifest.description,
        "enabled": manifest.enabled,
        "transport": transport,
    })
}

fn resolve_headers(
    headers: std::collections::BTreeMap<String, mcp_host_core::SecretValue>,
    server_id: &str,
) -> Result<HashMap<HeaderName, HeaderValue>, RuntimeError> {
    headers
        .into_iter()
        .map(|(name, value)| {
            let name = HeaderName::try_from(name).map_err(|_| {
                RuntimeError::for_server(
                    RuntimeErrorCode::HttpConnectionFailed,
                    "connect_server",
                    server_id,
                    "the configured HTTP header name is invalid",
                )
            })?;
            let value = HeaderValue::try_from(value.expose_secret()).map_err(|_| {
                RuntimeError::for_server(
                    RuntimeErrorCode::HttpConnectionFailed,
                    "connect_server",
                    server_id,
                    "the configured HTTP header value is invalid",
                )
            })?;
            Ok((name, value))
        })
        .collect()
}

fn tool_snapshot(server_id: &str, tools: Vec<Tool>) -> Result<ToolSnapshot, RuntimeError> {
    let definitions = tools
        .into_iter()
        .map(tool_definition)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ToolSnapshot {
        server_id: server_id.to_owned(),
        fetched_at_unix_ms: unix_ms(),
        tool_count: definitions.len() as u64,
        tools: definitions,
        stale: false,
    })
}

fn tool_definition(tool: Tool) -> Result<ToolDefinition, RuntimeError> {
    Ok(ToolDefinition {
        name: tool.name.into_owned(),
        title: tool.title,
        description: tool.description.map(|description| description.into_owned()),
        input_schema: Value::Object(tool.input_schema.as_ref().clone()),
        output_schema: tool
            .output_schema
            .map(|schema| Value::Object(schema.as_ref().clone())),
        annotations: value_or_none(tool.annotations)?,
        execution: value_or_none(tool.execution)?,
        icons: value_or_none(tool.icons)?,
        meta: value_or_none(tool.meta)?,
    })
}

fn value_or_none<T: serde::Serialize>(value: Option<T>) -> Result<Option<Value>, RuntimeError> {
    value.map(serde_json::to_value).transpose().map_err(|_| {
        RuntimeError::new(
            RuntimeErrorCode::ProtocolError,
            "tool_conversion",
            "tool metadata could not be serialized",
        )
    })
}

fn safe_peer_info(info: &ServerInfo) -> (Value, Value) {
    let protocol = json!({ "version": info.protocol_version.as_str() });
    let upstream = serde_json::to_value(json!({
        "serverInfo": info.server_info,
        "capabilities": info.capabilities,
        "instructions": info.instructions,
    }))
    .unwrap_or(Value::Null);
    (protocol, upstream)
}

fn request_timeout(
    timeout_ms: Option<u64>,
    settings: &RuntimeSettings,
    server_id: &str,
) -> Result<Duration, RuntimeError> {
    let Some(milliseconds) = timeout_ms else {
        return Ok(settings.request_timeout);
    };
    if !(1..=300_000).contains(&milliseconds)
        || Duration::from_millis(milliseconds) > settings.max_request_timeout
    {
        return Err(RuntimeError::for_server(
            RuntimeErrorCode::InvalidArguments,
            "call_tool",
            server_id,
            "timeout_ms must be between 1 and 300000 milliseconds",
        ));
    }
    Ok(Duration::from_millis(milliseconds))
}

fn spawn_monitor(
    runtime: Weak<ServerRuntime>,
    peer: Peer<RoleClient>,
    close_token: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = close_token.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if peer.is_transport_closed() {
                        if let Some(runtime) = runtime.upgrade() {
                            runtime.mark_transport_closed().await;
                        }
                        return;
                    }
                }
            }
        }
    })
}

fn spawn_stderr_drain(
    mut stderr: tokio::process::ChildStderr,
    secrets: Vec<String>,
    capacity: usize,
    tail: Arc<Mutex<VecDeque<u8>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 1024];
        let mut pending = Vec::new();
        loop {
            let read = match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            pending.extend_from_slice(&buffer[..read]);
            drain_redacted_tail(&mut pending, &secrets, capacity, &tail, false).await;
        }
        drain_redacted_tail(&mut pending, &secrets, capacity, &tail, true).await;
    })
}

async fn drain_redacted_tail(
    pending: &mut Vec<u8>,
    secrets: &[String],
    capacity: usize,
    tail: &Mutex<VecDeque<u8>>,
    end_of_stream: bool,
) {
    let mut sanitized = Vec::new();
    sanitize_pending(pending, secrets, end_of_stream, &mut sanitized);
    let mut tail = tail.lock().await;
    for byte in sanitized {
        if tail.len() == capacity {
            let _ = tail.pop_front();
        }
        if capacity != 0 {
            tail.push_back(byte);
        }
    }
}

fn sanitize_pending(
    pending: &mut Vec<u8>,
    secrets: &[String],
    end_of_stream: bool,
    sanitized: &mut Vec<u8>,
) {
    const REDACTED: &[u8] = b"<redacted>";
    let mut consumed = 0;
    while consumed < pending.len() {
        let remaining = &pending[consumed..];
        let complete = secrets
            .iter()
            .filter(|secret| remaining.starts_with(secret.as_bytes()))
            .map(String::len)
            .max();
        let can_extend = secrets.iter().any(|secret| {
            secret.as_bytes().starts_with(remaining) && secret.len() > remaining.len()
        });

        if can_extend {
            if end_of_stream {
                sanitized.extend_from_slice(REDACTED);
                consumed = pending.len();
            }
            break;
        }
        if let Some(length) = complete {
            consumed += length;
            sanitized.extend_from_slice(REDACTED);
        } else {
            sanitized.push(remaining[0]);
            consumed += 1;
        }
    }
    pending.drain(..consumed);
}

async fn close_session(mut session: ManagedSession, grace: Duration) -> bool {
    session.close_token.cancel();
    let closed_cleanly = matches!(session.service.close_with_timeout(grace).await, Ok(Some(_)));
    await_task(session.monitor, grace).await;
    if let Some(stderr) = session.stderr.take() {
        await_task(stderr, grace).await;
    }
    closed_cleanly
}

async fn close_unmanaged_service(
    service: &mut RunningService<RoleClient, ClientEvents>,
    stderr: Option<JoinHandle<()>>,
    grace: Duration,
) {
    let _ = service.close_with_timeout(grace).await;
    if let Some(stderr) = stderr {
        await_task(stderr, grace).await;
    }
}

async fn await_task(handle: JoinHandle<()>, grace: Duration) {
    let mut handle = handle;
    if timeout(grace, &mut handle).await.is_err() {
        handle.abort();
        let _ = handle.await;
    }
}

async fn mark_stale(tools: &Mutex<Option<ToolSnapshot>>) {
    if let Some(snapshot) = tools.lock().await.as_mut() {
        snapshot.stale = true;
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn trace_operation<T>(
    operation: &'static str,
    server_id: &str,
    started: Instant,
    result: &Result<T, RuntimeError>,
) {
    tracing::info!(
        operation,
        server_id,
        duration_ms = duration_ms(started.elapsed()),
        success = result.is_ok(),
        error_code = result
            .as_ref()
            .err()
            .map_or("", |error| error.code.as_str())
    );
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use mcp_host_core::{
        EnvironmentAccessError, EnvironmentProvider, ManifestLoader, McpServerRegistry,
        RegistryBuilder,
    };
    use rmcp::model::Tool;
    use serde_json::{Map, json};
    use tempfile::tempdir;

    use super::{RuntimeManager, RuntimeSettings, sanitize_pending, tool_definition};

    struct EmptyEnvironment;

    impl EnvironmentProvider for EmptyEnvironment {
        fn get(&self, _name: &str) -> Result<Option<String>, EnvironmentAccessError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn unknown_and_disabled_servers_are_rejected() {
        let manager = manager("disabled", false);
        let unknown = manager
            .connect_server("missing")
            .await
            .expect_err("unknown must fail");
        let disabled = manager
            .connect_server("disabled")
            .await
            .expect_err("disabled must fail");
        assert_eq!(unknown.code.as_str(), "SERVER_NOT_FOUND");
        assert_eq!(disabled.code.as_str(), "SERVER_DISABLED");
    }

    #[tokio::test]
    async fn invalid_tool_arguments_are_rejected_before_connection() {
        let manager = manager("enabled", true);
        let error = manager
            .call_tool("enabled", "tool", json!("not-an-object"), None)
            .await
            .expect_err("invalid arguments must fail");
        assert_eq!(error.code.as_str(), "INVALID_ARGUMENTS");
    }

    #[tokio::test]
    async fn server_entries_are_deterministic() {
        let manager = manager_with("zeta", true, "alpha", true);
        let entries = manager.list_servers().await;
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
    }

    #[test]
    fn tool_conversion_preserves_optional_metadata() {
        let tool = Tool::new("echo", "Echo", Arc::new(Map::new())).with_title("Echo title");
        let definition = tool_definition(tool).expect("tool should convert");
        assert_eq!(definition.name, "echo");
        assert_eq!(definition.title.as_deref(), Some("Echo title"));
        assert_eq!(definition.input_schema, json!({}));
    }

    #[test]
    fn stderr_redaction_handles_secrets_split_across_read_chunks() {
        let secrets = vec!["sentinel-secret".to_owned()];
        let mut pending = b"prefix-sentinel".to_vec();
        let mut sanitized = Vec::new();
        sanitize_pending(&mut pending, &secrets, false, &mut sanitized);
        assert_eq!(sanitized, b"prefix-");

        pending.extend_from_slice(b"-secret-suffix");
        sanitize_pending(&mut pending, &secrets, false, &mut sanitized);
        assert_eq!(sanitized, b"prefix-<redacted>-suffix");
        assert!(pending.is_empty());
    }

    #[test]
    fn stderr_redaction_hides_a_partial_secret_at_end_of_stream() {
        let secrets = vec!["sentinel-secret".to_owned()];
        let mut pending = b"sentinel-sec".to_vec();
        let mut sanitized = Vec::new();
        sanitize_pending(&mut pending, &secrets, true, &mut sanitized);
        assert_eq!(sanitized, b"<redacted>");
    }

    #[test]
    fn stderr_redaction_handles_overlapping_secret_prefixes() {
        let secrets = vec!["ab".to_owned(), "abcde".to_owned()];
        let mut pending = b"prefix-abc".to_vec();
        let mut sanitized = Vec::new();
        sanitize_pending(&mut pending, &secrets, false, &mut sanitized);
        assert_eq!(sanitized, b"prefix-");

        pending.extend_from_slice(b"de-suffix");
        sanitize_pending(&mut pending, &secrets, false, &mut sanitized);
        assert_eq!(sanitized, b"prefix-<redacted>-suffix");
        assert!(pending.is_empty());
    }

    fn manager(id: &str, enabled: bool) -> Arc<RuntimeManager> {
        let registry = registry(&[(id, enabled)]);
        RuntimeManager::new(Arc::new(registry), RuntimeSettings::default())
    }

    fn manager_with(
        first: &str,
        first_enabled: bool,
        second: &str,
        second_enabled: bool,
    ) -> Arc<RuntimeManager> {
        RuntimeManager::new(
            Arc::new(registry(&[
                (first, first_enabled),
                (second, second_enabled),
            ])),
            RuntimeSettings::default(),
        )
    }

    fn registry(entries: &[(&str, bool)]) -> McpServerRegistry {
        let directory = tempdir().expect("temporary directory should be created");
        for (index, (id, enabled)) in entries.iter().enumerate() {
            fs::write(
                directory.path().join(format!("{index}.toml")),
                format!(
                    "id = {id:?}\nname = {id:?}\ndescription = \"test\"\nenabled = {enabled}\n[transport]\ntype = \"stdio\"\ncommand = \"unused\"\n"
                ),
            )
            .expect("manifest should be written");
        }
        let loaded = ManifestLoader::new(EmptyEnvironment)
            .load_directory(directory.path())
            .expect("manifests should load");
        RegistryBuilder::build(loaded).expect("registry should build")
    }
}
