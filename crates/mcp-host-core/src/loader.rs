use std::{
    cmp::Ordering,
    ffi::OsStr,
    fs, io,
    ops::Range,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    environment::{EnvironmentProvider, ProcessEnvironment},
    manifest::{ResolvedServerManifest, ServerManifest},
    validation::{
        EnvironmentResolutionError, ManifestValidationError, resolve_manifest, validate_manifest,
    },
};

/// Loads, validates, and resolves TOML manifests from the filesystem.
pub struct ManifestLoader<E> {
    environment: E,
}

impl<E> ManifestLoader<E> {
    #[must_use]
    pub const fn new(environment: E) -> Self {
        Self { environment }
    }
}

impl Default for ManifestLoader<ProcessEnvironment> {
    fn default() -> Self {
        Self::new(ProcessEnvironment)
    }
}

impl<E: EnvironmentProvider> ManifestLoader<E> {
    /// Loads direct child `.toml` files in a deterministic filename order.
    pub fn load_directory(
        &self,
        directory: &Path,
    ) -> Result<Vec<LoadedManifest>, ManifestLoadError> {
        let entries = fs::read_dir(directory).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                ManifestLoadError::DirectoryNotFound {
                    path: directory.to_path_buf(),
                }
            } else {
                ManifestLoadError::DirectoryUnreadable {
                    path: directory.to_path_buf(),
                    source,
                }
            }
        })?;

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ManifestLoadError::DirectoryUnreadable {
                path: directory.to_path_buf(),
                source,
            })?;
            let file_type =
                entry
                    .file_type()
                    .map_err(|source| ManifestLoadError::DirectoryUnreadable {
                        path: directory.to_path_buf(),
                        source,
                    })?;
            let path = entry.path();

            if file_type.is_file() && is_discoverable_manifest(&path) {
                paths.push(path);
            }
        }

        paths.sort_by(|left, right| compare_source_paths(left, right));
        paths.iter().map(|path| self.load_file(path)).collect()
    }

    /// Loads one explicitly selected TOML manifest.
    pub fn load_file(&self, path: &Path) -> Result<LoadedManifest, ManifestLoadError> {
        if !has_toml_extension(path) {
            return Err(ManifestLoadError::UnsupportedManifestFormat {
                path: path.to_path_buf(),
            });
        }

        let contents = fs::read_to_string(path).map_err(|source| {
            ManifestLoadError::ManifestFileUnreadable {
                path: path.to_path_buf(),
                source,
            }
        })?;
        let raw: ServerManifest =
            toml::from_str(&contents).map_err(|error| ManifestLoadError::ManifestParse {
                path: path.to_path_buf(),
                span: error.span(),
            })?;
        let validated =
            validate_manifest(&raw).map_err(|source| ManifestLoadError::ManifestValidation {
                path: path.to_path_buf(),
                source,
            })?;
        let resolved = resolve_manifest(validated, path, &self.environment).map_err(|source| {
            ManifestLoadError::EnvironmentResolution {
                path: path.to_path_buf(),
                source,
            }
        })?;

        Ok(LoadedManifest {
            source_path: path.to_path_buf(),
            raw,
            resolved,
        })
    }
}

/// A source manifest paired with its validated, resolved form.
#[derive(Clone, Debug)]
pub struct LoadedManifest {
    source_path: PathBuf,
    raw: ServerManifest,
    resolved: ResolvedServerManifest,
}

impl LoadedManifest {
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

    pub(crate) fn into_parts(self) -> (PathBuf, ServerManifest, ResolvedServerManifest) {
        (self.source_path, self.raw, self.resolved)
    }
}

#[derive(Debug, Error)]
pub enum ManifestLoadError {
    #[error("manifest directory `{path}` was not found", path = path.display())]
    DirectoryNotFound { path: PathBuf },
    #[error("manifest directory `{path}` could not be read: {source}", path = path.display())]
    DirectoryUnreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("manifest file `{path}` could not be read: {source}", path = path.display())]
    ManifestFileUnreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("manifest file `{path}` has an unsupported format", path = path.display())]
    UnsupportedManifestFormat { path: PathBuf },
    #[error(
        "manifest file `{path}` contains invalid TOML or manifest structure at byte range {span:?}",
        path = path.display()
    )]
    ManifestParse {
        path: PathBuf,
        span: Option<Range<usize>>,
    },
    #[error("manifest file `{path}` failed validation: {source}", path = path.display())]
    ManifestValidation {
        path: PathBuf,
        #[source]
        source: ManifestValidationError,
    },
    #[error(
        "manifest file `{path}` failed environment resolution: {source}",
        path = path.display()
    )]
    EnvironmentResolution {
        path: PathBuf,
        #[source]
        source: EnvironmentResolutionError,
    },
}

pub(crate) fn compare_source_paths(left: &Path, right: &Path) -> Ordering {
    let left_name = left.file_name().unwrap_or_else(|| OsStr::new(""));
    let right_name = right.file_name().unwrap_or_else(|| OsStr::new(""));

    left_name
        .to_string_lossy()
        .cmp(&right_name.to_string_lossy())
        .then_with(|| left_name.cmp(right_name))
        .then_with(|| left.cmp(right))
}

fn is_discoverable_manifest(path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    let file_name = file_name.to_string_lossy();

    !file_name.starts_with(['.', '~', '#']) && has_toml_extension(path)
}

fn has_toml_extension(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("toml"))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::Path};

    use tempfile::tempdir;

    use crate::environment::{EnvironmentAccessError, EnvironmentProvider};

    use super::{ManifestLoadError, ManifestLoader};

    #[derive(Default)]
    struct TestEnvironment(BTreeMap<String, String>);

    impl EnvironmentProvider for TestEnvironment {
        fn get(&self, name: &str) -> Result<Option<String>, EnvironmentAccessError> {
            Ok(self.0.get(name).cloned())
        }
    }

    #[test]
    fn discovers_supported_files_in_deterministic_order() {
        let directory = tempdir().expect("temporary directory should be created");
        write_manifest(&directory.path().join("z-last.toml"), "z-last");
        write_manifest(&directory.path().join("a-first.toml"), "a-first");
        write_manifest(&directory.path().join(".hidden.toml"), "hidden");
        write_manifest(&directory.path().join("~backup.toml"), "backup");
        write_manifest(&directory.path().join("uppercase.TOML"), "uppercase");
        fs::write(directory.path().join("ignored.json"), "{}").expect("fixture should write");
        fs::write(directory.path().join("ignored.toml~"), "temporary")
            .expect("fixture should write");
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).expect("nested directory should be created");
        write_manifest(&nested.join("nested.toml"), "nested");

        let loaded = ManifestLoader::new(TestEnvironment::default())
            .load_directory(directory.path())
            .expect("directory should load");
        let names = loaded
            .iter()
            .map(|manifest| {
                manifest
                    .source_path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("fixture names are UTF-8")
            })
            .collect::<Vec<_>>();

        assert_eq!(names, ["a-first.toml", "z-last.toml"]);
    }

    #[test]
    fn empty_directory_is_valid() {
        let directory = tempdir().expect("temporary directory should be created");
        let loaded = ManifestLoader::new(TestEnvironment::default())
            .load_directory(directory.path())
            .expect("empty directory should load");
        assert!(loaded.is_empty());
    }

    #[test]
    fn missing_directory_is_typed() {
        let directory = tempdir().expect("temporary directory should be created");
        let missing = directory.path().join("missing");
        let error = ManifestLoader::new(TestEnvironment::default())
            .load_directory(&missing)
            .expect_err("missing directory must fail");

        assert!(matches!(
            error,
            ManifestLoadError::DirectoryNotFound { ref path } if path == &missing
        ));
    }

    #[test]
    fn non_directory_path_is_typed_as_unreadable_directory() {
        let directory = tempdir().expect("temporary directory should be created");
        let file = directory.path().join("not-a-directory");
        fs::write(&file, "content").expect("fixture should write");
        let error = ManifestLoader::new(TestEnvironment::default())
            .load_directory(&file)
            .expect_err("a file is not a manifest directory");

        assert!(matches!(
            error,
            ManifestLoadError::DirectoryUnreadable { ref path, .. } if path == &file
        ));
    }

    #[test]
    fn unreadable_manifest_file_is_typed() {
        let directory = tempdir().expect("temporary directory should be created");
        let unreadable = directory.path().join("directory.toml");
        fs::create_dir(&unreadable).expect("fixture directory should be created");
        let error = ManifestLoader::new(TestEnvironment::default())
            .load_file(&unreadable)
            .expect_err("reading a directory as a manifest must fail");

        assert!(matches!(
            error,
            ManifestLoadError::ManifestFileUnreadable { ref path, .. } if path == &unreadable
        ));
    }

    #[test]
    fn unsupported_explicit_format_is_typed() {
        let error = ManifestLoader::new(TestEnvironment::default())
            .load_file(Path::new("server.json"))
            .expect_err("unsupported format must fail before reading");
        assert!(matches!(
            error,
            ManifestLoadError::UnsupportedManifestFormat { .. }
        ));
    }

    #[test]
    fn parse_error_does_not_retain_source_secret() {
        let directory = tempdir().expect("temporary directory should be created");
        let path = directory.path().join("invalid.toml");
        fs::write(
            &path,
            r#"
                id = "server"
                name = "Server"
                description = "Test"
                [transport]
                type = "stdio"
                command = "server"
                [transport.environment]
                TOKEN = "sentinel-secret"
                TOKEN = "duplicate"
            "#,
        )
        .expect("fixture should write");

        let error = ManifestLoader::new(TestEnvironment::default())
            .load_file(&path)
            .expect_err("duplicate key must fail parsing");
        assert!(matches!(error, ManifestLoadError::ManifestParse { .. }));
        assert!(!format!("{error:?}").contains("sentinel-secret"));
        assert!(!error.to_string().contains("sentinel-secret"));
    }

    #[test]
    fn invalid_type_parse_error_does_not_retain_source_secret() {
        let directory = tempdir().expect("temporary directory should be created");
        let path = directory.path().join("invalid-type.toml");
        fs::write(
            &path,
            r#"
                id = "server"
                name = "Server"
                description = "Test"
                enabled = "sentinel-secret"
                [transport]
                type = "stdio"
                command = "server"
            "#,
        )
        .expect("fixture should write");

        let error = ManifestLoader::new(TestEnvironment::default())
            .load_file(&path)
            .expect_err("invalid field type must fail parsing");
        assert!(matches!(error, ManifestLoadError::ManifestParse { .. }));
        assert!(!format!("{error:?}").contains("sentinel-secret"));
        assert!(!error.to_string().contains("sentinel-secret"));
    }

    #[test]
    fn missing_environment_variable_keeps_source_path() {
        let directory = tempdir().expect("temporary directory should be created");
        let path = directory.path().join("missing-env.toml");
        fs::write(
            &path,
            r#"
                id = "server"
                name = "Server"
                description = "Test"
                [transport]
                type = "stdio"
                command = "server"
                [transport.environment]
                TOKEN = "${MISSING_TOKEN}"
            "#,
        )
        .expect("fixture should write");

        let error = ManifestLoader::new(TestEnvironment::default())
            .load_file(&path)
            .expect_err("missing environment variable must fail");
        assert!(matches!(
            error,
            ManifestLoadError::EnvironmentResolution { path: ref error_path, .. }
                if error_path == &path
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_manifest_is_not_discovered() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("temporary directory should be created");
        let target = directory.path().join("target.txt");
        fs::write(&target, valid_manifest("target")).expect("fixture should write");
        symlink(&target, directory.path().join("alias.toml")).expect("symlink should be created");

        let loaded = ManifestLoader::new(TestEnvironment::default())
            .load_directory(directory.path())
            .expect("directory should load");
        assert!(loaded.is_empty());
    }

    fn write_manifest(path: &Path, id: &str) {
        fs::write(path, valid_manifest(id)).expect("fixture should write");
    }

    fn valid_manifest(id: &str) -> String {
        format!(
            r#"
                id = {id:?}
                name = "Server"
                description = "Test"
                [transport]
                type = "stdio"
                command = "server"
            "#
        )
    }
}
