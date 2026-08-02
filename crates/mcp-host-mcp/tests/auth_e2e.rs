use std::{
    collections::HashMap,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    extract::{Form, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use mcp_host_core::{EnvironmentAccessError, EnvironmentProvider, ManifestLoader, RegistryBuilder};
use mcp_host_mcp::{RuntimeManager, RuntimeSettings, fixture::FixtureServer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const SERVER_ID: &str = "oauth-fixture";
const AUTHORIZATION_CODE: &str = "fixture-authorization-code";
const EXPIRED_ACCESS_TOKEN: &str = "expired-access-token";
const REFRESH_TOKEN: &str = "fixture-refresh-token";
const FRESH_ACCESS_TOKEN: &str = "refreshed-access-token";

#[derive(Clone)]
struct EmptyEnvironment;

impl EnvironmentProvider for EmptyEnvironment {
    fn get(&self, _name: &str) -> Result<Option<String>, EnvironmentAccessError> {
        Ok(None)
    }
}

struct OAuthFixture {
    base_url: String,
    protected_resource_metadata_requests: AtomicUsize,
    authorization_server_metadata_requests: AtomicUsize,
    registrations: AtomicUsize,
    authorization_code_exchanges: AtomicUsize,
    refreshes: AtomicUsize,
    code_verifier_seen: AtomicBool,
    accepted_bearer_requests: AtomicUsize,
    registration_request: Mutex<Option<Value>>,
}

impl OAuthFixture {
    fn new(base_url: String) -> Self {
        Self {
            base_url,
            protected_resource_metadata_requests: AtomicUsize::new(0),
            authorization_server_metadata_requests: AtomicUsize::new(0),
            registrations: AtomicUsize::new(0),
            authorization_code_exchanges: AtomicUsize::new(0),
            refreshes: AtomicUsize::new(0),
            code_verifier_seen: AtomicBool::new(false),
            accepted_bearer_requests: AtomicUsize::new(0),
            registration_request: Mutex::new(None),
        }
    }
}

#[tokio::test]
async fn authorization_code_pkce_dynamic_registration_refresh_and_logout() {
    let cancellation = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("OAuth fixture listener should bind");
    let address = listener
        .local_addr()
        .expect("OAuth fixture listener should have an address");
    let fixture = Arc::new(OAuthFixture::new(format!("http://{address}")));
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
    let protected_resource =
        Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn_with_state(
                Arc::clone(&fixture),
                require_fresh_bearer_token,
            ));
    let app = Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route("/register", post(register_client))
        .route("/token", post(exchange_token))
        .merge(protected_resource)
        .with_state(Arc::clone(&fixture));
    let (ready_tx, ready_rx) = oneshot::channel();
    let server_cancellation = cancellation.clone();
    let server = tokio::spawn(async move {
        let _ = ready_tx.send(());
        axum::serve(listener, app)
            .with_graceful_shutdown(server_cancellation.cancelled_owned())
            .await
    });
    ready_rx.await.expect("OAuth fixture should become ready");

    let directory = tempdir().expect("temporary directory should be created");
    fs::write(
        directory.path().join("oauth.toml"),
        format!(
            "id = \"{SERVER_ID}\"\nname = \"OAuth Fixture\"\ndescription = \"Local authorization-code fixture\"\n\n[auth]\nscopes = [\"mcp.read\"]\n\n[transport]\ntype = \"http\"\nurl = \"{}/mcp\"\n",
            fixture.base_url
        ),
    )
    .expect("OAuth manifest should be written");
    let registry = RegistryBuilder::build(
        ManifestLoader::new(EmptyEnvironment)
            .load_directory(directory.path())
            .expect("OAuth manifest should load"),
    )
    .expect("OAuth registry should build");
    let auth_root = directory.path().join("credentials");
    let manager = RuntimeManager::new(
        Arc::new(registry),
        RuntimeSettings {
            auth_root: Some(auth_root.clone()),
            ..RuntimeSettings::default()
        },
    );
    let redirect_uri = "http://127.0.0.1:43127/callback";

    let started = manager
        .auth_start(SERVER_ID, redirect_uri)
        .await
        .expect("authorization should start");
    let authorization_url =
        reqwest::Url::parse(&started.authorization_url).expect("authorization URL should be valid");
    let authorization_parameters: HashMap<_, _> =
        authorization_url.query_pairs().into_owned().collect();
    let state = authorization_parameters
        .get("state")
        .expect("authorization URL should carry state");
    assert!(!state.is_empty());
    assert!(
        authorization_parameters
            .get("code_challenge")
            .is_some_and(|challenge| !challenge.is_empty())
    );
    assert_eq!(
        authorization_parameters.get("code_challenge_method"),
        Some(&"S256".to_owned())
    );
    assert_eq!(
        manager
            .auth_start(SERVER_ID, redirect_uri)
            .await
            .expect_err("a second authorization flow should be rejected")
            .code
            .as_str(),
        "AUTH_IN_PROGRESS"
    );

    let registration = fixture
        .registration_request
        .lock()
        .expect("registration observation should not be poisoned")
        .clone()
        .expect("dynamic registration should be observed");
    assert_eq!(fixture.registrations.load(Ordering::Acquire), 1);
    assert_eq!(registration["redirect_uris"], json!([redirect_uri]));
    assert_eq!(registration["token_endpoint_auth_method"], "none");

    let mut callback = reqwest::Url::parse(redirect_uri).expect("loopback URI should be valid");
    callback
        .query_pairs_mut()
        .append_pair("code", AUTHORIZATION_CODE)
        .append_pair("state", state);
    let mut wrong_callback = callback.clone();
    wrong_callback
        .set_port(Some(43128))
        .expect("loopback callback port should update");
    assert_eq!(
        manager
            .auth_complete(SERVER_ID, wrong_callback.as_str())
            .await
            .expect_err("a callback on another port should be rejected")
            .code
            .as_str(),
        "INVALID_ARGUMENTS"
    );
    let authenticated = manager
        .auth_complete(SERVER_ID, callback.as_str())
        .await
        .expect("callback should exchange the authorization code");
    assert!(authenticated.authenticated);
    assert_eq!(authenticated.scopes, ["mcp.read"]);
    assert_eq!(
        fixture.authorization_code_exchanges.load(Ordering::Acquire),
        1
    );
    assert!(fixture.code_verifier_seen.load(Ordering::Acquire));

    let status = manager
        .auth_status(SERVER_ID)
        .await
        .expect("OAuth status should be available");
    assert!(status.authenticated);
    assert_eq!(status.scopes, ["mcp.read"]);
    let status_debug = format!("{status:?}");
    for secret in [EXPIRED_ACCESS_TOKEN, REFRESH_TOKEN, FRESH_ACCESS_TOKEN] {
        assert!(!status_debug.contains(secret), "status leaked a credential");
    }
    assert_eq!(credential_file_count(&auth_root), 1);

    let connected = manager
        .connect_server(SERVER_ID)
        .await
        .expect("expired access token should refresh and connect");
    assert_eq!(connected.tool_count, 5);
    let result = manager
        .call_tool(SERVER_ID, "echo", json!({"message": "authorized"}), None)
        .await
        .expect("Bearer-protected tool call should succeed");
    assert_eq!(result.value()["structuredContent"]["message"], "authorized");
    assert_eq!(fixture.refreshes.load(Ordering::Acquire), 1);
    assert!(
        fixture.accepted_bearer_requests.load(Ordering::Acquire) >= 1,
        "the RMCP service should receive the refreshed Bearer token"
    );
    assert!(
        fixture
            .protected_resource_metadata_requests
            .load(Ordering::Acquire)
            >= 1
    );
    assert!(
        fixture
            .authorization_server_metadata_requests
            .load(Ordering::Acquire)
            >= 1
    );

    let logged_out = manager
        .auth_logout(SERVER_ID)
        .await
        .expect("logout should disconnect and clear credentials");
    assert!(!logged_out.authenticated);
    assert_eq!(credential_file_count(&auth_root), 0);

    let preregistered_directory = tempdir().expect("pre-registered directory should be created");
    fs::write(
        preregistered_directory.path().join("oauth.toml"),
        format!(
            "id = \"{SERVER_ID}\"\nname = \"OAuth Fixture\"\ndescription = \"Pre-registered fixture\"\n\n[auth]\nclient_id = \"pre-registered-client\"\nscopes = [\"mcp.read\"]\n\n[transport]\ntype = \"http\"\nurl = \"{}/mcp\"\n",
            fixture.base_url
        ),
    )
    .expect("pre-registered manifest should be written");
    let preregistered_registry = RegistryBuilder::build(
        ManifestLoader::new(EmptyEnvironment)
            .load_directory(preregistered_directory.path())
            .expect("pre-registered manifest should load"),
    )
    .expect("pre-registered registry should build");
    let preregistered = RuntimeManager::new(
        Arc::new(preregistered_registry),
        RuntimeSettings {
            auth_root: Some(auth_root.clone()),
            ..RuntimeSettings::default()
        },
    );
    let started = preregistered
        .auth_start(SERVER_ID, redirect_uri)
        .await
        .expect("pre-registered authorization should start");
    let authorization_url =
        reqwest::Url::parse(&started.authorization_url).expect("authorization URL should parse");
    assert_eq!(
        authorization_url
            .query_pairs()
            .find(|(name, _)| name == "client_id")
            .map(|(_, value)| value.into_owned()),
        Some("pre-registered-client".to_owned())
    );
    assert_eq!(fixture.registrations.load(Ordering::Acquire), 1);

    cancellation.cancel();
    server
        .await
        .expect("OAuth fixture task should join")
        .expect("OAuth fixture should stop cleanly");
}

fn credential_file_count(auth_root: &std::path::Path) -> usize {
    fs::read_dir(auth_root)
        .expect("credential directory should be readable")
        .count()
}

async fn protected_resource_metadata(State(fixture): State<Arc<OAuthFixture>>) -> Json<Value> {
    fixture
        .protected_resource_metadata_requests
        .fetch_add(1, Ordering::AcqRel);
    Json(json!({
        "resource": format!("{}/mcp", fixture.base_url),
        "authorization_servers": [fixture.base_url],
        "scopes_supported": ["mcp.read"]
    }))
}

async fn authorization_server_metadata(State(fixture): State<Arc<OAuthFixture>>) -> Json<Value> {
    fixture
        .authorization_server_metadata_requests
        .fetch_add(1, Ordering::AcqRel);
    Json(json!({
        "issuer": fixture.base_url,
        "authorization_endpoint": format!("{}/authorize", fixture.base_url),
        "token_endpoint": format!("{}/token", fixture.base_url),
        "registration_endpoint": format!("{}/register", fixture.base_url),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": ["mcp.read"]
    }))
}

async fn register_client(
    State(fixture): State<Arc<OAuthFixture>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    fixture.registrations.fetch_add(1, Ordering::AcqRel);
    *fixture
        .registration_request
        .lock()
        .expect("registration observation should not be poisoned") = Some(request);
    Json(json!({
        "client_id": "fixture-public-client",
        "redirect_uris": ["http://127.0.0.1:43127/callback"]
    }))
}

async fn exchange_token(
    State(fixture): State<Arc<OAuthFixture>>,
    Form(parameters): Form<HashMap<String, String>>,
) -> Response {
    match parameters.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            fixture
                .authorization_code_exchanges
                .fetch_add(1, Ordering::AcqRel);
            let valid_code = parameters
                .get("code")
                .is_some_and(|code| code == AUTHORIZATION_CODE);
            let verifier_seen = parameters
                .get("code_verifier")
                .is_some_and(|verifier| verifier.len() >= 43);
            fixture
                .code_verifier_seen
                .store(verifier_seen, Ordering::Release);
            if valid_code && verifier_seen {
                Json(json!({
                    "access_token": EXPIRED_ACCESS_TOKEN,
                    "token_type": "Bearer",
                    "expires_in": 0,
                    "refresh_token": REFRESH_TOKEN,
                    "scope": "mcp.read"
                }))
                .into_response()
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid_grant"})),
                )
                    .into_response()
            }
        }
        Some("refresh_token") => {
            fixture.refreshes.fetch_add(1, Ordering::AcqRel);
            if parameters
                .get("refresh_token")
                .is_some_and(|token| token == REFRESH_TOKEN)
            {
                Json(json!({
                    "access_token": FRESH_ACCESS_TOKEN,
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "mcp.read"
                }))
                .into_response()
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid_grant"})),
                )
                    .into_response()
            }
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unsupported_grant_type"})),
        )
            .into_response(),
    }
}

async fn require_fresh_bearer_token(
    State(fixture): State<Arc<OAuthFixture>>,
    request: Request,
    next: Next,
) -> Response {
    if request
        .headers()
        .get(header::AUTHORIZATION)
        .is_some_and(|value| value.as_bytes() == format!("Bearer {FRESH_ACCESS_TOKEN}").as_bytes())
    {
        fixture
            .accepted_bearer_requests
            .fetch_add(1, Ordering::AcqRel);
        return next.run(request).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            format!(
                "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
                fixture.base_url
            ),
        )],
    )
        .into_response()
}
