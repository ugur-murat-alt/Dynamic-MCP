use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DesiredConnection, LifecycleState};

pub const CONTROL_PROTOCOL_VERSION: u32 = 1;
pub const MAX_BATCH_CALLS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RuntimeErrorCode {
    ServerNotFound,
    ServerDisabled,
    ServerNotConnected,
    ServerAlreadyConnected,
    ServerStartFailed,
    ServerInitializationFailed,
    ServerDisconnectFailed,
    ServerUnavailable,
    TransportClosed,
    ProtocolError,
    ToolsNotDiscovered,
    ToolNotFound,
    InvalidArguments,
    ToolCallFailed,
    ToolCallTimeout,
    HttpConnectionFailed,
    IpcUnavailable,
    IpcProtocolMismatch,
    DaemonAlreadyRunning,
    DaemonNotRunning,
    DaemonShuttingDown,
    ShutdownFailed,
    PolicyDenied,
    PackageNotConfigured,
    PackageInstallFailed,
    AuthNotConfigured,
    AuthRequired,
    AuthInProgress,
    AuthFailed,
    SkillNotFound,
    SkillInvalid,
    SkillInputInvalid,
    SkillTemplateError,
    SkillUpstreamError,
    SkillOutputTooLarge,
    ServiceForeign,
    ServicePermissionDenied,
    ServiceManagerUnavailable,
    ServiceOperationFailed,
}

impl RuntimeErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServerNotFound => "SERVER_NOT_FOUND",
            Self::ServerDisabled => "SERVER_DISABLED",
            Self::ServerNotConnected => "SERVER_NOT_CONNECTED",
            Self::ServerAlreadyConnected => "SERVER_ALREADY_CONNECTED",
            Self::ServerStartFailed => "SERVER_START_FAILED",
            Self::ServerInitializationFailed => "SERVER_INITIALIZATION_FAILED",
            Self::ServerDisconnectFailed => "SERVER_DISCONNECT_FAILED",
            Self::ServerUnavailable => "SERVER_UNAVAILABLE",
            Self::TransportClosed => "TRANSPORT_CLOSED",
            Self::ProtocolError => "PROTOCOL_ERROR",
            Self::ToolsNotDiscovered => "TOOLS_NOT_DISCOVERED",
            Self::ToolNotFound => "TOOL_NOT_FOUND",
            Self::InvalidArguments => "INVALID_ARGUMENTS",
            Self::ToolCallFailed => "TOOL_CALL_FAILED",
            Self::ToolCallTimeout => "TOOL_CALL_TIMEOUT",
            Self::HttpConnectionFailed => "HTTP_CONNECTION_FAILED",
            Self::IpcUnavailable => "IPC_UNAVAILABLE",
            Self::IpcProtocolMismatch => "IPC_PROTOCOL_MISMATCH",
            Self::DaemonAlreadyRunning => "DAEMON_ALREADY_RUNNING",
            Self::DaemonNotRunning => "DAEMON_NOT_RUNNING",
            Self::DaemonShuttingDown => "DAEMON_SHUTTING_DOWN",
            Self::ShutdownFailed => "SHUTDOWN_FAILED",
            Self::PolicyDenied => "POLICY_DENIED",
            Self::PackageNotConfigured => "PACKAGE_NOT_CONFIGURED",
            Self::PackageInstallFailed => "PACKAGE_INSTALL_FAILED",
            Self::AuthNotConfigured => "AUTH_NOT_CONFIGURED",
            Self::AuthRequired => "AUTH_REQUIRED",
            Self::AuthInProgress => "AUTH_IN_PROGRESS",
            Self::AuthFailed => "AUTH_FAILED",
            Self::SkillNotFound => "SKILL_NOT_FOUND",
            Self::SkillInvalid => "SKILL_INVALID",
            Self::SkillInputInvalid => "SKILL_INPUT_INVALID",
            Self::SkillTemplateError => "SKILL_TEMPLATE_ERROR",
            Self::SkillUpstreamError => "SKILL_UPSTREAM_ERROR",
            Self::SkillOutputTooLarge => "SKILL_OUTPUT_TOO_LARGE",
            Self::ServiceForeign => "SERVICE_FOREIGN",
            Self::ServicePermissionDenied => "SERVICE_PERMISSION_DENIED",
            Self::ServiceManagerUnavailable => "SERVICE_MANAGER_UNAVAILABLE",
            Self::ServiceOperationFailed => "SERVICE_OPERATION_FAILED",
        }
    }
}

impl fmt::Display for RuntimeErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeError {
    pub code: RuntimeErrorCode,
    pub operation: String,
    pub server_id: Option<String>,
    pub message: String,
    pub retryable: bool,
    pub source_summary: Option<String>,
}

impl RuntimeError {
    #[must_use]
    pub fn new(
        code: RuntimeErrorCode,
        operation: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            operation: operation.into(),
            server_id: None,
            message: message.into(),
            retryable: false,
            source_summary: None,
        }
    }

    #[must_use]
    pub fn for_server(
        code: RuntimeErrorCode,
        operation: impl Into<String>,
        server_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            server_id: Some(server_id.into()),
            ..Self::new(code, operation, message)
        }
    }

    #[must_use]
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    #[must_use]
    pub fn with_source_summary(mut self, source_summary: impl Into<String>) -> Self {
        self.source_summary = Some(source_summary.into());
        self
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Stdio,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub annotations: Option<Value>,
    pub icons: Option<Value>,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSnapshot {
    pub server_id: String,
    pub fetched_at_unix_ms: u64,
    pub tool_count: u64,
    pub tools: Vec<ToolDefinition>,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub transport: TransportKind,
    pub desired_state: DesiredConnection,
    pub observed_state: LifecycleState,
    pub tool_count: u64,
    pub tools_stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInspection {
    pub server_id: String,
    pub public_manifest: Value,
    pub source: String,
    pub transport: TransportKind,
    pub enabled: bool,
    pub desired_state: DesiredConnection,
    pub observed_state: LifecycleState,
    pub protocol: Value,
    pub upstream: Value,
    pub tool_snapshot: Option<ToolSnapshot>,
    pub last_safe_error: Option<RuntimeError>,
    pub pid: Option<u32>,
    pub connected_at_unix_ms: Option<u64>,
    pub disconnected_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectResult {
    pub server_id: String,
    pub state: LifecycleState,
    pub tool_count: u64,
    pub protocol_version: String,
    pub connected_at_unix_ms: Option<u64>,
    pub tool_snapshot: Option<ToolSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisconnectResult {
    pub server_id: String,
    pub state: LifecycleState,
    pub disconnected_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageInstallResult {
    pub server_id: String,
    pub provider: String,
    pub package: String,
    pub version: String,
    pub binary_path: String,
    pub installed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthLoginStartResult {
    pub server_id: String,
    pub authorization_url: String,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatusResult {
    pub server_id: String,
    pub authenticated: bool,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    pub daemon_version: String,
    pub protocol_version: u32,
    pub started_at_unix_ms: u64,
    pub uptime_ms: u64,
    pub registry_server_count: u64,
    pub connected_count: u64,
    pub failed_count: u64,
    pub active_downstream_mcp_sessions: u64,
    pub control_endpoint_ready: bool,
    pub mcp_endpoint_ready: bool,
    pub shutting_down: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallResult(Value);

impl ToolCallResult {
    #[must_use]
    pub const fn new(value: Value) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.0
    }

    #[must_use]
    pub fn into_value(self) -> Value {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchToolCall {
    pub server_id: String,
    pub tool_name: String,
    #[serde(default = "default_tool_arguments")]
    pub arguments: Value,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchToolCallResult {
    pub server_id: String,
    pub tool_name: String,
    #[serde(flatten)]
    pub outcome: BatchToolCallOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BatchToolCallOutcome {
    Success { result: ToolCallResult },
    Error { error: RuntimeError },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchToolCallResponse {
    pub results: Vec<BatchToolCallResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillRunStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillStepResult {
    pub step_id: String,
    pub server_id: String,
    pub tool_name: String,
    pub result: ToolCallResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRunFailure {
    pub step_index: u64,
    pub step_id: String,
    pub server_id: String,
    pub tool_name: String,
    pub error: RuntimeError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRunResult {
    pub skill_id: String,
    pub status: SkillRunStatus,
    pub steps_completed: u64,
    pub steps_total: u64,
    pub results: Vec<SkillStepResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<SkillRunFailure>,
}

fn default_tool_arguments() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    Ping,
    Status,
    ListServers,
    InspectServer {
        server_id: String,
    },
    ConnectServer {
        server_id: String,
    },
    DisconnectServer {
        server_id: String,
    },
    ListTools {
        server_id: String,
        refresh: bool,
    },
    CallTool {
        server_id: String,
        tool_name: String,
        arguments: Value,
        timeout_ms: Option<u64>,
    },
    CallTools {
        calls: Vec<BatchToolCall>,
    },
    RefreshServer {
        server_id: String,
    },
    PackageInstall {
        server_id: String,
    },
    AuthStart {
        server_id: String,
        redirect_uri: String,
    },
    AuthComplete {
        server_id: String,
        callback_url: String,
    },
    AuthStatus {
        server_id: String,
    },
    AuthLogout {
        server_id: String,
    },
    SkillList,
    SkillRun {
        skill_id: String,
        #[serde(default = "default_tool_arguments")]
        inputs: Value,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequestEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    pub request: ControlRequest,
}

impl ControlRequestEnvelope {
    #[must_use]
    pub fn new(request_id: impl Into<String>, request: ControlRequest) -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            request_id: request_id.into(),
            request,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponseEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    pub result: Option<Value>,
    pub error: Option<RuntimeError>,
}

impl ControlResponseEnvelope {
    #[must_use]
    pub fn success(request_id: impl Into<String>, result: Value) -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            request_id: request_id.into(),
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub fn failure(request_id: impl Into<String>, error: RuntimeError) -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            request_id: request_id.into(),
            result: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        BatchToolCall, BatchToolCallOutcome, BatchToolCallResponse, BatchToolCallResult,
        CONTROL_PROTOCOL_VERSION, ControlRequest, ControlRequestEnvelope, ControlResponseEnvelope,
        RuntimeError, RuntimeErrorCode, ToolCallResult,
    };

    #[test]
    fn runtime_error_codes_use_the_specified_wire_values() {
        let cases = [
            (RuntimeErrorCode::ServerNotFound, "SERVER_NOT_FOUND"),
            (RuntimeErrorCode::ServerDisabled, "SERVER_DISABLED"),
            (RuntimeErrorCode::ServerNotConnected, "SERVER_NOT_CONNECTED"),
            (
                RuntimeErrorCode::ServerAlreadyConnected,
                "SERVER_ALREADY_CONNECTED",
            ),
            (RuntimeErrorCode::ServerStartFailed, "SERVER_START_FAILED"),
            (
                RuntimeErrorCode::ServerInitializationFailed,
                "SERVER_INITIALIZATION_FAILED",
            ),
            (
                RuntimeErrorCode::ServerDisconnectFailed,
                "SERVER_DISCONNECT_FAILED",
            ),
            (RuntimeErrorCode::ServerUnavailable, "SERVER_UNAVAILABLE"),
            (RuntimeErrorCode::TransportClosed, "TRANSPORT_CLOSED"),
            (RuntimeErrorCode::ProtocolError, "PROTOCOL_ERROR"),
            (RuntimeErrorCode::ToolsNotDiscovered, "TOOLS_NOT_DISCOVERED"),
            (RuntimeErrorCode::ToolNotFound, "TOOL_NOT_FOUND"),
            (RuntimeErrorCode::InvalidArguments, "INVALID_ARGUMENTS"),
            (RuntimeErrorCode::ToolCallFailed, "TOOL_CALL_FAILED"),
            (RuntimeErrorCode::ToolCallTimeout, "TOOL_CALL_TIMEOUT"),
            (
                RuntimeErrorCode::HttpConnectionFailed,
                "HTTP_CONNECTION_FAILED",
            ),
            (RuntimeErrorCode::IpcUnavailable, "IPC_UNAVAILABLE"),
            (
                RuntimeErrorCode::IpcProtocolMismatch,
                "IPC_PROTOCOL_MISMATCH",
            ),
            (
                RuntimeErrorCode::DaemonAlreadyRunning,
                "DAEMON_ALREADY_RUNNING",
            ),
            (RuntimeErrorCode::DaemonNotRunning, "DAEMON_NOT_RUNNING"),
            (RuntimeErrorCode::DaemonShuttingDown, "DAEMON_SHUTTING_DOWN"),
            (RuntimeErrorCode::ShutdownFailed, "SHUTDOWN_FAILED"),
            (RuntimeErrorCode::PolicyDenied, "POLICY_DENIED"),
            (
                RuntimeErrorCode::PackageNotConfigured,
                "PACKAGE_NOT_CONFIGURED",
            ),
            (
                RuntimeErrorCode::PackageInstallFailed,
                "PACKAGE_INSTALL_FAILED",
            ),
            (RuntimeErrorCode::AuthNotConfigured, "AUTH_NOT_CONFIGURED"),
            (RuntimeErrorCode::AuthRequired, "AUTH_REQUIRED"),
            (RuntimeErrorCode::AuthInProgress, "AUTH_IN_PROGRESS"),
            (RuntimeErrorCode::AuthFailed, "AUTH_FAILED"),
            (RuntimeErrorCode::SkillNotFound, "SKILL_NOT_FOUND"),
            (RuntimeErrorCode::SkillInvalid, "SKILL_INVALID"),
            (RuntimeErrorCode::SkillInputInvalid, "SKILL_INPUT_INVALID"),
            (RuntimeErrorCode::SkillTemplateError, "SKILL_TEMPLATE_ERROR"),
            (RuntimeErrorCode::SkillUpstreamError, "SKILL_UPSTREAM_ERROR"),
            (
                RuntimeErrorCode::SkillOutputTooLarge,
                "SKILL_OUTPUT_TOO_LARGE",
            ),
            (RuntimeErrorCode::ServiceForeign, "SERVICE_FOREIGN"),
            (
                RuntimeErrorCode::ServicePermissionDenied,
                "SERVICE_PERMISSION_DENIED",
            ),
            (
                RuntimeErrorCode::ServiceManagerUnavailable,
                "SERVICE_MANAGER_UNAVAILABLE",
            ),
            (
                RuntimeErrorCode::ServiceOperationFailed,
                "SERVICE_OPERATION_FAILED",
            ),
        ];

        for (code, wire_value) in cases {
            assert_eq!(code.as_str(), wire_value);
            assert_eq!(code.to_string(), wire_value);
            let serialized = serde_json::to_string(&code).expect("code should serialize");
            assert_eq!(serialized, format!("\"{wire_value}\""));
        }
    }

    #[test]
    fn request_and_response_envelopes_round_trip() {
        let request = ControlRequestEnvelope::new(
            "42",
            ControlRequest::CallTool {
                server_id: "filesystem".to_owned(),
                tool_name: "read_file".to_owned(),
                arguments: json!({"path": "notes/today.md"}),
                timeout_ms: Some(1_000),
            },
        );
        let response = ControlResponseEnvelope::success("42", json!({"content": "ok"}));
        let failure = ControlResponseEnvelope::failure(
            "42",
            RuntimeError::new(
                RuntimeErrorCode::ServerUnavailable,
                "connect_server",
                "the server is unavailable",
            ),
        );

        assert_eq!(round_trip(&request), request);
        assert_eq!(round_trip(&response), response);
        assert_eq!(round_trip(&failure), failure);
    }

    #[test]
    fn call_tool_arguments_preserve_newlines() {
        let request = ControlRequestEnvelope::new(
            "7",
            ControlRequest::CallTool {
                server_id: "writer".to_owned(),
                tool_name: "append".to_owned(),
                arguments: json!({"text": "first line\nsecond line"}),
                timeout_ms: None,
            },
        );

        let encoded = serde_json::to_string(&request).expect("request should serialize");
        assert!(encoded.contains("\\n"));
        assert_eq!(round_trip(&request), request);
    }

    #[test]
    fn batch_request_and_results_round_trip_in_input_order() {
        let request = ControlRequestEnvelope::new(
            "8",
            ControlRequest::CallTools {
                calls: vec![
                    BatchToolCall {
                        server_id: "one".to_owned(),
                        tool_name: "echo".to_owned(),
                        arguments: json!({"value": 1}),
                        timeout_ms: Some(1_000),
                    },
                    BatchToolCall {
                        server_id: "two".to_owned(),
                        tool_name: "missing".to_owned(),
                        arguments: json!({}),
                        timeout_ms: None,
                    },
                ],
            },
        );
        let response = BatchToolCallResponse {
            results: vec![
                BatchToolCallResult {
                    server_id: "one".to_owned(),
                    tool_name: "echo".to_owned(),
                    outcome: BatchToolCallOutcome::Success {
                        result: ToolCallResult::new(json!({"content": []})),
                    },
                },
                BatchToolCallResult {
                    server_id: "two".to_owned(),
                    tool_name: "missing".to_owned(),
                    outcome: BatchToolCallOutcome::Error {
                        error: RuntimeError::for_server(
                            RuntimeErrorCode::ToolNotFound,
                            "call_tool",
                            "two",
                            "the requested tool was not discovered",
                        ),
                    },
                },
            ],
        };

        assert_eq!(round_trip(&request), request);
        assert_eq!(round_trip(&response), response);
        assert!(matches!(
            response.results[0].outcome,
            BatchToolCallOutcome::Success { .. }
        ));
        assert!(matches!(
            response.results[1].outcome,
            BatchToolCallOutcome::Error { .. }
        ));
    }

    #[test]
    fn batch_call_defaults_missing_arguments_to_an_object() {
        let call: BatchToolCall = serde_json::from_value(json!({
            "server_id": "fixture",
            "tool_name": "echo"
        }))
        .expect("batch call should deserialize");

        assert_eq!(call.arguments, json!({}));
        assert_eq!(call.timeout_ms, None);
    }

    #[test]
    fn runtime_error_never_carries_tool_arguments() {
        let secret_argument = "token=never-display-this";
        let _request = ControlRequest::CallTool {
            server_id: "github".to_owned(),
            tool_name: "search".to_owned(),
            arguments: json!({"query": secret_argument}),
            timeout_ms: None,
        };
        let error = RuntimeError::for_server(
            RuntimeErrorCode::ToolCallFailed,
            "call_tool",
            "github",
            "the downstream tool failed",
        );

        assert!(!format!("{error:?}").contains(secret_argument));
        assert!(!error.to_string().contains(secret_argument));
    }

    #[test]
    fn oauth_errors_never_carry_callback_credentials() {
        let callback_secret = "code=never-display-this&state=also-secret";
        let _request = ControlRequest::AuthComplete {
            server_id: "remote".to_owned(),
            callback_url: format!("http://127.0.0.1:41000/callback?{callback_secret}"),
        };
        let error = RuntimeError::for_server(
            RuntimeErrorCode::AuthFailed,
            "auth_complete",
            "remote",
            "the OAuth operation failed",
        );

        assert!(!format!("{error:?}").contains(callback_secret));
        assert!(!error.to_string().contains(callback_secret));
    }

    #[test]
    fn control_protocol_version_is_v1() {
        assert_eq!(CONTROL_PROTOCOL_VERSION, 1);
        assert_eq!(
            ControlRequestEnvelope::new("1", ControlRequest::Ping).protocol_version,
            1
        );
    }

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let serialized = serde_json::to_string(value).expect("value should serialize");
        serde_json::from_str(&serialized).expect("value should deserialize")
    }
}
