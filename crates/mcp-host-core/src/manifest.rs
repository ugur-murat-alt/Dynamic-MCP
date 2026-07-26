use std::{borrow::Borrow, collections::BTreeMap, fmt, path::PathBuf, str::FromStr};

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

/// A parsed, unresolved registration for one downstream MCP server.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerManifest {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub transport: TransportConfig,
}

impl fmt::Debug for ServerManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerManifest")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("description", &self.description)
            .field("enabled", &self.enabled)
            .field("transport", &self.transport)
            .finish()
    }
}

/// Raw transport settings before semantic validation and secret resolution.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        working_directory: Option<String>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl fmt::Debug for TransportConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                command,
                args,
                working_directory,
                environment,
            } => formatter
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("working_directory", working_directory)
                .field("environment_keys", &environment.keys().collect::<Vec<_>>())
                .finish(),
            Self::Http { headers, .. } => formatter
                .debug_struct("Http")
                .field("url", &"<configured>")
                .field("header_names", &headers.keys().collect::<Vec<_>>())
                .finish(),
        }
    }
}

const fn enabled_by_default() -> bool {
    true
}

/// A normalized, validated server identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerId(String);

impl ServerId {
    /// Normalizes surrounding whitespace and ASCII case, then validates the ID.
    pub fn parse(value: &str) -> Result<Self, ServerIdError> {
        let normalized = value.trim().to_ascii_lowercase();
        let mut bytes = normalized.bytes();

        if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            || !bytes.all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
        {
            return Err(ServerIdError::InvalidFormat);
        }

        Ok(Self(normalized))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for ServerId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for ServerId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for ServerId {
    type Err = ServerIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ServerIdError {
    #[error("must match ^[a-z][a-z0-9._-]*$ after normalization")]
    InvalidFormat,
}

/// A resolved value that cannot be displayed or serialized accidentally.
#[derive(Clone)]
pub struct SecretValue(SecretString);

impl SecretValue {
    pub(crate) fn new(value: String) -> Self {
        Self(SecretString::from(value))
    }

    /// Explicitly exposes the secret to the future transport/process boundary.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// A validated manifest with all environment references resolved.
#[derive(Clone)]
pub struct ResolvedServerManifest {
    pub id: ServerId,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub transport: ResolvedTransportConfig,
}

impl fmt::Debug for ResolvedServerManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedServerManifest")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("description", &self.description)
            .field("enabled", &self.enabled)
            .field("transport", &self.transport)
            .finish()
    }
}

/// Transport settings ready for the future process or HTTP client layer.
#[derive(Clone)]
pub enum ResolvedTransportConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        working_directory: Option<PathBuf>,
        environment: BTreeMap<String, SecretValue>,
    },
    Http {
        url: Url,
        headers: BTreeMap<String, SecretValue>,
    },
}

impl fmt::Debug for ResolvedTransportConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                command,
                args,
                working_directory,
                environment,
            } => formatter
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("working_directory", working_directory)
                .field("environment", environment)
                .finish(),
            Self::Http { url, headers } => formatter
                .debug_struct("Http")
                .field("scheme", &url.scheme())
                .field("host", &url.host_str())
                .field("port", &url.port())
                .field("path", &url.path())
                .field("query", &url.query().map(|_| "<redacted>"))
                .field("headers", headers)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ServerId, ServerManifest, TransportConfig};

    #[test]
    fn parses_stdio_manifest() {
        let manifest: ServerManifest = toml::from_str(
            r#"
                id = "github"
                name = "GitHub"
                description = "GitHub MCP server"

                [transport]
                type = "stdio"
                command = "github-mcp-server"
                args = ["stdio", "--verbose"]
                working_directory = "./work"

                [transport.environment]
                GITHUB_TOKEN = "${GITHUB_TOKEN}"
            "#,
        )
        .expect("stdio manifest should parse");

        assert!(manifest.enabled);
        let TransportConfig::Stdio {
            args, environment, ..
        } = manifest.transport
        else {
            panic!("expected stdio transport");
        };
        assert_eq!(args, ["stdio", "--verbose"]);
        assert_eq!(environment["GITHUB_TOKEN"], "${GITHUB_TOKEN}");
    }

    #[test]
    fn parses_disabled_http_manifest() {
        let manifest: ServerManifest = toml::from_str(
            r#"
                id = "remote"
                name = "Remote"
                description = "Remote MCP server"
                enabled = false

                [transport]
                type = "http"
                url = "https://example.com/mcp"

                [transport.headers]
                Authorization = "${AUTH_HEADER}"
            "#,
        )
        .expect("HTTP manifest should parse");

        assert!(!manifest.enabled);
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let error = toml::from_str::<ServerManifest>(
            r#"
                id = "github"
                name = "GitHub"
                description = "GitHub MCP server"
                unexpected = true

                [transport]
                type = "stdio"
                command = "github-mcp-server"
            "#,
        )
        .expect_err("unknown fields must be rejected");

        assert!(error.message().contains("unknown field `unexpected`"));
    }

    #[test]
    fn rejects_transport_fields_from_another_variant() {
        let error = toml::from_str::<ServerManifest>(
            r#"
                id = "remote"
                name = "Remote"
                description = "Remote MCP server"

                [transport]
                type = "http"
                url = "https://example.com/mcp"
                command = "not-valid-for-http"
            "#,
        )
        .expect_err("transport-specific unknown fields must be rejected");

        assert!(error.message().contains("unknown field `command`"));
    }

    #[test]
    fn normalizes_server_id() {
        let id = ServerId::parse("  GitHub.Tools  ").expect("ID should normalize");
        assert_eq!(id.as_str(), "github.tools");
    }

    #[test]
    fn raw_debug_redacts_sensitive_values() {
        let manifest: ServerManifest = toml::from_str(
            r#"
                id = "github"
                name = "GitHub"
                description = "GitHub MCP server"

                [transport]
                type = "stdio"
                command = "github-mcp-server"

                [transport.environment]
                TOKEN = "sentinel-secret"
            "#,
        )
        .expect("manifest should parse");

        assert!(!format!("{manifest:?}").contains("sentinel-secret"));
    }
}
