use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use mcp_host_core::{
    AuthLoginStartResult, AuthStatusResult, OAuthConfig, RuntimeError, RuntimeErrorCode,
};
use reqwest::Url;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore, OAuthState,
    StoredCredentials,
};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;

const AUTH_FLOW_TTL: Duration = Duration::from_secs(300);
const STORE_VERSION: u32 = 1;

pub(crate) struct ServerAuth {
    server_id: String,
    resource_url: String,
    config: OAuthConfig,
    store: FileCredentialStore,
    pending: Mutex<Option<PendingAuthorization>>,
}

struct PendingAuthorization {
    state: OAuthState,
    redirect_uri: Url,
    expires_at_unix_ms: u64,
}

impl ServerAuth {
    pub(crate) fn new(
        server_id: String,
        resource_url: String,
        config: OAuthConfig,
        root: &Path,
    ) -> Self {
        let path = credential_path(root, &server_id, &resource_url, &config);
        Self {
            store: FileCredentialStore::new(path, resource_url.clone()),
            server_id,
            resource_url,
            config,
            pending: Mutex::new(None),
        }
    }

    pub(crate) async fn authenticated_manager(&self) -> Result<AuthorizationManager, RuntimeError> {
        let mut manager = self.authorization_manager().await?;
        let initialized = manager
            .initialize_from_store()
            .await
            .map_err(|error| self.runtime_error("connect_server", &error))?;
        if !initialized {
            return Err(self.error(
                RuntimeErrorCode::AuthRequired,
                "connect_server",
                "OAuth authorization is required",
            ));
        }
        manager
            .get_access_token()
            .await
            .map_err(|error| self.runtime_error("connect_server", &error))?;
        Ok(manager)
    }

    pub(crate) async fn start(
        &self,
        redirect_uri: &str,
    ) -> Result<AuthLoginStartResult, RuntimeError> {
        let redirect_uri = validate_redirect_uri(redirect_uri).map_err(|message| {
            self.error(RuntimeErrorCode::InvalidArguments, "auth_start", message)
        })?;
        let mut pending = self.pending.lock().await;
        if pending
            .as_ref()
            .is_some_and(|flow| flow.expires_at_unix_ms > unix_ms())
        {
            return Err(self.error(
                RuntimeErrorCode::AuthInProgress,
                "auth_start",
                "an OAuth authorization flow is already in progress",
            ));
        }
        *pending = None;

        let manager = self.authorization_manager().await?;
        let mut state = OAuthState::Unauthorized(manager);
        let mut request = AuthorizationRequest::new(redirect_uri.to_string())
            .with_client_name("Dynamic MCP Host");
        if !self.config.scopes.is_empty() {
            request = request.with_scopes(self.config.scopes.clone());
        }
        if let Some(client_id) = &self.config.client_id {
            request = request.with_preregistered_client(client_id.clone());
        }
        state
            .start_authorization(request)
            .await
            .map_err(|error| self.runtime_error("auth_start", &error))?;
        let authorization_url = state
            .get_authorization_url()
            .await
            .map_err(|error| self.runtime_error("auth_start", &error))?;
        let expires_at_unix_ms = unix_ms().saturating_add(duration_ms(AUTH_FLOW_TTL));
        *pending = Some(PendingAuthorization {
            state,
            redirect_uri,
            expires_at_unix_ms,
        });
        Ok(AuthLoginStartResult {
            server_id: self.server_id.clone(),
            authorization_url,
            expires_at_unix_ms,
        })
    }

    pub(crate) async fn complete(
        &self,
        callback_url: &str,
    ) -> Result<AuthStatusResult, RuntimeError> {
        let callback = Url::parse(callback_url).map_err(|_| {
            self.error(
                RuntimeErrorCode::InvalidArguments,
                "auth_complete",
                "the OAuth callback URL is invalid",
            )
        })?;
        let mut pending = self.pending.lock().await;
        let flow = pending.as_ref().ok_or_else(|| {
            self.error(
                RuntimeErrorCode::AuthRequired,
                "auth_complete",
                "no OAuth authorization flow is in progress",
            )
        })?;
        if flow.expires_at_unix_ms <= unix_ms() {
            *pending = None;
            return Err(self.error(
                RuntimeErrorCode::AuthRequired,
                "auth_complete",
                "the OAuth authorization flow expired",
            ));
        }
        if !callback_matches_redirect(&callback, &flow.redirect_uri) {
            return Err(self.error(
                RuntimeErrorCode::InvalidArguments,
                "auth_complete",
                "the OAuth callback does not match the loopback redirect",
            ));
        }
        let mut flow = pending.take().expect("pending authorization was checked");
        flow.state
            .handle_callback_url(callback_url)
            .await
            .map_err(|error| self.runtime_error("auth_complete", &error))?;
        self.status().await
    }

    pub(crate) async fn in_progress(&self) -> bool {
        self.pending
            .lock()
            .await
            .as_ref()
            .is_some_and(|flow| flow.expires_at_unix_ms > unix_ms())
    }

    pub(crate) async fn status(&self) -> Result<AuthStatusResult, RuntimeError> {
        let credentials = self
            .store
            .load()
            .await
            .map_err(|error| self.runtime_error("auth_status", &error))?;
        let authenticated = credentials
            .as_ref()
            .and_then(|credentials| credentials.token_response.as_ref())
            .is_some();
        let scopes = credentials.map_or_else(Vec::new, |credentials| credentials.granted_scopes);
        Ok(AuthStatusResult {
            server_id: self.server_id.clone(),
            authenticated,
            scopes,
        })
    }

    pub(crate) async fn logout(&self) -> Result<AuthStatusResult, RuntimeError> {
        *self.pending.lock().await = None;
        self.store
            .clear()
            .await
            .map_err(|error| self.runtime_error("auth_logout", &error))?;
        Ok(AuthStatusResult {
            server_id: self.server_id.clone(),
            authenticated: false,
            scopes: Vec::new(),
        })
    }

    async fn authorization_manager(&self) -> Result<AuthorizationManager, RuntimeError> {
        let mut manager = AuthorizationManager::new(&self.resource_url)
            .await
            .map_err(|error| self.runtime_error("oauth_client", &error))?;
        manager.set_credential_store(self.store.clone());
        Ok(manager)
    }

    fn runtime_error(&self, operation: &str, error: &AuthError) -> RuntimeError {
        let code = if matches!(error, AuthError::AuthorizationRequired) {
            RuntimeErrorCode::AuthRequired
        } else {
            RuntimeErrorCode::AuthFailed
        };
        self.error(code, operation, "the OAuth operation failed")
    }

    fn error(&self, code: RuntimeErrorCode, operation: &str, message: &str) -> RuntimeError {
        RuntimeError::for_server(code, operation, &self.server_id, message)
    }
}

#[derive(Clone)]
struct FileCredentialStore {
    path: PathBuf,
    resource_url: String,
}

#[derive(Serialize, Deserialize)]
struct CredentialFile {
    version: u32,
    resource_url: String,
    credentials: StoredCredentials,
}

impl FileCredentialStore {
    fn new(path: PathBuf, resource_url: String) -> Self {
        Self { path, resource_url }
    }

    fn load_sync(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(store_error(
                    "stored OAuth credentials could not be inspected",
                ));
            }
        };
        if !metadata.file_type().is_file() {
            return Err(store_error(
                "stored OAuth credentials are not a regular file",
            ));
        }
        let encoded = fs::read(&self.path)
            .map_err(|_| store_error("stored OAuth credentials could not be read"))?;
        let stored: CredentialFile = serde_json::from_slice(&encoded)
            .map_err(|_| store_error("stored OAuth credentials are invalid"))?;
        if stored.version != STORE_VERSION || stored.resource_url != self.resource_url {
            return Ok(None);
        }
        Ok(Some(stored.credentials))
    }

    fn save_sync(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| store_error("the OAuth credential path is invalid"))?;
        ensure_private_directory(parent)?;
        let encoded = serde_json::to_vec(&CredentialFile {
            version: STORE_VERSION,
            resource_url: self.resource_url.clone(),
            credentials,
        })
        .map_err(|_| store_error("OAuth credentials could not be serialized"))?;
        let mut temporary = NamedTempFile::new_in(parent)
            .map_err(|_| store_error("OAuth credentials could not be staged"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| store_error("OAuth credential permissions could not be set"))?;
        }
        temporary
            .as_file_mut()
            .write_all(&encoded)
            .and_then(|()| temporary.as_file_mut().sync_all())
            .map_err(|_| store_error("OAuth credentials could not be written"))?;
        temporary
            .persist(&self.path)
            .map_err(|_| store_error("OAuth credentials could not be published"))?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| store_error("the OAuth credential directory could not be synced"))?;
        Ok(())
    }

    fn clear_sync(&self) -> Result<(), AuthError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(store_error("stored OAuth credentials could not be removed")),
        }
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        self.load_sync()
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        self.save_sync(credentials)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.clear_sync()
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), AuthError> {
    fs::create_dir_all(path)
        .map_err(|_| store_error("the OAuth credential directory could not be created"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| store_error("the OAuth credential directory could not be inspected"))?;
    if !metadata.file_type().is_dir() {
        return Err(store_error(
            "the OAuth credential directory is not a regular directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| store_error("the OAuth credential directory could not be secured"))?;
    }
    Ok(())
}

fn validate_redirect_uri(value: &str) -> Result<Url, &'static str> {
    let url = Url::parse(value).map_err(|_| "the OAuth redirect URI is invalid")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|port| port == 0)
        || url.path() != "/callback"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("the OAuth redirect URI must be an ephemeral IPv4 loopback callback");
    }
    Ok(url)
}

fn callback_matches_redirect(callback: &Url, redirect: &Url) -> bool {
    callback.scheme() == redirect.scheme()
        && callback.host_str() == redirect.host_str()
        && callback.port() == redirect.port()
        && callback.path() == redirect.path()
        && callback.fragment().is_none()
        && callback.username().is_empty()
        && callback.password().is_none()
}

fn store_error(message: &str) -> AuthError {
    AuthError::InternalError(message.to_owned())
}

fn credential_path(
    root: &Path,
    server_id: &str,
    resource_url: &str,
    config: &OAuthConfig,
) -> PathBuf {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in server_id
        .bytes()
        .chain([0])
        .chain(resource_url.bytes())
        .chain([0])
        .chain(config.client_id.as_deref().unwrap_or_default().bytes())
        .chain([0])
        .chain(
            config
                .scopes
                .iter()
                .flat_map(|scope| scope.bytes().chain([0])),
        )
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    root.join(format!("{server_id}-{hash:016x}.json"))
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;

    use mcp_host_core::OAuthConfig;
    use rmcp::transport::auth::CredentialStore as _;
    use tempfile::tempdir;

    use super::{FileCredentialStore, credential_path, validate_redirect_uri};

    #[test]
    fn redirect_uri_requires_an_exact_ephemeral_loopback_callback() {
        assert!(validate_redirect_uri("http://127.0.0.1:54321/callback").is_ok());
        for invalid in [
            "https://127.0.0.1:54321/callback",
            "http://localhost:54321/callback",
            "http://127.0.0.1/callback",
            "http://127.0.0.1:0/callback",
            "http://127.0.0.1:54321/other",
            "http://127.0.0.1:54321/callback?state=value",
        ] {
            assert!(
                validate_redirect_uri(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn credential_path_separates_resource_and_client_configuration() {
        let root = std::path::Path::new("auth");
        let dynamic = OAuthConfig {
            client_id: None,
            scopes: vec!["read".to_owned()],
        };
        let registered = OAuthConfig {
            client_id: Some("client".to_owned()),
            scopes: vec!["read".to_owned()],
        };

        assert_ne!(
            credential_path(root, "server", "https://one.example/mcp", &dynamic),
            credential_path(root, "server", "https://two.example/mcp", &dynamic)
        );
        assert_ne!(
            credential_path(root, "server", "https://one.example/mcp", &dynamic),
            credential_path(root, "server", "https://one.example/mcp", &registered)
        );
    }

    #[tokio::test]
    async fn credential_store_ignores_a_different_resource_and_clears_idempotently() {
        let root = tempdir().expect("temporary directory");
        let path = root.path().join("auth/server.json");
        let first = FileCredentialStore::new(path.clone(), "https://one.example/mcp".to_owned());
        let second = FileCredentialStore::new(path.clone(), "https://two.example/mcp".to_owned());
        let credentials = rmcp::transport::auth::StoredCredentials::new(
            "client".to_owned(),
            None,
            vec!["read".to_owned()],
            None,
        );

        first.save(credentials).await.expect("credentials save");
        assert!(first.load().await.expect("credentials load").is_some());
        assert!(second.load().await.expect("other resource load").is_none());
        first.clear().await.expect("first clear");
        first.clear().await.expect("second clear");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn credential_store_uses_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().expect("temporary directory");
        let path = root.path().join("auth/server.json");
        let store = FileCredentialStore::new(path.clone(), "https://example.com/mcp".to_owned());
        store
            .save(rmcp::transport::auth::StoredCredentials::new(
                "client".to_owned(),
                None,
                Vec::new(),
                None,
            ))
            .await
            .expect("credentials save");

        assert_eq!(
            fs::metadata(&path)
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().expect("parent"))
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
