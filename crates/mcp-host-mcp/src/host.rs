//! Fixed inbound MCP server for managing downstream MCP runtimes.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use mcp_host_core::{BatchToolCall, HostStatus, RuntimeError, RuntimeErrorCode};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::RuntimeManager;

const SERVER_DESCRIPTION: &str = "A long-running MCP runtime and process manager that presents one stable MCP server to AI clients while managing manifest-defined MCP servers as downstream clients.";
const INSTRUCTIONS: &str = "1. Call list_servers to discover available downstream MCP servers.\n2. Call inspect_server to review a server's public configuration and current state.\n3. Call connect_server before using a disconnected server.\n4. Call list_tools after connecting to discover that server's available tools.\n5. Call call_tool for one invocation or call_tools for up to 32 parallel invocations.\n6. Use refresh_server when tools may have changed, then disconnect_server when the server is no longer needed.";

/// Shared process state owned by the daemon hosting inbound MCP sessions.
pub struct HostRuntimeState {
    started_at_unix_ms: u64,
    shutting_down: AtomicBool,
    control_ready: AtomicBool,
    mcp_ready: AtomicBool,
    active_downstream_mcp_sessions: AtomicU64,
}

impl HostRuntimeState {
    /// Creates an initially unavailable, non-shutting-down runtime state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started_at_unix_ms: unix_ms(),
            shutting_down: AtomicBool::new(false),
            control_ready: AtomicBool::new(false),
            mcp_ready: AtomicBool::new(false),
            active_downstream_mcp_sessions: AtomicU64::new(0),
        }
    }

    /// Marks whether daemon shutdown is in progress.
    pub fn set_shutting_down(&self, value: bool) {
        self.shutting_down.store(value, Ordering::Release);
    }

    /// Marks whether the control endpoint is ready to accept requests.
    pub fn set_control_ready(&self, value: bool) {
        self.control_ready.store(value, Ordering::Release);
    }

    /// Marks whether the inbound MCP endpoint is ready to accept requests.
    pub fn set_mcp_ready(&self, value: bool) {
        self.mcp_ready.store(value, Ordering::Release);
    }

    /// Counts one live downstream MCP session until the returned guard is dropped.
    #[must_use]
    pub fn track_downstream_session(self: &Arc<Self>) -> DownstreamSessionGuard {
        self.active_downstream_mcp_sessions
            .fetch_add(1, Ordering::AcqRel);
        DownstreamSessionGuard {
            state: Arc::clone(self),
        }
    }

    /// Returns a snapshot that combines host lifecycle state and managed runtimes.
    pub async fn status(&self, runtime: &RuntimeManager) -> HostStatus {
        let now = unix_ms();
        HostStatus {
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: mcp_host_core::CONTROL_PROTOCOL_VERSION,
            started_at_unix_ms: self.started_at_unix_ms,
            uptime_ms: now.saturating_sub(self.started_at_unix_ms),
            registry_server_count: runtime.server_count(),
            connected_count: runtime.connected_count().await,
            failed_count: runtime.failed_count().await,
            active_downstream_mcp_sessions: self
                .active_downstream_mcp_sessions
                .load(Ordering::Acquire),
            control_endpoint_ready: self.control_ready.load(Ordering::Acquire),
            mcp_endpoint_ready: self.mcp_ready.load(Ordering::Acquire),
            shutting_down: self.shutting_down.load(Ordering::Acquire),
        }
    }
}

impl Default for HostRuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

/// Decrements the active downstream session count when its session ends.
pub struct DownstreamSessionGuard {
    state: Arc<HostRuntimeState>,
}

impl Drop for DownstreamSessionGuard {
    fn drop(&mut self) {
        self.state
            .active_downstream_mcp_sessions
            .fetch_sub(1, Ordering::AcqRel);
    }
}

/// Inbound MCP handler exposing the fixed Dynamic MCP Host control surface.
#[derive(Clone)]
pub struct HostMcpServer {
    runtime: Arc<RuntimeManager>,
    state: Arc<HostRuntimeState>,
    tool_router: ToolRouter<Self>,
}

impl HostMcpServer {
    /// Creates the fixed MCP server adapter over a managed downstream runtime.
    #[must_use]
    pub fn new(runtime: Arc<RuntimeManager>, state: Arc<HostRuntimeState>) -> Self {
        Self {
            runtime,
            state,
            tool_router: Self::tool_router(),
        }
    }

    /// Returns the fixed tool names in deterministic order.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        let mut names = self
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    }
}

#[derive(Deserialize, JsonSchema)]
struct ServerIdParams {
    server_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct ListToolsParams {
    server_id: String,
    #[serde(default)]
    refresh: bool,
}

#[derive(JsonSchema)]
struct CallToolParams {
    server_id: String,
    tool_name: String,
    arguments: Map<String, Value>,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct CallToolParamsWire {
    server_id: String,
    tool_name: String,
    #[serde(default)]
    arguments: Map<String, Value>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for CallToolParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = CallToolParamsWire::deserialize(deserializer)?;
        Ok(Self {
            server_id: wire.server_id,
            tool_name: wire.tool_name,
            arguments: wire.arguments,
            timeout_ms: wire.timeout_ms,
        })
    }
}

#[derive(Deserialize, JsonSchema)]
struct CallToolsParams {
    calls: Vec<BatchToolCallParams>,
}

#[derive(Deserialize, JsonSchema)]
struct BatchToolCallParams {
    server_id: String,
    tool_name: String,
    #[serde(default)]
    arguments: Map<String, Value>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[tool_router]
impl HostMcpServer {
    #[tool(description = "List registered downstream MCP servers and their runtime state.")]
    async fn list_servers(&self) -> Result<CallToolResult, ErrorData> {
        let servers = self.runtime.list_servers().await;
        structured_result(
            json!({ "servers": servers }),
            "Listed downstream MCP servers.",
        )
    }

    #[tool(
        description = "Inspect one downstream MCP server's public configuration and runtime state."
    )]
    async fn inspect_server(
        &self,
        Parameters(ServerIdParams { server_id }): Parameters<ServerIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let inspection = self
            .runtime
            .inspect_server(&server_id)
            .await
            .map_err(runtime_error)?;
        structured_result(inspection, "Inspected downstream MCP server.")
    }

    #[tool(description = "Connect to a downstream MCP server and discover its tools.")]
    async fn connect_server(
        &self,
        Parameters(ServerIdParams { server_id }): Parameters<ServerIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let connection = self
            .runtime
            .connect_server(&server_id)
            .await
            .map_err(runtime_error)?;
        structured_result(connection, "Connected to downstream MCP server.")
    }

    #[tool(description = "Disconnect from a downstream MCP server.")]
    async fn disconnect_server(
        &self,
        Parameters(ServerIdParams { server_id }): Parameters<ServerIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let disconnection = self
            .runtime
            .disconnect_server(&server_id)
            .await
            .map_err(runtime_error)?;
        structured_result(disconnection, "Disconnected from downstream MCP server.")
    }

    #[tool(
        description = "List cached tools for a downstream MCP server, optionally refreshing first."
    )]
    async fn list_tools(
        &self,
        Parameters(ListToolsParams { server_id, refresh }): Parameters<ListToolsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let tools = self
            .runtime
            .list_tools(&server_id, refresh)
            .await
            .map_err(runtime_error)?;
        structured_result(tools, "Listed downstream MCP tools.")
    }

    #[tool(description = "Call a discovered tool on a connected downstream MCP server.")]
    async fn call_tool(
        &self,
        Parameters(CallToolParams {
            server_id,
            tool_name,
            arguments,
            timeout_ms,
        }): Parameters<CallToolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .runtime
            .call_tool(&server_id, &tool_name, Value::Object(arguments), timeout_ms)
            .await
            .map_err(runtime_error)?;
        decode_downstream_result(result.into_value())
    }

    #[tool(
        description = "Call between 1 and 32 discovered tools concurrently. Results preserve input order; item runtime errors are embedded without cancelling other calls."
    )]
    async fn call_tools(
        &self,
        Parameters(CallToolsParams { calls }): Parameters<CallToolsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let calls = calls
            .into_iter()
            .map(|call| BatchToolCall {
                server_id: call.server_id,
                tool_name: call.tool_name,
                arguments: Value::Object(call.arguments),
                timeout_ms: call.timeout_ms,
            })
            .collect();
        let response = self
            .runtime
            .call_tools(calls)
            .await
            .map_err(runtime_error)?;
        let text = format!(
            "Completed {} concurrent downstream tool calls.",
            response.results.len()
        );
        structured_result(response, &text)
    }

    #[tool(description = "Return Dynamic MCP Host process and downstream runtime status.")]
    async fn status(&self) -> Result<CallToolResult, ErrorData> {
        let status = self.state.status(&self.runtime).await;
        structured_result(status, "Retrieved Dynamic MCP Host status.")
    }

    #[tool(
        description = "Refresh the discovered tool cache for a connected downstream MCP server."
    )]
    async fn refresh_server(
        &self,
        Parameters(ServerIdParams { server_id }): Parameters<ServerIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let tools = self
            .runtime
            .refresh_server(&server_id)
            .await
            .map_err(runtime_error)?;
        structured_result(tools, "Refreshed downstream MCP tools.")
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HostMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("mcp-host", env!("CARGO_PKG_VERSION"))
                    .with_title("Dynamic MCP Host")
                    .with_description(SERVER_DESCRIPTION),
            )
            .with_instructions(INSTRUCTIONS)
    }
}

fn structured_result<T>(value: T, text: &str) -> Result<CallToolResult, ErrorData>
where
    T: Serialize,
{
    let value = serde_json::to_value(value).map_err(|_| serialization_error())?;
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text.to_owned())];
    Ok(result)
}

fn decode_downstream_result(value: Value) -> Result<CallToolResult, ErrorData> {
    serde_json::from_value(value).map_err(|_| serialization_error())
}

fn runtime_error(error: RuntimeError) -> ErrorData {
    let data = match serde_json::to_value(&error) {
        Ok(data) => data,
        Err(_) => return serialization_error(),
    };
    match error.code {
        RuntimeErrorCode::InvalidArguments
        | RuntimeErrorCode::ServerNotFound
        | RuntimeErrorCode::ServerDisabled
        | RuntimeErrorCode::ServerNotConnected
        | RuntimeErrorCode::ToolNotFound
        | RuntimeErrorCode::ToolsNotDiscovered => {
            ErrorData::invalid_params(error.message, Some(data))
        }
        _ => ErrorData::internal_error(error.message, Some(data)),
    }
}

fn serialization_error() -> ErrorData {
    ErrorData::internal_error("internal error", None)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use mcp_host_core::{
        EnvironmentAccessError, EnvironmentProvider, ManifestLoader, McpServerRegistry,
        RegistryBuilder,
    };
    use rmcp::{ServerHandler, model::ErrorCode};
    use serde_json::json;
    use tempfile::tempdir;

    use crate::RuntimeSettings;

    use super::{
        HostMcpServer, HostRuntimeState, RuntimeError, RuntimeErrorCode, RuntimeManager,
        decode_downstream_result, runtime_error,
    };

    #[test]
    fn tool_names_are_exactly_the_fixed_sorted_set() {
        let server = server();

        assert_eq!(
            server.tool_names(),
            [
                "call_tool",
                "call_tools",
                "connect_server",
                "disconnect_server",
                "inspect_server",
                "list_servers",
                "list_tools",
                "refresh_server",
                "status",
            ]
        );
    }

    #[test]
    fn metadata_advertises_only_tools_without_list_changed() {
        let info = server().get_info();
        let capabilities =
            serde_json::to_value(info.capabilities).expect("server capabilities should serialize");

        assert_eq!(capabilities, json!({ "tools": {} }));
    }

    #[test]
    fn call_tool_schema_requires_routing_fields_and_object_arguments() {
        let server = server();
        let tool = server
            .tool_router
            .get("call_tool")
            .cloned()
            .expect("call_tool should be registered");
        let schema = tool.schema_as_json_value();
        let required = schema["required"]
            .as_array()
            .expect("call_tool required fields should be an array");

        assert!(required.contains(&json!("server_id")));
        assert!(required.contains(&json!("tool_name")));
        assert!(required.contains(&json!("arguments")));
        assert_eq!(schema["properties"]["arguments"]["type"], json!("object"));
    }

    #[test]
    fn call_tools_schema_requires_a_calls_array() {
        let server = server();
        let tool = server
            .tool_router
            .get("call_tools")
            .cloned()
            .expect("call_tools should be registered");
        let schema = tool.schema_as_json_value();
        let required = schema["required"]
            .as_array()
            .expect("call_tools required fields should be an array");

        assert!(required.contains(&json!("calls")));
        assert_eq!(schema["properties"]["calls"]["type"], json!("array"));
    }

    #[test]
    fn runtime_error_mapping_includes_the_safe_runtime_code() {
        let error = runtime_error(RuntimeError::for_server(
            RuntimeErrorCode::ServerNotFound,
            "inspect_server",
            "missing",
            "the server is not registered",
        ));

        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(
            error.data.as_ref().and_then(|data| data["code"].as_str()),
            Some("SERVER_NOT_FOUND")
        );
    }

    #[test]
    fn downstream_tool_error_result_is_returned_without_conversion() {
        let value = json!({
            "content": [{ "type": "text", "text": "upstream error" }],
            "structuredContent": { "answer": false },
            "isError": true,
            "_meta": { "trace": "preserved" }
        });

        let result = decode_downstream_result(value).expect("downstream result should decode");
        let preserved = serde_json::to_value(result).expect("result should serialize");

        assert_eq!(preserved["isError"], json!(true));
        assert_eq!(preserved["structuredContent"], json!({ "answer": false }));
        assert_eq!(preserved["_meta"], json!({ "trace": "preserved" }));
    }

    #[tokio::test]
    async fn status_reads_atomics_and_session_guard_tracks_lifetime() {
        let state = Arc::new(HostRuntimeState::new());
        state.set_control_ready(true);
        state.set_mcp_ready(true);
        state.set_shutting_down(true);
        let runtime = runtime();

        let guard = state.track_downstream_session();
        let active = state.status(&runtime).await;
        assert_eq!(active.active_downstream_mcp_sessions, 1);
        assert!(active.control_endpoint_ready);
        assert!(active.mcp_endpoint_ready);
        assert!(active.shutting_down);

        drop(guard);
        assert_eq!(
            state.status(&runtime).await.active_downstream_mcp_sessions,
            0
        );
    }

    fn server() -> HostMcpServer {
        HostMcpServer::new(runtime(), Arc::new(HostRuntimeState::new()))
    }

    fn runtime() -> Arc<RuntimeManager> {
        RuntimeManager::new(Arc::new(registry()), RuntimeSettings::default())
    }

    fn registry() -> McpServerRegistry {
        let directory = tempdir().expect("temporary directory should be created");
        fs::write(
            directory.path().join("server.toml"),
            "id = \"test\"\nname = \"test\"\ndescription = \"test\"\n[transport]\ntype = \"stdio\"\ncommand = \"unused\"\n",
        )
        .expect("manifest should be written");
        let loaded = ManifestLoader::new(EmptyEnvironment)
            .load_directory(directory.path())
            .expect("manifest should load");
        RegistryBuilder::build(loaded).expect("registry should build")
    }

    struct EmptyEnvironment;

    impl EnvironmentProvider for EmptyEnvironment {
        fn get(&self, _name: &str) -> Result<Option<String>, EnvironmentAccessError> {
            Ok(None)
        }
    }
}
