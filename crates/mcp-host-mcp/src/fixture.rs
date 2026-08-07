//! MCP fixture server used by integration tests.

use std::{fs, io, path::PathBuf, process, time::Duration};

use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, Implementation, InitializeRequestParams, ListResourcesResult,
        PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

const MAX_SLEEP_MILLISECONDS: u64 = 5_000;

#[derive(Clone, Debug, Default)]
pub struct FixtureOptions {
    pub startup_counter_file: Option<PathBuf>,
    pub pid_file: Option<PathBuf>,
    pub initialize_delay_ms: u64,
}

#[derive(Clone)]
pub struct FixtureServer {
    tool_router: ToolRouter<Self>,
    options: FixtureOptions,
}

impl FixtureServer {
    pub fn new(options: FixtureOptions) -> Self {
        Self {
            tool_router: Self::tool_router(),
            options,
        }
    }

    pub fn record_startup(&self) -> io::Result<()> {
        if let Some(path) = &self.options.startup_counter_file {
            let current = match fs::read_to_string(path) {
                Ok(contents) => contents.trim().parse::<u64>().map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid startup counter in {}: {error}", path.display()),
                    )
                })?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
                Err(error) => return Err(error),
            };
            let next = current.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("startup counter overflow in {}", path.display()),
                )
            })?;
            fs::write(path, next.to_string())?;
        }

        if let Some(path) = &self.options.pid_file {
            fs::write(path, process::id().to_string())?;
        }

        Ok(())
    }
}

impl Default for FixtureServer {
    fn default() -> Self {
        Self::new(FixtureOptions::default())
    }
}

/// Runs the fixture over stdio without writing non-protocol data to stdout.
pub async fn run_stdio_fixture(options: FixtureOptions) -> Result<(), Box<dyn std::error::Error>> {
    let server = FixtureServer::new(options);
    server.record_startup()?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
struct EchoParams {
    message: String,
}

#[derive(Deserialize, JsonSchema)]
struct AddParams {
    a: i64,
    b: i64,
}

#[derive(Deserialize, JsonSchema)]
struct SleepParams {
    milliseconds: u64,
}

#[tool_router]
impl FixtureServer {
    #[tool(description = "Return the supplied message.")]
    async fn echo(
        &self,
        Parameters(EchoParams { message }): Parameters<EchoParams>,
    ) -> CallToolResult {
        let mut result = CallToolResult::structured(json!({ "message": message }));
        result.content = vec![ContentBlock::text(message)];
        result
    }

    #[tool(description = "Add two signed 64-bit integers.")]
    async fn add(&self, Parameters(AddParams { a, b }): Parameters<AddParams>) -> CallToolResult {
        match a.checked_add(b) {
            Some(sum) => {
                let mut result = CallToolResult::structured(json!({ "sum": sum }));
                result.content = vec![ContentBlock::text(sum.to_string())];
                result
            }
            None => CallToolResult::error(vec![ContentBlock::text("integer addition overflow")]),
        }
    }

    #[tool(description = "Sleep for up to five seconds.")]
    async fn sleep(
        &self,
        Parameters(SleepParams { milliseconds }): Parameters<SleepParams>,
    ) -> CallToolResult {
        let milliseconds = milliseconds.min(MAX_SLEEP_MILLISECONDS);
        tokio::time::sleep(Duration::from_millis(milliseconds)).await;
        CallToolResult::success(vec![ContentBlock::text(format!("slept {milliseconds}ms"))])
    }

    #[tool(description = "Return a caller-visible fixture error.")]
    async fn fail(&self) -> CallToolResult {
        CallToolResult::error(vec![ContentBlock::text("fixture failure")])
    }

    #[tool(description = "Exit the fixture process with status 86.")]
    async fn crash(&self) -> CallToolResult {
        process::exit(86)
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "mcp-host-fixture-server",
    instructions = "Fixture server for MCP host integration tests."
)]
impl ServerHandler for FixtureServer {
    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ServerInfo, ErrorData> {
        let delay = Duration::from_millis(self.options.initialize_delay_ms.min(5_000));
        tokio::select! {
            _ = context.ct.cancelled() => Err(ErrorData::internal_error("initialization cancelled", None)),
            _ = tokio::time::sleep(delay) => Ok(self.get_info()),
        }
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            "mcp-host-fixture-server",
            env!("CARGO_PKG_VERSION"),
        ))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult {
            resources: vec![
                Resource::new("fixture://info", "fixture info")
                    .with_description("Static fixture resource"),
            ],
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri != "fixture://info" {
            return Err(ErrorData::resource_not_found(
                "unknown fixture resource URI",
                None,
            ));
        }
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::TextResourceContents {
                uri: "fixture://info".to_owned(),
                mime_type: Some("text/plain".to_owned()),
                text: "fixture information resource".to_owned(),
                meta: None,
            }],
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::FixtureServer;

    #[test]
    fn fixture_tool_schema_has_exactly_five_tools() {
        let server = FixtureServer::default();
        assert!(server.record_startup().is_ok());
        let names: Vec<_> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();

        assert_eq!(names, ["add", "crash", "echo", "fail", "sleep"]);
    }

    #[tokio::test]
    async fn fail_tool_returns_a_tool_error_result() {
        let server = FixtureServer::default();
        let result = server.fail().await;

        assert_eq!(result.is_error, Some(true));
    }
}
