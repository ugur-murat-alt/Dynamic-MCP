use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use mcp_host_core::{
    PackageInstallResult, PackageProvider, ProvisionConfig, RuntimeError, RuntimeErrorCode,
};
use tokio::{process::Command, sync::Mutex, time::timeout};

static NEXT_STAGE: AtomicU64 = AtomicU64::new(1);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
const MARKER: &str = ".dynamic-mcp-package.json";

#[derive(Debug)]
pub struct PackageInstaller {
    root: PathBuf,
    locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

impl PackageInstaller {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            locks: Mutex::new(BTreeMap::new()),
        }
    }

    #[must_use]
    pub fn binary_path(&self, server_id: &str, provision: &ProvisionConfig) -> Option<PathBuf> {
        let parent = checked_child(&self.root, server_id)?;
        let target = checked_child(&parent, &provision.version)?;
        let binary = checked_package_binary(&target, provision)?;

        let canonical_root = std::fs::canonicalize(&self.root).ok()?;
        let parent_metadata = std::fs::symlink_metadata(&parent).ok()?;
        if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
            return None;
        }
        let canonical_parent = std::fs::canonicalize(&parent).ok()?;
        if canonical_parent.parent() != Some(canonical_root.as_path()) {
            return None;
        }

        let target_metadata = std::fs::symlink_metadata(&target).ok()?;
        if target_metadata.file_type().is_symlink() || !target_metadata.is_dir() {
            return None;
        }
        let canonical_target = std::fs::canonicalize(&target).ok()?;
        if canonical_target.parent() != Some(canonical_parent.as_path()) {
            return None;
        }

        is_contained_file(&canonical_target, &binary).then_some(binary)
    }

    pub async fn install(
        &self,
        server_id: &str,
        provision: &ProvisionConfig,
    ) -> Result<PackageInstallResult, RuntimeError> {
        let lock = {
            let mut locks = self.locks.lock().await;
            Arc::clone(
                locks
                    .entry(server_id.to_owned())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = lock.lock().await;
        self.install_locked(server_id, provision).await
    }

    async fn install_locked(
        &self,
        server_id: &str,
        provision: &ProvisionConfig,
    ) -> Result<PackageInstallResult, RuntimeError> {
        let parent = checked_child(&self.root, server_id).ok_or_else(|| {
            package_error(
                server_id,
                "the package destination is outside the package root",
            )
        })?;
        let target = checked_child(&parent, &provision.version).ok_or_else(|| {
            package_error(server_id, "the package target is outside the server cache")
        })?;
        checked_package_binary(&target, provision).ok_or_else(|| {
            package_error(
                server_id,
                "the package binary is outside the package target",
            )
        })?;
        ensure_safe_parent(&self.root, &parent, server_id)?;
        let target_exists = match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(package_error(
                    server_id,
                    "the package target is not a safe managed directory",
                ));
            }
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => {
                return Err(package_error(
                    server_id,
                    "the package target could not be inspected",
                ));
            }
        };
        if target_exists {
            let marker = std::fs::read(target.join(MARKER)).map_err(|_| {
                package_error(
                    server_id,
                    "the package target is not managed by Dynamic MCP",
                )
            })?;
            let expected = serde_json::to_vec(provision).map_err(|_| {
                package_error(server_id, "the package definition could not be serialized")
            })?;
            let managed_binary = self.binary_path(server_id, provision);
            if marker != expected || managed_binary.is_none() {
                return Err(package_error(
                    server_id,
                    "the installed package does not match the manifest",
                ));
            }
            return Ok(package_result(
                server_id,
                provision,
                managed_binary.as_ref().expect("checked above"),
                false,
            ));
        }

        let stage_name = format!(
            ".{}.install.{}.{}",
            provision.version,
            std::process::id(),
            NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
        );
        let stage = checked_child(&parent, &stage_name).ok_or_else(|| {
            package_error(server_id, "the package stage is outside the server cache")
        })?;
        match std::fs::symlink_metadata(&stage) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                std::fs::remove_dir_all(&stage).map_err(|_| {
                    package_error(server_id, "a stale package stage could not be removed")
                })?;
            }
            Ok(_) => {
                return Err(package_error(
                    server_id,
                    "the package stage is not a safe managed directory",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(package_error(
                    server_id,
                    "the package stage could not be inspected",
                ));
            }
        }
        std::fs::create_dir(&stage)
            .map_err(|_| package_error(server_id, "the package stage could not be created"))?;

        let mut command = package_command(&stage, provision);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let status = timeout(INSTALL_TIMEOUT, command.status()).await;
        let installed = matches!(status, Ok(Ok(status)) if status.success());
        if !installed {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(package_error(server_id, "the package provider failed"));
        }
        let staged_binary = checked_package_binary(&stage, provision).ok_or_else(|| {
            package_error(server_id, "the staged binary is outside the package stage")
        })?;
        if !is_contained_file(&stage, &staged_binary) {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(package_error(
                server_id,
                "the package provider did not install the expected binary",
            ));
        }
        let marker = serde_json::to_vec(provision).map_err(|_| {
            package_error(server_id, "the package definition could not be serialized")
        })?;
        std::fs::write(stage.join(MARKER), marker)
            .map_err(|_| package_error(server_id, "the package marker could not be written"))?;
        std::fs::rename(&stage, &target).map_err(|_| {
            package_error(server_id, "the package could not be published atomically")
        })?;
        let managed_binary = self.binary_path(server_id, provision).ok_or_else(|| {
            package_error(
                server_id,
                "the installed package binary escapes the package target",
            )
        })?;
        Ok(package_result(server_id, provision, &managed_binary, true))
    }
}

fn checked_child(parent: &Path, component: &str) -> Option<PathBuf> {
    if !is_safe_portable_component(component) {
        return None;
    }
    let child = parent.join(component);
    (child.parent() == Some(parent)).then_some(child)
}

fn ensure_safe_parent(root: &Path, parent: &Path, server_id: &str) -> Result<(), RuntimeError> {
    std::fs::create_dir_all(root)
        .map_err(|_| package_error(server_id, "the package root could not be created"))?;
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|_| package_error(server_id, "the package root could not be inspected"))?;

    match std::fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(package_error(
                server_id,
                "the server package cache is not a safe directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(parent).map_err(|_| {
                package_error(server_id, "the server package cache could not be created")
            })?;
        }
        Err(_) => {
            return Err(package_error(
                server_id,
                "the server package cache could not be inspected",
            ));
        }
    }

    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|_| package_error(server_id, "the server package cache could not be inspected"))?;
    if canonical_parent.parent() != Some(canonical_root.as_path()) {
        return Err(package_error(
            server_id,
            "the server package cache is outside the package root",
        ));
    }
    Ok(())
}

fn checked_package_binary(root: &Path, provision: &ProvisionConfig) -> Option<PathBuf> {
    is_safe_portable_component(&provision.binary).then(|| package_binary(root, provision))
}

fn is_contained_file(root: &Path, candidate: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(candidate) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let (Ok(canonical_root), Ok(canonical_candidate)) = (
        std::fs::canonicalize(root),
        std::fs::canonicalize(candidate),
    ) else {
        return false;
    };
    canonical_candidate.starts_with(canonical_root)
}

fn is_safe_portable_component(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !matches!(value, "." | "..")
        && !value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
                )
        })
        && !value.ends_with('.')
        && !is_windows_reserved_name(value)
}

fn is_windows_reserved_name(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    let basic_device = stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL");
    let numbered_device = stem.get(..3).is_some_and(|prefix| {
        prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT")
    }) && stem.get(3..).is_some_and(|suffix| {
        matches!(
            suffix,
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
    });
    basic_device || numbered_device
}

fn package_command(stage: &Path, provision: &ProvisionConfig) -> Command {
    match provision.provider {
        PackageProvider::Npm => {
            let mut command = Command::new("npm");
            command.args(["install", "--no-audit", "--no-fund", "--prefix"]);
            command.arg(stage);
            if !provision.allow_scripts {
                command.arg("--ignore-scripts");
            }
            command.arg(format!("{}@{}", provision.package, provision.version));
            command
        }
        PackageProvider::Uv => {
            let mut command = Command::new("uv");
            command.args(["tool", "install", "--force"]);
            command.arg(format!("{}=={}", provision.package, provision.version));
            command.env("UV_TOOL_DIR", stage.join("tools"));
            command.env("UV_TOOL_BIN_DIR", stage.join("bin"));
            command
        }
        PackageProvider::Cargo => {
            let mut command = Command::new("cargo");
            command.args(["install", "--locked", "--version"]);
            command.arg(&provision.version);
            command.args(["--root"]);
            command.arg(stage);
            command.arg(&provision.package);
            command
        }
    }
}

fn package_binary(root: &Path, provision: &ProvisionConfig) -> PathBuf {
    #[allow(unused_mut)]
    let mut path = match provision.provider {
        PackageProvider::Npm => root.join("node_modules/.bin").join(&provision.binary),
        PackageProvider::Uv | PackageProvider::Cargo => root.join("bin").join(&provision.binary),
    };
    #[cfg(windows)]
    {
        let extension = match provision.provider {
            PackageProvider::Npm => "cmd",
            PackageProvider::Uv | PackageProvider::Cargo => "exe",
        };
        path.set_extension(extension);
    }
    path
}

fn package_result(
    server_id: &str,
    provision: &ProvisionConfig,
    binary: &Path,
    installed: bool,
) -> PackageInstallResult {
    PackageInstallResult {
        server_id: server_id.to_owned(),
        provider: provision.provider.as_str().to_owned(),
        package: provision.package.clone(),
        version: provision.version.clone(),
        binary_path: binary.display().to_string(),
        installed,
    }
}

fn package_error(server_id: &str, message: &'static str) -> RuntimeError {
    RuntimeError::for_server(
        RuntimeErrorCode::PackageInstallFailed,
        "package_install",
        server_id,
        message,
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use mcp_host_core::{PackageProvider, ProvisionConfig};
    use tempfile::tempdir;

    use super::{package_binary, package_command};

    #[test]
    fn provider_commands_are_shell_free_and_version_pinned() {
        let root = tempdir().expect("temporary package root");
        for (provider, executable, separator) in [
            (PackageProvider::Npm, "npm", "@"),
            (PackageProvider::Uv, "uv", "=="),
            (PackageProvider::Cargo, "cargo", ""),
        ] {
            let provision = ProvisionConfig {
                provider,
                package: "example-package".to_owned(),
                version: "1.2.3".to_owned(),
                binary: "example".to_owned(),
                allow_scripts: false,
            };
            let command = package_command(root.path(), &provision);
            assert_eq!(command.as_std().get_program(), executable);
            let arguments = command
                .as_std()
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert!(arguments.iter().any(|argument| argument.contains("1.2.3")));
            if !separator.is_empty() {
                assert!(
                    arguments
                        .iter()
                        .any(|argument| argument.contains(separator))
                );
            }
            assert!(package_binary(root.path(), &provision).starts_with(root.path()));
        }
    }

    #[tokio::test]
    async fn exact_managed_install_is_idempotent_without_provider_execution() {
        let root = tempdir().expect("temporary package root");
        let provision = ProvisionConfig {
            provider: PackageProvider::Cargo,
            package: "example-package".to_owned(),
            version: "1.2.3".to_owned(),
            binary: "example".to_owned(),
            allow_scripts: false,
        };
        let installer = super::PackageInstaller::new(root.path().to_owned());
        let target = root.path().join("server/1.2.3");
        let binary = super::package_binary(&target, &provision);
        fs::create_dir_all(binary.parent().expect("binary parent"))
            .expect("managed package directory");
        fs::write(&binary, "fixture").expect("managed binary");
        fs::write(
            target.join(super::MARKER),
            serde_json::to_vec(&provision).expect("marker JSON"),
        )
        .expect("managed marker");

        let result = installer
            .install("server", &provision)
            .await
            .expect("exact install should be reused");
        assert!(!result.installed);
        assert_eq!(result.binary_path, binary.display().to_string());
    }

    #[tokio::test]
    async fn installer_rejects_unsafe_versions_without_touching_their_targets() {
        let sandbox = tempdir().expect("temporary sandbox");
        let root = sandbox.path().join("packages");
        let absolute_version = sandbox.path().join("absolute-victim");
        let absolute_version = absolute_version.to_str().expect("UTF-8 temporary path");

        for version in [
            "../victim",
            "../../tmp/victim",
            "..\\victim",
            absolute_version,
            "C:\\tmp\\victim",
        ] {
            let provision = provision(version);
            let target = root.join("server").join(version);
            create_managed_target(&target, &provision);
            let sentinel = target.join("sentinel");
            fs::write(&sentinel, "do not touch").expect("victim sentinel");

            let installer = super::PackageInstaller::new(root.clone());
            installer
                .install("server", &provision)
                .await
                .expect_err("unsafe version must fail before provider execution");

            assert_eq!(
                fs::read_to_string(&sentinel).expect("victim must remain"),
                "do not touch"
            );
        }
    }

    #[tokio::test]
    async fn installer_rejects_server_id_traversal_without_touching_outside_root() {
        let sandbox = tempdir().expect("temporary sandbox");
        let root = sandbox.path().join("packages");
        let provision = provision("1.2.3");
        let target = root.join("../outside/1.2.3");
        create_managed_target(&target, &provision);
        let sentinel = target.join("sentinel");
        fs::write(&sentinel, "do not touch").expect("outside sentinel");

        let installer = super::PackageInstaller::new(root);
        installer
            .install("../outside", &provision)
            .await
            .expect_err("server ID traversal must fail before provider execution");

        assert_eq!(
            fs::read_to_string(sentinel).expect("outside victim must remain"),
            "do not touch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn binary_path_allows_internal_symlinks_and_rejects_external_symlinks() {
        use std::os::unix::fs::symlink;

        let sandbox = tempdir().expect("temporary sandbox");
        let root = sandbox.path().join("packages");
        let provision = ProvisionConfig {
            provider: PackageProvider::Npm,
            package: "example-package".to_owned(),
            version: "1.2.3".to_owned(),
            binary: "example".to_owned(),
            allow_scripts: false,
        };
        let target = root.join("server/1.2.3");
        let binary = super::package_binary(&target, &provision);
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("npm bin directory");
        let package_binary = target.join("node_modules/example-package/cli.js");
        fs::create_dir_all(package_binary.parent().expect("package binary parent"))
            .expect("npm package directory");
        fs::write(&package_binary, "fixture").expect("npm package binary");
        symlink("../example-package/cli.js", &binary).expect("npm-style internal symlink");

        let installer = super::PackageInstaller::new(root.clone());
        assert_eq!(
            installer.binary_path("server", &provision),
            Some(binary.clone())
        );

        fs::remove_file(&binary).expect("remove internal symlink");
        let outside_binary = sandbox.path().join("outside-binary");
        fs::write(&outside_binary, "outside").expect("outside binary");
        symlink(&outside_binary, &binary).expect("external symlink");
        assert_eq!(installer.binary_path("server", &provision), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn installer_rejects_symlinked_server_cache_without_touching_outside_root() {
        use std::os::unix::fs::symlink;

        let sandbox = tempdir().expect("temporary sandbox");
        let root = sandbox.path().join("packages");
        let outside = sandbox.path().join("outside");
        let provision = provision("1.2.3");
        let target = outside.join("1.2.3");
        create_managed_target(&target, &provision);
        let sentinel = target.join("sentinel");
        fs::write(&sentinel, "do not touch").expect("outside sentinel");
        fs::create_dir(&root).expect("package root");
        symlink(&outside, root.join("server")).expect("symlinked server cache");

        let installer = super::PackageInstaller::new(root);
        assert_eq!(installer.binary_path("server", &provision), None);
        installer
            .install("server", &provision)
            .await
            .expect_err("symlinked server cache must be rejected");

        assert_eq!(
            fs::read_to_string(sentinel).expect("outside victim must remain"),
            "do not touch"
        );
    }

    fn provision(version: &str) -> ProvisionConfig {
        ProvisionConfig {
            provider: PackageProvider::Cargo,
            package: "example-package".to_owned(),
            version: version.to_owned(),
            binary: "example".to_owned(),
            allow_scripts: false,
        }
    }

    fn create_managed_target(target: &Path, provision: &ProvisionConfig) {
        let binary = super::package_binary(target, provision);
        fs::create_dir_all(binary.parent().expect("binary parent"))
            .expect("managed package directory");
        fs::write(binary, "fixture").expect("managed binary");
        fs::write(
            target.join(super::MARKER),
            serde_json::to_vec(provision).expect("marker JSON"),
        )
        .expect("managed marker");
    }
}
