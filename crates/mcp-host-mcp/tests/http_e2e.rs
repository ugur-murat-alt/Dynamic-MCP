use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::{Request, State},
    middleware::{self, Next},
    response::Response,
};
use mcp_host_core::{EnvironmentAccessError, EnvironmentProvider, ManifestLoader, RegistryBuilder};
use mcp_host_mcp::{RuntimeManager, RuntimeSettings, fixture::FixtureServer};
use rmcp::{
    ErrorData, ServerHandler,
    model::{ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool},
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;
use tempfile::tempdir;
use tokio::{io::AsyncReadExt as _, sync::oneshot};
use tokio_util::sync::CancellationToken;

const SECRET: &str = "http-sentinel-secret";

#[derive(Clone)]
struct StaticEnvironment;

impl EnvironmentProvider for StaticEnvironment {
    fn get(&self, name: &str) -> Result<Option<String>, EnvironmentAccessError> {
        Ok((name == "FIXTURE_HTTP_HEADER").then(|| SECRET.to_owned()))
    }
}

#[derive(Default)]
struct HeaderObservation {
    matching_requests: std::sync::atomic::AtomicUsize,
}

#[tokio::test]
async fn real_streamable_http_initialize_headers_tools_call_and_disconnect() {
    let observation = Arc::new(HeaderObservation::default());
    let cancellation = CancellationToken::new();
    let service: StreamableHttpService<FixtureServer, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(FixtureServer::default()),
            Default::default(),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(true)
                .with_sse_keep_alive(None)
                .with_cancellation_token(cancellation.child_token()),
        );
    let router = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&observation),
            observe_header,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral HTTP listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let (ready_tx, ready_rx) = oneshot::channel();
    let server_cancellation = cancellation.clone();
    let server = tokio::spawn(async move {
        let _ = ready_tx.send(());
        axum::serve(listener, router)
            .with_graceful_shutdown(server_cancellation.cancelled_owned())
            .await
    });
    ready_rx.await.expect("HTTP server should become ready");

    let directory = tempdir().expect("temporary directory should be created");
    fs::write(
        directory.path().join("http.toml"),
        format!(
            "id = \"http-fixture\"\nname = \"HTTP Fixture\"\ndescription = \"Local HTTP fixture\"\n[transport]\ntype = \"http\"\nurl = \"http://{address}/mcp\"\n[transport.headers]\nX-Fixture-Secret = \"${{FIXTURE_HTTP_HEADER}}\"\n"
        ),
    )
    .expect("HTTP manifest should be written");
    let loaded = ManifestLoader::new(StaticEnvironment)
        .load_directory(directory.path())
        .expect("HTTP manifest should load");
    let registry = RegistryBuilder::build(loaded).expect("registry should build");
    let manager = RuntimeManager::new(Arc::new(registry), RuntimeSettings::default());

    let connected = manager
        .connect_server("http-fixture")
        .await
        .expect("HTTP fixture should initialize");
    assert_eq!(connected.protocol_version, "2025-11-25");
    assert_eq!(connected.tool_count, 5);
    let result = manager
        .call_tool(
            "http-fixture",
            "echo",
            json!({"message": "over-http"}),
            None,
        )
        .await
        .expect("HTTP tool call should succeed");
    assert_eq!(result.value()["structuredContent"]["message"], "over-http");
    manager
        .disconnect_server("http-fixture")
        .await
        .expect("HTTP fixture should disconnect");

    assert!(
        observation
            .matching_requests
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 3
    );
    let inspection = manager
        .inspect_server("http-fixture")
        .await
        .expect("HTTP fixture should be inspectable");
    assert!(!format!("{inspection:?}").contains(SECRET));

    cancellation.cancel();
    server
        .await
        .expect("HTTP task should join")
        .expect("HTTP server should stop cleanly");
}

#[tokio::test]
async fn paginated_discovery_and_failed_refresh_preserve_the_stale_snapshot() {
    let fail_listing = Arc::new(AtomicBool::new(false));
    let server_state = PaginatedServer {
        fail_listing: Arc::clone(&fail_listing),
    };
    let cancellation = CancellationToken::new();
    let service: StreamableHttpService<PaginatedServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server_state.clone()),
            Default::default(),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(true)
                .with_sse_keep_alive(None)
                .with_cancellation_token(cancellation.child_token()),
        );
    let router = Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server_cancellation = cancellation.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(server_cancellation.cancelled_owned())
            .await
    });

    let directory = tempdir().expect("temporary directory should be created");
    fs::write(
        directory.path().join("paginated.toml"),
        format!(
            "id = \"paginated\"\nname = \"Paginated\"\ndescription = \"Pagination test\"\n[transport]\ntype = \"http\"\nurl = \"http://{address}/mcp\"\n"
        ),
    )
    .expect("manifest should write");
    let registry = RegistryBuilder::build(
        ManifestLoader::new(StaticEnvironment)
            .load_directory(directory.path())
            .expect("manifest should load"),
    )
    .expect("registry should build");
    let manager = RuntimeManager::new(Arc::new(registry), RuntimeSettings::default());
    let connected = manager
        .connect_server("paginated")
        .await
        .expect("paginated server should connect");
    let names = connected
        .tool_snapshot
        .expect("connect should discover tools")
        .tools
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["first", "second", "third"]);

    fail_listing.store(true, Ordering::Release);
    manager
        .refresh_server("paginated")
        .await
        .expect_err("refresh should fail");
    let stale = manager
        .list_tools("paginated", false)
        .await
        .expect("previous snapshot should remain available");
    assert!(stale.stale);
    assert_eq!(stale.tool_count, 3);

    manager
        .disconnect_server("paginated")
        .await
        .expect("paginated server should disconnect");
    cancellation.cancel();
    server
        .await
        .expect("HTTP task should join")
        .expect("HTTP server should stop");
}

#[tokio::test]
async fn https_transport_opens_a_tls_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral TLS probe listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let directory = tempdir().expect("temporary directory should be created");
    fs::write(
        directory.path().join("https.toml"),
        format!(
            "id = \"https-fixture\"\nname = \"HTTPS Fixture\"\ndescription = \"TLS feature probe\"\n[transport]\ntype = \"http\"\nurl = \"https://{address}/mcp\"\n"
        ),
    )
    .expect("HTTPS manifest should be written");
    let registry = RegistryBuilder::build(
        ManifestLoader::new(StaticEnvironment)
            .load_directory(directory.path())
            .expect("HTTPS manifest should load"),
    )
    .expect("registry should build");
    let manager = RuntimeManager::new(
        Arc::new(registry),
        RuntimeSettings {
            connect_timeout: Duration::from_secs(2),
            ..RuntimeSettings::default()
        },
    );
    let connecting = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.connect_server("https-fixture").await }
    });

    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .expect("HTTPS transport should open a TCP connection")
        .expect("TLS probe should accept the connection");
    let mut content_type = [0_u8; 1];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut content_type))
        .await
        .expect("TLS ClientHello should arrive")
        .expect("TLS record byte should be readable");
    assert_eq!(content_type[0], 0x16, "expected a TLS handshake record");
    drop(stream);

    let error = connecting
        .await
        .expect("connect task should join")
        .expect_err("plain probe listener cannot finish a TLS handshake");
    assert_eq!(error.code.as_str(), "HTTP_CONNECTION_FAILED");
}

#[derive(Clone)]
struct PaginatedServer {
    fail_listing: Arc<AtomicBool>,
}

impl ServerHandler for PaginatedServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if self.fail_listing.load(Ordering::Acquire) {
            return Err(ErrorData::internal_error("listing disabled", None));
        }
        let schema = Arc::new(serde_json::Map::new());
        if request.and_then(|params| params.cursor).as_deref() == Some("second-page") {
            Ok(ListToolsResult::with_all_items(vec![Tool::new(
                "third", "Third", schema,
            )]))
        } else {
            let mut result = ListToolsResult::with_all_items(vec![
                Tool::new("first", "First", Arc::clone(&schema)),
                Tool::new("second", "Second", schema),
            ]);
            result.next_cursor = Some("second-page".to_owned());
            Ok(result)
        }
    }
}

async fn observe_header(
    State(observation): State<Arc<HeaderObservation>>,
    request: Request,
    next: Next,
) -> Response {
    if request
        .headers()
        .get("x-fixture-secret")
        .is_some_and(|value| value.as_bytes() == SECRET.as_bytes())
    {
        observation
            .matching_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    next.run(request).await
}
