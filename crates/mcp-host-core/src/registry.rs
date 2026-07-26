use std::{collections::BTreeMap, path::Path, path::PathBuf};

use thiserror::Error;

use crate::{
    loader::{LoadedManifest, compare_source_paths},
    manifest::{ResolvedServerManifest, ServerId, ServerManifest},
};

/// Builds immutable registry snapshots from loaded manifests.
#[derive(Debug, Default, Clone, Copy)]
pub struct RegistryBuilder;

impl RegistryBuilder {
    pub fn build(
        mut manifests: Vec<LoadedManifest>,
    ) -> Result<McpServerRegistry, RegistryBuildError> {
        manifests
            .sort_by(|left, right| compare_source_paths(left.source_path(), right.source_path()));

        let mut servers: BTreeMap<ServerId, RegisteredServer> = BTreeMap::new();
        for manifest in manifests {
            let (source_path, raw, resolved) = manifest.into_parts();
            let id = resolved.id.clone();
            if let Some(first) = servers.get(&id) {
                return Err(RegistryBuildError::DuplicateServerId {
                    id,
                    first_path: first.source_path.clone(),
                    second_path: source_path,
                });
            }

            servers.insert(
                id,
                RegisteredServer {
                    source_path,
                    raw,
                    resolved,
                },
            );
        }

        Ok(McpServerRegistry { servers })
    }
}

/// An immutable, deterministically ordered configuration registry snapshot.
#[derive(Debug, Default, Clone)]
pub struct McpServerRegistry {
    servers: BTreeMap<ServerId, RegisteredServer>,
}

impl McpServerRegistry {
    #[must_use]
    pub fn get(&self, id: &ServerId) -> Option<&RegisteredServer> {
        self.servers.get(id)
    }

    #[must_use]
    pub fn contains(&self, id: &ServerId) -> bool {
        self.servers.contains_key(id)
    }

    /// Iterates by normalized server ID.
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredServer> {
        self.servers.values()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

/// One registry entry with source, raw, and resolved configuration metadata.
#[derive(Debug, Clone)]
pub struct RegisteredServer {
    source_path: PathBuf,
    raw: ServerManifest,
    resolved: ResolvedServerManifest,
}

impl RegisteredServer {
    #[must_use]
    pub fn id(&self) -> &ServerId {
        &self.resolved.id
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.resolved.enabled
    }

    #[must_use]
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    #[must_use]
    pub fn raw_manifest(&self) -> &ServerManifest {
        &self.raw
    }

    #[must_use]
    pub fn resolved_manifest(&self) -> &ResolvedServerManifest {
        &self.resolved
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegistryBuildError {
    #[error(
        "duplicate server ID `{id}` in `{first}` and `{second}`",
        first = first_path.display(),
        second = second_path.display()
    )]
    DuplicateServerId {
        id: ServerId,
        first_path: PathBuf,
        second_path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use tempfile::tempdir;

    use crate::{
        environment::{EnvironmentAccessError, EnvironmentProvider},
        loader::ManifestLoader,
        manifest::ServerId,
    };

    use super::{McpServerRegistry, RegistryBuildError, RegistryBuilder};

    struct EmptyEnvironment;

    impl EnvironmentProvider for EmptyEnvironment {
        fn get(&self, _name: &str) -> Result<Option<String>, EnvironmentAccessError> {
            Ok(None)
        }
    }

    #[test]
    fn registry_queries_and_iteration_are_deterministic() {
        let directory = tempdir().expect("temporary directory should be created");
        fs::write(
            directory.path().join("first.toml"),
            manifest("z-server", true),
        )
        .expect("fixture should write");
        fs::write(
            directory.path().join("second.toml"),
            manifest("a-server", false),
        )
        .expect("fixture should write");
        let loaded = ManifestLoader::new(EmptyEnvironment)
            .load_directory(directory.path())
            .expect("manifests should load");
        let registry = RegistryBuilder::build(loaded).expect("registry should build");
        let disabled = ServerId::parse("a-server").expect("ID should parse");

        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());
        assert!(registry.contains(&disabled));
        assert!(
            !registry
                .get(&disabled)
                .expect("server should exist")
                .enabled()
        );
        assert_eq!(
            registry
                .iter()
                .map(|server| server.id().as_str())
                .collect::<Vec<_>>(),
            ["a-server", "z-server"]
        );
    }

    #[test]
    fn empty_input_builds_empty_registry() {
        let registry = RegistryBuilder::build(Vec::new()).expect("empty registry should build");
        assert_registry_empty(&registry);
    }

    #[test]
    fn duplicate_normalized_id_reports_deterministic_paths() {
        let directory = tempdir().expect("temporary directory should be created");
        let first = directory.path().join("a-first.toml");
        let second = directory.path().join("z-second.toml");
        fs::write(&first, manifest("GitHub", true)).expect("fixture should write");
        fs::write(&second, manifest(" github ", true)).expect("fixture should write");
        let mut loaded = ManifestLoader::new(EmptyEnvironment)
            .load_directory(directory.path())
            .expect("manifests should load");
        loaded.reverse();

        let error = RegistryBuilder::build(loaded).expect_err("duplicate ID must fail");
        assert_eq!(
            error,
            RegistryBuildError::DuplicateServerId {
                id: ServerId::parse("github").expect("ID should parse"),
                first_path: first,
                second_path: second,
            }
        );
    }

    fn assert_registry_empty(registry: &McpServerRegistry) {
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
        assert_eq!(registry.iter().count(), 0);
    }

    fn manifest(id: &str, enabled: bool) -> String {
        let values = BTreeMap::from([("id", format!("{id:?}"))]);
        format!(
            r#"
                id = {}
                name = "Server"
                description = "Test"
                enabled = {enabled}
                [transport]
                type = "stdio"
                command = "server"
            "#,
            values["id"]
        )
    }
}
