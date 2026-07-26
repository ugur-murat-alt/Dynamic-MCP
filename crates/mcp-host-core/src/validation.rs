use std::{collections::BTreeMap, path::Path};

use thiserror::Error;
use url::{ParseError, Url};

use crate::{
    environment::{EnvironmentAccessError, EnvironmentProvider},
    manifest::{
        ResolvedServerManifest, ResolvedTransportConfig, SecretValue, ServerId, ServerIdError,
        ServerManifest, TransportConfig,
    },
};

pub(crate) struct ValidatedManifest {
    id: ServerId,
    name: String,
    description: String,
    enabled: bool,
    transport: ValidatedTransportConfig,
}

enum ValidatedTransportConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        working_directory: Option<String>,
        environment: BTreeMap<String, String>,
    },
    Http {
        url: Url,
        headers: BTreeMap<String, String>,
    },
}

pub(crate) fn validate_manifest(
    manifest: &ServerManifest,
) -> Result<ValidatedManifest, ManifestValidationError> {
    let id = ServerId::parse(&manifest.id).map_err(|source| {
        ManifestValidationError::InvalidServerId {
            field: "id",
            source,
        }
    })?;

    if manifest.name.trim().is_empty() {
        return Err(ManifestValidationError::EmptyField { field: "name" });
    }

    let transport = match &manifest.transport {
        TransportConfig::Stdio {
            command,
            args,
            working_directory,
            environment,
        } => {
            if command.trim().is_empty() {
                return Err(ManifestValidationError::InvalidTransportConfiguration {
                    field: "transport.command".to_owned(),
                    reason: "must not be empty",
                });
            }
            if working_directory
                .as_deref()
                .is_some_and(|directory| directory.trim().is_empty())
            {
                return Err(ManifestValidationError::InvalidTransportConfiguration {
                    field: "transport.working_directory".to_owned(),
                    reason: "must not be empty",
                });
            }
            if environment.keys().any(|key| key.trim().is_empty()) {
                return Err(ManifestValidationError::InvalidTransportConfiguration {
                    field: "transport.environment".to_owned(),
                    reason: "keys must not be empty",
                });
            }

            ValidatedTransportConfig::Stdio {
                command: command.clone(),
                args: args.clone(),
                working_directory: working_directory.clone(),
                environment: environment.clone(),
            }
        }
        TransportConfig::Http { url, headers } => {
            let url = Url::parse(url).map_err(|source| ManifestValidationError::InvalidUrl {
                field: "transport.url",
                source,
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(ManifestValidationError::UnsupportedUrlScheme {
                    field: "transport.url",
                    scheme: url.scheme().to_owned(),
                });
            }
            if url.host_str().is_none() {
                return Err(ManifestValidationError::UrlHostRequired {
                    field: "transport.url",
                });
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err(ManifestValidationError::UrlCredentialsNotAllowed {
                    field: "transport.url",
                });
            }
            for name in headers.keys() {
                if !is_valid_header_name(name) {
                    return Err(ManifestValidationError::InvalidTransportConfiguration {
                        field: format!("transport.headers.{name}"),
                        reason: "must be a non-empty HTTP token",
                    });
                }
            }

            ValidatedTransportConfig::Http {
                url,
                headers: headers.clone(),
            }
        }
    };

    Ok(ValidatedManifest {
        id,
        name: manifest.name.clone(),
        description: manifest.description.clone(),
        enabled: manifest.enabled,
        transport,
    })
}

pub(crate) fn resolve_manifest<E: EnvironmentProvider>(
    manifest: ValidatedManifest,
    source_path: &Path,
    environment: &E,
) -> Result<ResolvedServerManifest, EnvironmentResolutionError> {
    let transport = match manifest.transport {
        ValidatedTransportConfig::Stdio {
            command,
            args,
            working_directory,
            environment: raw_environment,
        } => {
            let environment = resolve_map(raw_environment, "transport.environment", environment)?;
            let working_directory = working_directory.map(|directory| {
                let directory = std::path::PathBuf::from(directory);
                if directory.is_relative() {
                    source_path
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(directory)
                } else {
                    directory
                }
            });

            ResolvedTransportConfig::Stdio {
                command,
                args,
                working_directory,
                environment,
            }
        }
        ValidatedTransportConfig::Http { url, headers } => ResolvedTransportConfig::Http {
            url,
            headers: resolve_map(headers, "transport.headers", environment)?,
        },
    };

    Ok(ResolvedServerManifest {
        id: manifest.id,
        name: manifest.name,
        description: manifest.description,
        enabled: manifest.enabled,
        transport,
    })
}

fn resolve_map<E: EnvironmentProvider>(
    values: BTreeMap<String, String>,
    field_prefix: &str,
    environment: &E,
) -> Result<BTreeMap<String, SecretValue>, EnvironmentResolutionError> {
    values
        .into_iter()
        .map(|(key, value)| {
            let field = format!("{field_prefix}.{key}");
            resolve_value(&value, &field, environment).map(|value| (key, value))
        })
        .collect()
}

fn resolve_value<E: EnvironmentProvider>(
    value: &str,
    field: &str,
    environment: &E,
) -> Result<SecretValue, EnvironmentResolutionError> {
    let Some(variable) = value
        .strip_prefix("${")
        .and_then(|candidate| candidate.strip_suffix('}'))
    else {
        return Ok(SecretValue::new(value.to_owned()));
    };

    if !is_valid_environment_name(variable) {
        return Err(EnvironmentResolutionError::MalformedReference {
            field: field.to_owned(),
            variable: variable.to_owned(),
        });
    }

    let resolved =
        environment
            .get(variable)
            .map_err(|source| EnvironmentResolutionError::Provider {
                field: field.to_owned(),
                variable: variable.to_owned(),
                source,
            })?;
    let resolved = resolved.ok_or_else(|| EnvironmentResolutionError::MissingVariable {
        field: field.to_owned(),
        variable: variable.to_owned(),
    })?;

    Ok(SecretValue::new(resolved))
}

fn is_valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[derive(Debug, Error)]
pub enum ManifestValidationError {
    #[error("field `{field}` contains an invalid server ID: {source}")]
    InvalidServerId {
        field: &'static str,
        #[source]
        source: ServerIdError,
    },
    #[error("field `{field}` must not be empty")]
    EmptyField { field: &'static str },
    #[error("field `{field}` has invalid transport configuration: {reason}")]
    InvalidTransportConfiguration { field: String, reason: &'static str },
    #[error("field `{field}` contains an invalid URL: {source}")]
    InvalidUrl {
        field: &'static str,
        #[source]
        source: ParseError,
    },
    #[error("field `{field}` uses unsupported URL scheme `{scheme}`")]
    UnsupportedUrlScheme { field: &'static str, scheme: String },
    #[error("field `{field}` requires a URL host")]
    UrlHostRequired { field: &'static str },
    #[error("field `{field}` must not contain embedded credentials")]
    UrlCredentialsNotAllowed { field: &'static str },
}

#[derive(Debug, Error)]
pub enum EnvironmentResolutionError {
    #[error("field `{field}` references missing environment variable `{variable}`")]
    MissingVariable { field: String, variable: String },
    #[error("field `{field}` contains malformed environment reference `${{{variable}}}`")]
    MalformedReference { field: String, variable: String },
    #[error("field `{field}` could not read environment variable `{variable}`: {source}")]
    Provider {
        field: String,
        variable: String,
        #[source]
        source: EnvironmentAccessError,
    },
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use crate::{
        environment::{EnvironmentAccessError, EnvironmentProvider},
        manifest::{ResolvedTransportConfig, ServerManifest},
    };

    use super::{
        EnvironmentResolutionError, ManifestValidationError, resolve_manifest, validate_manifest,
    };

    #[derive(Default)]
    struct TestEnvironment(BTreeMap<String, String>);

    impl EnvironmentProvider for TestEnvironment {
        fn get(&self, name: &str) -> Result<Option<String>, EnvironmentAccessError> {
            Ok(self.0.get(name).cloned())
        }
    }

    #[test]
    fn rejects_empty_and_invalid_server_ids() {
        for id in ["", "   ", "1server", "server name", "server/one"] {
            let manifest = stdio_manifest(id, "Server", "command", None);
            assert!(matches!(
                validate_manifest(&manifest),
                Err(ManifestValidationError::InvalidServerId { field: "id", .. })
            ));
        }
    }

    #[test]
    fn rejects_empty_name_and_command() {
        let empty_name = stdio_manifest("server", " ", "command", None);
        assert!(matches!(
            validate_manifest(&empty_name),
            Err(ManifestValidationError::EmptyField { field: "name" })
        ));

        let empty_command = stdio_manifest("server", "Server", " ", None);
        assert!(matches!(
            validate_manifest(&empty_command),
            Err(ManifestValidationError::InvalidTransportConfiguration { ref field, .. })
                if field == "transport.command"
        ));
    }

    #[test]
    fn rejects_empty_working_directory_and_environment_key() {
        let empty_directory = stdio_manifest("server", "Server", "command", Some(" "));
        assert!(matches!(
            validate_manifest(&empty_directory),
            Err(ManifestValidationError::InvalidTransportConfiguration { ref field, .. })
                if field == "transport.working_directory"
        ));

        let manifest: ServerManifest = toml::from_str(
            r#"
                id = "server"
                name = "Server"
                description = "Test"
                [transport]
                type = "stdio"
                command = "server"
                [transport.environment]
                "" = "value"
            "#,
        )
        .expect("TOML supports a quoted empty key");
        assert!(matches!(
            validate_manifest(&manifest),
            Err(ManifestValidationError::InvalidTransportConfiguration { ref field, .. })
                if field == "transport.environment"
        ));
    }

    #[test]
    fn rejects_invalid_urls_and_schemes() {
        let invalid = http_manifest("not a URL");
        assert!(matches!(
            validate_manifest(&invalid),
            Err(ManifestValidationError::InvalidUrl { .. })
        ));

        let unsupported = http_manifest("ftp://example.com/mcp");
        assert!(matches!(
            validate_manifest(&unsupported),
            Err(ManifestValidationError::UnsupportedUrlScheme { ref scheme, .. })
                if scheme == "ftp"
        ));

        let credentials = http_manifest("https://user:password@example.com/mcp");
        assert!(matches!(
            validate_manifest(&credentials),
            Err(ManifestValidationError::UrlCredentialsNotAllowed { .. })
        ));
    }

    #[test]
    fn rejects_invalid_http_header_name() {
        let manifest: ServerManifest = toml::from_str(
            r#"
                id = "remote"
                name = "Remote"
                description = "Test"
                [transport]
                type = "http"
                url = "https://example.com/mcp"
                [transport.headers]
                "" = "value"
            "#,
        )
        .expect("TOML supports a quoted empty key");

        assert!(matches!(
            validate_manifest(&manifest),
            Err(ManifestValidationError::InvalidTransportConfiguration { ref field, .. })
                if field == "transport.headers."
        ));
    }

    #[test]
    fn resolves_environment_reference_and_accepts_empty_value() {
        let manifest = stdio_manifest_with_environment("${TOKEN}");
        let validated = validate_manifest(&manifest).expect("manifest should validate");
        let environment = TestEnvironment(BTreeMap::from([("TOKEN".to_owned(), String::new())]));
        let resolved = resolve_manifest(validated, Path::new("config/server.toml"), &environment)
            .expect("empty environment values are valid");

        let ResolvedTransportConfig::Stdio { environment, .. } = resolved.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(environment["TOKEN"].expose_secret(), "");
    }

    #[test]
    fn missing_environment_variable_is_typed_and_redacted() {
        let manifest = stdio_manifest_with_environment("${TOKEN}");
        let validated = validate_manifest(&manifest).expect("manifest should validate");
        let error = resolve_manifest(
            validated,
            Path::new("config/server.toml"),
            &TestEnvironment::default(),
        )
        .expect_err("missing variable must fail");

        assert!(matches!(
            error,
            EnvironmentResolutionError::MissingVariable { ref variable, .. }
                if variable == "TOKEN"
        ));
        assert!(!format!("{error:?}").contains("sentinel-secret"));
    }

    #[test]
    fn embedded_reference_remains_literal_and_malformed_full_reference_fails() {
        let literal = stdio_manifest_with_environment("prefix-${TOKEN}");
        let resolved = resolve_manifest(
            validate_manifest(&literal).expect("manifest should validate"),
            Path::new("server.toml"),
            &TestEnvironment::default(),
        )
        .expect("embedded references stay literal");
        let ResolvedTransportConfig::Stdio { environment, .. } = resolved.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(environment["TOKEN"].expose_secret(), "prefix-${TOKEN}");

        for value in ["${BAD-NAME}", "${}"] {
            let malformed = stdio_manifest_with_environment(value);
            let error = resolve_manifest(
                validate_manifest(&malformed).expect("manifest should validate"),
                Path::new("server.toml"),
                &TestEnvironment::default(),
            )
            .expect_err("malformed full reference must fail");
            assert!(matches!(
                error,
                EnvironmentResolutionError::MalformedReference { .. }
            ));
        }
    }

    #[test]
    fn resolved_debug_redacts_secrets_and_url_query() {
        let manifest: ServerManifest = toml::from_str(
            r#"
                id = "remote"
                name = "Remote"
                description = "Test"
                [transport]
                type = "http"
                url = "https://example.com/mcp?api_key=url-secret"
                [transport.headers]
                Authorization = "${AUTH}"
            "#,
        )
        .expect("manifest should parse");
        let environment = TestEnvironment(BTreeMap::from([(
            "AUTH".to_owned(),
            "header-secret".to_owned(),
        )]));
        let resolved = resolve_manifest(
            validate_manifest(&manifest).expect("manifest should validate"),
            Path::new("remote.toml"),
            &environment,
        )
        .expect("manifest should resolve");
        let debug = format!("{resolved:?}");

        assert!(!debug.contains("header-secret"));
        assert!(!debug.contains("url-secret"));
        assert!(debug.contains("<redacted>"));
    }

    fn stdio_manifest(
        id: &str,
        name: &str,
        command: &str,
        working_directory: Option<&str>,
    ) -> ServerManifest {
        let working_directory = working_directory
            .map(|directory| format!("working_directory = {directory:?}"))
            .unwrap_or_default();
        toml::from_str(&format!(
            r#"
                id = {id:?}
                name = {name:?}
                description = "Test"
                [transport]
                type = "stdio"
                command = {command:?}
                {working_directory}
            "#
        ))
        .expect("test manifest should parse")
    }

    fn stdio_manifest_with_environment(value: &str) -> ServerManifest {
        toml::from_str(&format!(
            r#"
                id = "server"
                name = "Server"
                description = "Test"
                [transport]
                type = "stdio"
                command = "server"
                [transport.environment]
                TOKEN = {value:?}
            "#
        ))
        .expect("test manifest should parse")
    }

    fn http_manifest(url: &str) -> ServerManifest {
        toml::from_str(&format!(
            r#"
                id = "remote"
                name = "Remote"
                description = "Test"
                [transport]
                type = "http"
                url = {url:?}
            "#
        ))
        .expect("test manifest should parse")
    }
}
