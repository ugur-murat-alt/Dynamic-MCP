//! Per-server MCP proxy endpoint.
//!
//! Presents one downstream server's tools, resources, and prompts natively to
//! an MCP client, so harnesses see `echo`, `add`, ... instead of a routing
//! layer. The daemon binds one proxy endpoint per connected server; a proxy
//! only exists while its server is connected.

use std::sync::Arc;

use mcp_host_core::{CallPolicy, ToolDefinition};
use rmcp::{
    ErrorData, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, Implementation, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::{RequestContext, RoleServer},
};
use serde_json::{Map, Value};

use crate::RuntimeManager;
use crate::host::runtime_error;

/// MCP server adapter that proxies exactly one connected downstream server.
#[derive(Clone)]
pub struct ProxyMcpServer {
    runtime: Arc<RuntimeManager>,
    server_id: String,
    resources: bool,
}

impl ProxyMcpServer {
    /// Creates a proxy over one connected downstream server.
    #[must_use]
    pub fn new(runtime: Arc<RuntimeManager>, server_id: impl Into<String>) -> Self {
        Self {
            runtime,
            server_id: server_id.into(),
            resources: false,
        }
    }

    /// Creates a proxy whose advertised capabilities mirror the downstream
    /// server's negotiated capabilities.
    pub async fn from_runtime(runtime: Arc<RuntimeManager>, server_id: impl Into<String>) -> Self {
        let server_id = server_id.into();
        let resources = runtime.supports_capability(&server_id, "resources").await;
        Self {
            runtime,
            server_id,
            resources,
        }
    }

    fn tool(definition: &ToolDefinition) -> Tool {
        let schema = match &definition.input_schema {
            Value::Object(schema) => Arc::new(schema.clone()),
            _ => Arc::new(Map::new()),
        };
        Tool::new(
            definition.name.clone(),
            definition.description.clone().unwrap_or_default(),
            schema,
        )
    }
}

impl ServerHandler for ProxyMcpServer {
    fn get_info(&self) -> ServerInfo {
        let capabilities = if self.resources {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build()
        } else {
            ServerCapabilities::builder().enable_tools().build()
        };
        ServerInfo::new(capabilities).with_server_info(
            Implementation::new("mcp-host-proxy", env!("CARGO_PKG_VERSION"))
                .with_title(format!("Dynamic MCP Host proxy: {}", self.server_id)),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let snapshot = self
            .runtime
            .list_tools(&self.server_id, false)
            .await
            .map_err(runtime_error)?;
        Ok(ListToolsResult {
            tools: snapshot.tools.iter().map(Self::tool).collect(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.unwrap_or_default();
        let result = self
            .runtime
            .call_tool(
                &self.server_id,
                &request.name,
                Value::Object(arguments),
                None,
                CallPolicy::default(),
            )
            .await
            .map_err(runtime_error)?;
        let result = serde_json::from_value(result.into_value())
            .map_err(|_| ErrorData::internal_error("proxy result decode failed", None))?;
        Ok(CallToolResponse::Complete(result))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let resources = self
            .runtime
            .list_resources(&self.server_id)
            .await
            .map_err(runtime_error)?;
        serde_json::from_value(Value::Object(Map::from_iter([(
            "resources".to_owned(),
            resources,
        )])))
        .map_err(|_| ErrorData::internal_error("proxy resource decode failed", None))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let resource = self
            .runtime
            .read_resource(&self.server_id, &request.uri)
            .await
            .map_err(runtime_error)?;
        let result = serde_json::from_value(resource)
            .map_err(|_| ErrorData::internal_error("proxy resource read decode failed", None))?;
        Ok(ReadResourceResponse::Complete(result))
    }
}
