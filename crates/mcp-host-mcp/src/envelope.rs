use std::sync::Arc;

use mcp_host_core::{RuntimeError, ToolSuggestion};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

pub const HOST_TOOL_SCHEMA_VERSION: &str = "dynamic-mcp/v1";

#[derive(Debug, Serialize, JsonSchema)]
pub struct HostToolEnvelope<T> {
    pub schema_version: &'static str,
    pub operation: &'static str,
    pub ok: bool,
    pub data: T,
}

impl<T> HostToolEnvelope<T> {
    pub fn success(operation: &'static str, data: T) -> Self {
        Self {
            schema_version: HOST_TOOL_SCHEMA_VERSION,
            operation,
            ok: true,
            data,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RoutedToolResult {
    pub server_id: String,
    pub tool_name: String,
    pub result: Value,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HostToolErrorEnvelope {
    pub schema_version: &'static str,
    pub operation: String,
    pub ok: bool,
    pub error: HostToolError,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HostToolError {
    pub code: String,
    pub retryable: bool,
    pub server_id: Option<String>,
    /// Close-name candidates for misspelled tools, when the runtime computed them.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub suggestions: Option<Vec<ToolSuggestion>>,
}

impl From<&RuntimeError> for HostToolErrorEnvelope {
    fn from(error: &RuntimeError) -> Self {
        Self {
            schema_version: HOST_TOOL_SCHEMA_VERSION,
            operation: error.operation.clone(),
            ok: false,
            error: HostToolError {
                code: error.code.as_str().to_owned(),
                retryable: error.retryable,
                server_id: error.server_id.clone(),
                suggestions: error.suggestions.as_deref().cloned(),
            },
        }
    }
}

pub fn output_schema() -> Arc<Map<String, Value>> {
    let value = serde_json::to_value(schemars::schema_for!(HostToolEnvelope<Map<String, Value>>))
        .expect("host tool envelope schema should serialize");
    Arc::new(
        value
            .as_object()
            .expect("host tool envelope schema should be an object")
            .clone(),
    )
}

#[cfg(test)]
mod tests {
    use mcp_host_core::{RuntimeError, RuntimeErrorCode};
    use serde_json::{json, to_value};

    use super::{HOST_TOOL_SCHEMA_VERSION, HostToolEnvelope, HostToolErrorEnvelope, output_schema};
    use mcp_host_core::ToolSuggestion;

    #[test]
    fn success_envelope_has_a_stable_machine_shape() {
        let value = to_value(HostToolEnvelope::success(
            "list_servers",
            json!({"servers": []}),
        ))
        .expect("envelope should serialize");

        assert_eq!(value["schema_version"], HOST_TOOL_SCHEMA_VERSION);
        assert_eq!(value["operation"], "list_servers");
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"], json!({"servers": []}));
    }

    #[test]
    fn error_envelope_omits_messages_sources_and_arguments() {
        let mut error = RuntimeError::for_server(
            RuntimeErrorCode::ToolNotFound,
            "call_tool",
            "fixture",
            "secret argument must not be copied",
        );
        error.source_summary = Some("sensitive stderr".to_owned());
        let value =
            to_value(HostToolErrorEnvelope::from(&error)).expect("error envelope should serialize");

        assert_eq!(value["schema_version"], HOST_TOOL_SCHEMA_VERSION);
        assert_eq!(value["operation"], "call_tool");
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "TOOL_NOT_FOUND");
        assert_eq!(value["error"]["server_id"], "fixture");
        assert!(value.get("message").is_none());
        assert!(value.get("source_summary").is_none());
        assert!(value["error"].get("suggestions").is_none());
        assert!(!value.to_string().contains("secret argument"));
        assert!(!value.to_string().contains("sensitive stderr"));
    }

    #[test]
    fn error_envelope_carries_close_name_suggestions_when_present() {
        let error = RuntimeError::for_server(
            RuntimeErrorCode::ToolNotFound,
            "call_tool",
            "fixture",
            "the requested tool was not discovered",
        )
        .with_suggestions(vec![ToolSuggestion {
            server_id: "fixture".to_owned(),
            tool_name: "echo".to_owned(),
            description: Some("Return the supplied message.".to_owned()),
        }]);
        let value =
            to_value(HostToolErrorEnvelope::from(&error)).expect("error envelope should serialize");

        assert_eq!(value["error"]["suggestions"][0]["tool_name"], "echo");
        assert_eq!(value["error"]["suggestions"][0]["server_id"], "fixture");
        assert_eq!(
            value["error"]["suggestions"][0]["description"],
            "Return the supplied message."
        );
    }

    #[test]
    fn output_schema_requires_the_stable_outer_fields() {
        let schema = output_schema();
        let required = schema["required"]
            .as_array()
            .expect("required should be an array");

        assert!(required.contains(&json!("schema_version")));
        assert!(required.contains(&json!("operation")));
        assert!(required.contains(&json!("ok")));
        assert!(required.contains(&json!("data")));
        assert_eq!(schema["properties"]["data"]["type"], "object");
    }
}
