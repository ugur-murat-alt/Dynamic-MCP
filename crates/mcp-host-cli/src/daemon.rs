use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use fs2::FileExt as _;
use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::tokio::Listener as _;
use mcp_host_core::{
    CONTROL_PROTOCOL_VERSION, ControlRequest, ControlRequestEnvelope, ControlResponseEnvelope,
    ManifestLoader, Policy, ProcessEnvironment, RegistryBuilder, RuntimeError, RuntimeErrorCode,
    SkillCatalog,
};
use mcp_host_mcp::{HostMcpServer, HostRuntimeState, RuntimeManager, RuntimeSettings};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use rmcp::ServiceExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::ipc::{EndpointKind, EndpointSet, read_json, write_json};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Filesystem locations required to start a daemon instance.
#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub config_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

/// Secret-free daemon state written while the local endpoints are live.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DaemonMetadata {
    pub pid: u32,
    pub started_at_unix_ms: u64,
    pub control_protocol_version: u32,
    pub control_endpoint: String,
    pub mcp_endpoint: String,
    pub config_dir: PathBuf,
    pub binary_version: String,
}

/// Runs the singleton local daemon until a signal or control shutdown request arrives.
pub async fn run_daemon(options: DaemonOptions) -> Result<(), RuntimeError> {
    run_daemon_inner(options, None, None).await
}

/// Runs the daemon with an optional platform service cancellation source.
pub async fn run_daemon_with_shutdown(
    options: DaemonOptions,
    external_shutdown: Option<CancellationToken>,
) -> Result<(), RuntimeError> {
    run_daemon_inner(options, external_shutdown, None).await
}

#[cfg(any(windows, test))]
pub(crate) async fn run_daemon_with_ready(
    options: DaemonOptions,
    external_shutdown: CancellationToken,
    ready: tokio::sync::oneshot::Sender<()>,
) -> Result<(), RuntimeError> {
    run_daemon_inner(options, Some(external_shutdown), Some(ready)).await
}

async fn run_daemon_inner(
    options: DaemonOptions,
    external_shutdown: Option<CancellationToken>,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<(), RuntimeError> {
    let cancellation = external_shutdown.unwrap_or_default();
    let auth_root = oauth_data_dir();
    prepare_runtime_dir(&options.runtime_dir)?;

    let lock_path = options.runtime_dir.join("daemon.lock");
    let lock = acquire_lock(&lock_path)?;
    let endpoints = match EndpointSet::for_runtime_dir(&options.runtime_dir) {
        Ok(endpoints) => endpoints,
        Err(_) => {
            release_lock(lock, &lock_path);
            return Err(ipc_error(
                "endpoint_setup",
                "failed to prepare daemon endpoints",
            ));
        }
    };
    let mut files = DaemonFiles::new(
        lock,
        lock_path,
        options.runtime_dir.join("daemon.json"),
        endpoints,
    );

    if files.endpoints.cleanup_stale().is_err() {
        files.cleanup();
        return Err(ipc_error(
            "cleanup_stale",
            "failed to clean stale daemon endpoints",
        ));
    }

    let (registry, policy, skills) = match load_configuration(&options.config_dir) {
        Ok(configuration) => configuration,
        Err(error) => {
            files.cleanup();
            return Err(error);
        }
    };
    let runtime = RuntimeManager::new_with_configuration(
        Arc::new(registry),
        RuntimeSettings {
            package_root: Some(options.runtime_dir.join("packages")),
            auth_root,
            ..RuntimeSettings::default()
        },
        policy,
        skills,
    );
    let state = Arc::new(HostRuntimeState::new());

    let control_listener = match files.endpoints.bind(EndpointKind::Control) {
        Ok(listener) => listener,
        Err(_) => {
            files.cleanup();
            return Err(ipc_error(
                "bind_control",
                "failed to bind the control endpoint",
            ));
        }
    };
    let mcp_listener = match files.endpoints.bind(EndpointKind::Mcp) {
        Ok(listener) => listener,
        Err(_) => {
            drop(control_listener);
            files.cleanup();
            return Err(ipc_error("bind_mcp", "failed to bind the MCP endpoint"));
        }
    };

    let metadata = DaemonMetadata {
        pid: std::process::id(),
        started_at_unix_ms: unix_ms(),
        control_protocol_version: CONTROL_PROTOCOL_VERSION,
        control_endpoint: files.endpoints.address(EndpointKind::Control).to_owned(),
        mcp_endpoint: files.endpoints.address(EndpointKind::Mcp).to_owned(),
        config_dir: options.config_dir.clone(),
        binary_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    if let Err(error) = write_metadata(&files.metadata_path, &metadata) {
        drop(control_listener);
        drop(mcp_listener);
        files.cleanup();
        return Err(error);
    }

    let (watcher, reload_task) = match spawn_config_watcher(
        options.config_dir.clone(),
        Arc::clone(&runtime),
        cancellation.clone(),
    ) {
        Ok(watcher) => watcher,
        Err(error) => {
            drop(control_listener);
            drop(mcp_listener);
            files.cleanup();
            return Err(error);
        }
    };
    state.set_control_ready(true);
    state.set_mcp_ready(true);
    let control_accept = tokio::spawn(control_accept_loop(
        control_listener,
        Arc::clone(&runtime),
        Arc::clone(&state),
        cancellation.clone(),
    ));
    let mcp_accept = tokio::spawn(mcp_accept_loop(
        mcp_listener,
        Arc::clone(&runtime),
        Arc::clone(&state),
        cancellation.clone(),
    ));
    if let Some(ready) = ready {
        let _ = ready.send(());
    }

    wait_for_shutdown_event(cancellation.clone()).await;
    state.set_control_ready(false);
    state.set_mcp_ready(false);
    state.set_shutting_down(true);
    cancellation.cancel();
    drop(watcher);

    wait_for_accept_task(control_accept, "control_accept_shutdown").await;
    wait_for_accept_task(mcp_accept, "mcp_accept_shutdown").await;
    wait_for_accept_task(reload_task, "config_reload_shutdown").await;
    let shutdown = timeout(SHUTDOWN_TIMEOUT, runtime.shutdown()).await;
    files.cleanup();

    match shutdown {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(RuntimeError::new(
            RuntimeErrorCode::ShutdownFailed,
            "shutdown",
            "one or more downstream servers did not shut down cleanly",
        )),
        Err(_) => Err(RuntimeError::new(
            RuntimeErrorCode::ShutdownFailed,
            "shutdown",
            "downstream server shutdown timed out",
        )),
    }
}

fn oauth_data_dir() -> Option<PathBuf> {
    ProjectDirs::from("org", "mcp-host", "mcp-host")
        .map(|directories| directories.data_local_dir().join("auth"))
}

struct DaemonFiles {
    lock: Option<File>,
    lock_path: PathBuf,
    metadata_path: PathBuf,
    endpoints: EndpointSet,
}

impl DaemonFiles {
    fn new(lock: File, lock_path: PathBuf, metadata_path: PathBuf, endpoints: EndpointSet) -> Self {
        Self {
            lock: Some(lock),
            lock_path,
            metadata_path,
            endpoints,
        }
    }

    fn cleanup(&mut self) {
        let _ = fs::remove_file(&self.metadata_path);
        let _ = self.endpoints.cleanup_stale();
        if let Some(lock) = self.lock.take() {
            release_lock(lock, &self.lock_path);
        }
    }
}

async fn control_accept_loop(
    listener: Listener,
    runtime: Arc<RuntimeManager>,
    state: Arc<HostRuntimeState>,
    cancellation: CancellationToken,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            result = listener.accept() => match result {
                Ok(stream) => {
                    connections.spawn(handle_control_connection(
                        stream,
                        Arc::clone(&runtime),
                        Arc::clone(&state),
                        cancellation.clone(),
                    ));
                }
                Err(_) => tracing::warn!(operation = "control_accept", code = "IPC_UNAVAILABLE"),
            },
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if result.is_err() {
                    tracing::warn!(operation = "control_connection", code = "IPC_UNAVAILABLE");
                }
            }
        }
    }
    drain_tasks(&mut connections, "control_connection_shutdown").await;
}

async fn mcp_accept_loop(
    listener: Listener,
    runtime: Arc<RuntimeManager>,
    state: Arc<HostRuntimeState>,
    cancellation: CancellationToken,
) {
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            result = listener.accept() => match result {
                Ok(stream) => {
                    sessions.spawn(serve_mcp_session(
                        stream,
                        Arc::clone(&runtime),
                        Arc::clone(&state),
                        cancellation.clone(),
                    ));
                }
                Err(_) => tracing::warn!(operation = "mcp_accept", code = "IPC_UNAVAILABLE"),
            },
            Some(result) = sessions.join_next(), if !sessions.is_empty() => {
                if result.is_err() {
                    tracing::warn!(operation = "mcp_session", code = "TRANSPORT_CLOSED");
                }
            }
        }
    }
    drain_tasks(&mut sessions, "mcp_session_shutdown").await;
}

async fn handle_control_connection(
    mut stream: Stream,
    runtime: Arc<RuntimeManager>,
    state: Arc<HostRuntimeState>,
    cancellation: CancellationToken,
) {
    let request = match read_json::<_, ControlRequestEnvelope>(&mut stream).await {
        Ok(request) => request,
        Err(_) => {
            tracing::debug!(operation = "control_decode", code = "IPC_PROTOCOL_MISMATCH");
            return;
        }
    };
    let started = Instant::now();
    let request_id = request.request_id.clone();
    let operation = control_operation(&request.request);
    let request_bytes = serde_json::to_vec(&request).map_or(0, |value| value.len());

    let is_shutdown = request.protocol_version == CONTROL_PROTOCOL_VERSION
        && matches!(&request.request, ControlRequest::Shutdown);
    let response = if request.protocol_version != CONTROL_PROTOCOL_VERSION {
        ControlResponseEnvelope::failure(request.request_id, protocol_mismatch_error())
    } else if cancellation.is_cancelled() && !allowed_during_shutdown(&request.request) {
        ControlResponseEnvelope::failure(request.request_id, daemon_shutting_down_error())
    } else {
        match dispatch_request(&runtime, &state, request.request).await {
            Ok(result) => ControlResponseEnvelope::success(request.request_id, result),
            Err(error) => ControlResponseEnvelope::failure(request.request_id, error),
        }
    };

    let success = response.error.is_none();
    let error_code = response
        .error
        .as_ref()
        .map_or("", |error| error.code.as_str());
    let response_bytes = serde_json::to_vec(&response).map_or(0, |value| value.len());
    if is_shutdown {
        cancellation.cancel();
    }
    if write_json(&mut stream, &response).await.is_err() {
        tracing::debug!(operation = "control_write", code = "IPC_UNAVAILABLE");
        return;
    }
    tracing::info!(
        operation,
        request_id,
        request_bytes,
        response_bytes,
        duration_ms = duration_ms(started.elapsed()),
        success,
        error_code
    );
}

fn load_configuration(
    config_dir: &Path,
) -> Result<(mcp_host_core::McpServerRegistry, Policy, SkillCatalog), RuntimeError> {
    let manifests = ManifestLoader::new(ProcessEnvironment)
        .load_directory(config_dir)
        .map_err(|_| config_error("load_manifests"))?;
    let registry = RegistryBuilder::build(manifests).map_err(|_| config_error("build_registry"))?;
    let policy = Policy::load_optional(config_dir).map_err(|_| config_error("load_policy"))?;
    let skills =
        SkillCatalog::load_directory(config_dir).map_err(|_| config_error("load_skills"))?;
    skills
        .validate_server_references(&registry)
        .map_err(|_| config_error("validate_skills"))?;
    Ok((registry, policy, skills))
}

fn spawn_config_watcher(
    config_dir: PathBuf,
    runtime: Arc<RuntimeManager>,
    cancellation: CancellationToken,
) -> Result<(RecommendedWatcher, JoinHandle<()>), RuntimeError> {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |result| {
        let _ = sender.send(result);
    })
    .map_err(|_| config_error("watch_config"))?;
    watcher
        .watch(&config_dir, RecursiveMode::NonRecursive)
        .map_err(|_| config_error("watch_config"))?;

    let task = tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                _ = cancellation.cancelled() => return,
                event = receiver.recv() => event,
            };
            let Some(event) = event else {
                return;
            };
            if event.is_err() {
                tracing::warn!(operation = "config_watch", code = "PROTOCOL_ERROR");
                continue;
            }

            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
            while receiver.try_recv().is_ok() {}

            match load_configuration(&config_dir) {
                Ok((registry, policy, skills)) => {
                    let result = runtime
                        .reload_configuration(Arc::new(registry), policy, skills)
                        .await;
                    tracing::info!(
                        operation = "config_reload",
                        generation = result.generation,
                        added = result.added,
                        removed = result.removed,
                        changed = result.changed,
                        policy_changed = result.policy_changed,
                        skills_changed = result.skills_changed,
                        success = true
                    );
                }
                Err(error) => tracing::warn!(
                    operation = "config_reload",
                    code = error.code.as_str(),
                    success = false
                ),
            }
        }
    });
    Ok((watcher, task))
}

async fn serve_mcp_session(
    stream: Stream,
    runtime: Arc<RuntimeManager>,
    state: Arc<HostRuntimeState>,
    cancellation: CancellationToken,
) {
    let _session = state.track_downstream_session();
    let (reader, writer) = tokio::io::split(stream);
    let server = HostMcpServer::new(runtime, state);
    match server
        .serve_with_ct((reader, writer), cancellation.child_token())
        .await
    {
        Ok(service) => {
            if service.waiting().await.is_err() {
                tracing::debug!(operation = "mcp_wait", code = "TRANSPORT_CLOSED");
            }
        }
        Err(_) => tracing::debug!(operation = "mcp_initialize", code = "PROTOCOL_ERROR"),
    }
}

async fn dispatch_request(
    runtime: &RuntimeManager,
    state: &HostRuntimeState,
    request: ControlRequest,
) -> Result<Value, RuntimeError> {
    match request {
        ControlRequest::Ping => Ok(json!({ "ok": true })),
        ControlRequest::Status => json_value("status", state.status(runtime).await),
        ControlRequest::ListServers => json_value("list_servers", runtime.list_servers().await),
        ControlRequest::InspectServer { server_id } => {
            json_value("inspect_server", runtime.inspect_server(&server_id).await?)
        }
        ControlRequest::ConnectServer { server_id } => {
            json_value("connect_server", runtime.connect_server(&server_id).await?)
        }
        ControlRequest::DisconnectServer { server_id } => json_value(
            "disconnect_server",
            runtime.disconnect_server(&server_id).await?,
        ),
        ControlRequest::ListTools { server_id, refresh } => {
            json_value("list_tools", runtime.list_tools(&server_id, refresh).await?)
        }
        ControlRequest::CallTool {
            server_id,
            tool_name,
            arguments,
            timeout_ms,
        } => Ok(runtime
            .call_tool(&server_id, &tool_name, arguments, timeout_ms)
            .await?
            .into_value()),
        ControlRequest::CallTools { calls } => {
            json_value("call_tools", runtime.call_tools(calls).await?)
        }
        ControlRequest::RefreshServer { server_id } => {
            json_value("refresh_server", runtime.refresh_server(&server_id).await?)
        }
        ControlRequest::PackageInstall { server_id } => json_value(
            "package_install",
            runtime.package_install(&server_id).await?,
        ),
        ControlRequest::AuthStart {
            server_id,
            redirect_uri,
        } => json_value(
            "auth_start",
            runtime.auth_start(&server_id, &redirect_uri).await?,
        ),
        ControlRequest::AuthComplete {
            server_id,
            callback_url,
        } => json_value(
            "auth_complete",
            runtime.auth_complete(&server_id, &callback_url).await?,
        ),
        ControlRequest::AuthStatus { server_id } => {
            json_value("auth_status", runtime.auth_status(&server_id).await?)
        }
        ControlRequest::AuthLogout { server_id } => {
            json_value("auth_logout", runtime.auth_logout(&server_id).await?)
        }
        ControlRequest::SkillList => json_value("skill_list", runtime.list_skills().await),
        ControlRequest::SkillRun { skill_id, inputs } => {
            json_value("skill_run", runtime.run_skill(&skill_id, inputs).await?)
        }
        ControlRequest::Shutdown => Ok(json!({ "accepted": true })),
    }
}

fn allowed_during_shutdown(request: &ControlRequest) -> bool {
    matches!(
        request,
        ControlRequest::Ping | ControlRequest::Status | ControlRequest::Shutdown
    )
}

fn control_operation(request: &ControlRequest) -> &'static str {
    match request {
        ControlRequest::Ping => "ping",
        ControlRequest::Status => "status",
        ControlRequest::ListServers => "list_servers",
        ControlRequest::InspectServer { .. } => "inspect_server",
        ControlRequest::ConnectServer { .. } => "connect_server",
        ControlRequest::DisconnectServer { .. } => "disconnect_server",
        ControlRequest::ListTools { .. } => "list_tools",
        ControlRequest::CallTool { .. } => "call_tool",
        ControlRequest::CallTools { .. } => "call_tools",
        ControlRequest::RefreshServer { .. } => "refresh_server",
        ControlRequest::PackageInstall { .. } => "package_install",
        ControlRequest::AuthStart { .. } => "auth_start",
        ControlRequest::AuthComplete { .. } => "auth_complete",
        ControlRequest::AuthStatus { .. } => "auth_status",
        ControlRequest::AuthLogout { .. } => "auth_logout",
        ControlRequest::SkillList => "skill_list",
        ControlRequest::SkillRun { .. } => "skill_run",
        ControlRequest::Shutdown => "shutdown",
    }
}

fn json_value(operation: &str, value: impl Serialize) -> Result<Value, RuntimeError> {
    serde_json::to_value(value).map_err(|_| {
        RuntimeError::new(
            RuntimeErrorCode::ProtocolError,
            operation,
            "failed to serialize a control response",
        )
    })
}

async fn wait_for_shutdown_event(cancellation: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = cancellation.cancelled() => {}
                    result = tokio::signal::ctrl_c() => {
                        if result.is_err() {
                            tracing::warn!(operation = "ctrl_c", code = "IPC_UNAVAILABLE");
                        }
                    }
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => wait_for_ctrl_c_or_cancellation(cancellation).await,
        }
    }
    #[cfg(not(unix))]
    wait_for_ctrl_c_or_cancellation(cancellation).await;
}

async fn wait_for_ctrl_c_or_cancellation(cancellation: CancellationToken) {
    tokio::select! {
        _ = cancellation.cancelled() => {}
        result = tokio::signal::ctrl_c() => {
            if result.is_err() {
                tracing::warn!(operation = "ctrl_c", code = "IPC_UNAVAILABLE");
            }
        }
    }
}

async fn wait_for_accept_task(mut task: JoinHandle<()>, operation: &'static str) {
    if timeout(SHUTDOWN_TIMEOUT, &mut task).await.is_err() {
        tracing::warn!(operation, code = "SHUTDOWN_FAILED");
        task.abort();
        let _ = task.await;
    }
}

async fn drain_tasks(tasks: &mut JoinSet<()>, operation: &'static str) {
    if timeout(SHUTDOWN_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tracing::warn!(operation, code = "SHUTDOWN_FAILED");
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

fn prepare_runtime_dir(runtime_dir: &Path) -> Result<(), RuntimeError> {
    fs::create_dir_all(runtime_dir).map_err(|_| {
        ipc_error(
            "create_runtime_dir",
            "failed to create the runtime directory",
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700)).map_err(|_| {
            ipc_error(
                "secure_runtime_dir",
                "failed to secure the runtime directory",
            )
        })?;
    }
    Ok(())
}

fn acquire_lock(lock_path: &Path) -> Result<File, RuntimeError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let lock = options.open(lock_path).map_err(|_| {
        RuntimeError::new(
            RuntimeErrorCode::DaemonAlreadyRunning,
            "acquire_lock",
            "another daemon instance is already running",
        )
    })?;
    lock.try_lock_exclusive().map_err(|_| {
        RuntimeError::new(
            RuntimeErrorCode::DaemonAlreadyRunning,
            "acquire_lock",
            "another daemon instance is already running",
        )
    })?;
    Ok(lock)
}

fn release_lock(lock: File, lock_path: &Path) {
    let _ = lock.unlock();
    drop(lock);
    let _ = fs::remove_file(lock_path);
}

fn write_metadata(path: &Path, metadata: &DaemonMetadata) -> Result<(), RuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(existing) if !existing.file_type().is_file() => {
            return Err(ipc_error(
                "write_metadata",
                "the daemon metadata path is not a regular file",
            ));
        }
        Ok(_) => fs::remove_file(path)
            .map_err(|_| ipc_error("write_metadata", "failed to replace stale daemon metadata"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(ipc_error(
                "write_metadata",
                "failed to inspect daemon metadata",
            ));
        }
    }
    let encoded = serde_json::to_vec(metadata)
        .map_err(|_| ipc_error("write_metadata", "failed to serialize daemon metadata"))?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| ipc_error("write_metadata", "failed to create daemon metadata"))?;
    file.write_all(&encoded)
        .and_then(|()| file.sync_all())
        .map_err(|_| ipc_error("write_metadata", "failed to write daemon metadata"))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn ipc_error(operation: &'static str, message: &'static str) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::IpcUnavailable, operation, message)
}

fn config_error(operation: &'static str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ProtocolError,
        operation,
        "failed to load the daemon configuration",
    )
}

fn protocol_mismatch_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::IpcProtocolMismatch,
        "control_protocol",
        "the control request uses an unsupported protocol version",
    )
}

fn daemon_shutting_down_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::DaemonShuttingDown,
        "control_request",
        "the daemon is shutting down",
    )
}

#[cfg(test)]
mod tests {
    use std::{error::Error, sync::Arc};

    use mcp_host_core::{
        BatchToolCall, HostStatus, ManifestLoader, ProcessEnvironment, RegistryBuilder,
    };
    use mcp_host_mcp::{HostRuntimeState, RuntimeManager, RuntimeSettings};
    use serde_json::json;
    use tokio::time::{Duration, timeout};
    use tokio_util::sync::CancellationToken;

    use super::{
        CONTROL_PROTOCOL_VERSION, ControlRequest, DaemonMetadata, DaemonOptions, RuntimeErrorCode,
        allowed_during_shutdown, dispatch_request, protocol_mismatch_error, run_daemon_with_ready,
    };

    #[test]
    fn metadata_is_stable_and_contains_no_configuration_secrets() -> Result<(), Box<dyn Error>> {
        let metadata = DaemonMetadata {
            pid: 42,
            started_at_unix_ms: 1_234,
            control_protocol_version: CONTROL_PROTOCOL_VERSION,
            control_endpoint: "runtime/control.sock".to_owned(),
            mcp_endpoint: "runtime/mcp.sock".to_owned(),
            config_dir: "config".into(),
            binary_version: env!("CARGO_PKG_VERSION").to_owned(),
        };

        let value = serde_json::to_value(&metadata)?;
        assert_eq!(value["pid"], json!(42));
        assert_eq!(value["control_protocol_version"], json!(1));
        assert_eq!(serde_json::from_value::<DaemonMetadata>(value)?, metadata);
        assert!(!serde_json::to_string(&metadata)?.contains("sentinel-secret"));
        Ok(())
    }

    #[test]
    fn protocol_mismatch_response_has_a_stable_code() {
        let response =
            mcp_host_core::ControlResponseEnvelope::failure("request-7", protocol_mismatch_error());

        assert_eq!(response.request_id, "request-7");
        assert!(response.result.is_none());
        assert_eq!(
            response.error.map(|error| error.code),
            Some(RuntimeErrorCode::IpcProtocolMismatch)
        );
    }

    #[test]
    fn tool_calls_are_rejected_during_shutdown() {
        let call = ControlRequest::CallTool {
            server_id: "fixture".to_owned(),
            tool_name: "echo".to_owned(),
            arguments: json!({}),
            timeout_ms: None,
        };
        let batch = ControlRequest::CallTools {
            calls: vec![BatchToolCall {
                server_id: "fixture".to_owned(),
                tool_name: "echo".to_owned(),
                arguments: json!({}),
                timeout_ms: None,
            }],
        };

        assert!(!allowed_during_shutdown(&call));
        assert!(!allowed_during_shutdown(&batch));
        assert!(allowed_during_shutdown(&ControlRequest::Status));
    }

    #[tokio::test]
    async fn ping_and_status_work_with_an_empty_registry() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let loaded = ManifestLoader::new(ProcessEnvironment).load_directory(directory.path())?;
        let registry = RegistryBuilder::build(loaded)?;
        let runtime = RuntimeManager::new(Arc::new(registry), RuntimeSettings::default());
        let state = HostRuntimeState::new();

        assert_eq!(
            dispatch_request(&runtime, &state, ControlRequest::Ping).await?,
            json!({ "ok": true })
        );
        let status = dispatch_request(&runtime, &state, ControlRequest::Status).await?;
        let status: HostStatus = serde_json::from_value(status)?;
        assert_eq!(status.registry_server_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn external_service_cancellation_stops_a_ready_daemon() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let config_dir = root.path().join("config");
        let runtime_dir = root.path().join("runtime");
        std::fs::create_dir(&config_dir)?;
        let cancellation = CancellationToken::new();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let daemon = tokio::spawn(run_daemon_with_ready(
            DaemonOptions {
                config_dir,
                runtime_dir,
            },
            cancellation.clone(),
            ready_tx,
        ));

        timeout(Duration::from_secs(2), ready_rx).await??;
        cancellation.cancel();
        timeout(Duration::from_secs(2), daemon).await???;
        Ok(())
    }
}
