use std::{collections::BTreeMap, fs, path::PathBuf};

use mcp_host_core::{
    EnvironmentAccessError, EnvironmentProvider, ManifestLoader, McpServerRegistry,
    RegistryBuildError, RegistryBuilder, ResolvedTransportConfig, ServerId,
};
use tempfile::tempdir;

#[derive(Default)]
struct MapEnvironment(BTreeMap<String, String>);

impl EnvironmentProvider for MapEnvironment {
    fn get(&self, name: &str) -> Result<Option<String>, EnvironmentAccessError> {
        Ok(self.0.get(name).cloned())
    }
}

#[test]
fn loads_real_files_and_builds_registry_snapshot() {
    let directory = tempdir().expect("temporary directory should be created");
    let filesystem_path = directory.path().join("filesystem.toml");
    let remote_path = directory.path().join("remote.toml");
    fs::write(&filesystem_path, FILESYSTEM_MANIFEST).expect("filesystem fixture should write");
    fs::write(&remote_path, REMOTE_MANIFEST).expect("remote fixture should write");
    let loader = ManifestLoader::new(MapEnvironment(BTreeMap::from([
        (
            "FILESYSTEM_TOKEN".to_owned(),
            "filesystem-secret".to_owned(),
        ),
        ("REMOTE_AUTH".to_owned(), "remote-secret".to_owned()),
    ])));

    let loaded = loader
        .load_directory(directory.path())
        .expect("manifest directory should load");
    let registry = RegistryBuilder::build(loaded).expect("registry should build");

    assert_eq!(registry.len(), 2);
    assert_server_metadata(&registry, "filesystem", &filesystem_path, true);
    assert_server_metadata(&registry, "remote", &remote_path, false);

    let filesystem = registry
        .get(&ServerId::parse("filesystem").expect("ID should parse"))
        .expect("filesystem server should exist");
    let ResolvedTransportConfig::Stdio {
        args,
        working_directory,
        environment,
        ..
    } = &filesystem.resolved_manifest().transport
    else {
        panic!("filesystem should use stdio");
    };
    assert_eq!(args, &["--root", "."]);
    assert_eq!(
        working_directory.as_ref(),
        Some(&directory.path().join("workspace"))
    );
    assert_eq!(
        environment["FILESYSTEM_TOKEN"].expose_secret(),
        "filesystem-secret"
    );

    let registry_debug = format!("{registry:?}");
    assert!(!registry_debug.contains("filesystem-secret"));
    assert!(!registry_debug.contains("remote-secret"));
}

#[test]
fn duplicate_id_fails_after_real_directory_reload() {
    let directory = tempdir().expect("temporary directory should be created");
    let first_path = directory.path().join("a-filesystem.toml");
    let duplicate_path = directory.path().join("z-duplicate.toml");
    fs::write(&first_path, FILESYSTEM_MANIFEST).expect("filesystem fixture should write");
    fs::write(
        &duplicate_path,
        FILESYSTEM_MANIFEST.replace("id = \"filesystem\"", "id = \" FileSystem \""),
    )
    .expect("duplicate fixture should write");
    let loader = ManifestLoader::new(MapEnvironment(BTreeMap::from([(
        "FILESYSTEM_TOKEN".to_owned(),
        "secret".to_owned(),
    )])));

    let loaded = loader
        .load_directory(directory.path())
        .expect("manifests should load");
    let error = RegistryBuilder::build(loaded).expect_err("duplicate ID must fail");

    assert_eq!(
        error,
        RegistryBuildError::DuplicateServerId {
            id: ServerId::parse("filesystem").expect("ID should parse"),
            first_path,
            second_path: duplicate_path,
        }
    );
}

#[test]
fn repository_examples_parse_without_real_credentials() {
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/examples");
    let loader = ManifestLoader::new(MapEnvironment(BTreeMap::from([(
        "GITHUB_TOKEN".to_owned(),
        "test-only-token".to_owned(),
    )])));

    let loaded = loader
        .load_directory(&examples)
        .expect("repository examples should load");
    let registry = RegistryBuilder::build(loaded).expect("example registry should build");

    assert_eq!(registry.len(), 2);
    assert!(registry.contains(&ServerId::parse("filesystem").expect("ID should parse")));
    assert!(registry.contains(&ServerId::parse("github").expect("ID should parse")));
}

fn assert_server_metadata(
    registry: &McpServerRegistry,
    id: &str,
    source_path: &std::path::Path,
    enabled: bool,
) {
    let server_id = ServerId::parse(id).expect("fixture ID should parse");
    let server = registry.get(&server_id).expect("server should exist");
    assert_eq!(server.id(), &server_id);
    assert_eq!(server.source_path(), source_path);
    assert_eq!(server.enabled(), enabled);
}

const FILESYSTEM_MANIFEST: &str = r#"
id = "filesystem"
name = "Filesystem"
description = "Filesystem MCP server"

[transport]
type = "stdio"
command = "filesystem-mcp-server"
args = ["--root", "."]
working_directory = "workspace"

[transport.environment]
FILESYSTEM_TOKEN = "${FILESYSTEM_TOKEN}"
"#;

const REMOTE_MANIFEST: &str = r#"
id = "remote"
name = "Remote"
description = "Remote Streamable HTTP MCP server"
enabled = false

[transport]
type = "http"
url = "https://example.com/mcp?mode=test"

[transport.headers]
Authorization = "${REMOTE_AUTH}"
"#;
