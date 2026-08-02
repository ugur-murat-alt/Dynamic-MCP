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
    pub fn binary_path(&self, server_id: &str, provision: &ProvisionConfig) -> PathBuf {
        package_binary(
            &self.root.join(server_id).join(&provision.version),
            provision,
        )
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
        let parent = self.root.join(server_id);
        let target = parent.join(&provision.version);
        let binary = package_binary(&target, provision);
        if target.exists() {
            let marker = std::fs::read(target.join(MARKER)).map_err(|_| {
                package_error(
                    server_id,
                    "the package target is not managed by Dynamic MCP",
                )
            })?;
            let expected = serde_json::to_vec(provision).map_err(|_| {
                package_error(server_id, "the package definition could not be serialized")
            })?;
            if marker != expected || !binary.is_file() {
                return Err(package_error(
                    server_id,
                    "the installed package does not match the manifest",
                ));
            }
            return Ok(package_result(server_id, provision, &binary, false));
        }

        std::fs::create_dir_all(&parent)
            .map_err(|_| package_error(server_id, "the package cache could not be created"))?;
        let stage = parent.join(format!(
            ".{}.install.{}.{}",
            provision.version,
            std::process::id(),
            NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
        ));
        if stage.exists() {
            std::fs::remove_dir_all(&stage).map_err(|_| {
                package_error(server_id, "a stale package stage could not be removed")
            })?;
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
        let staged_binary = package_binary(&stage, provision);
        if !staged_binary.is_file() {
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
        Ok(package_result(server_id, provision, &binary, true))
    }
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
    use std::fs;

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
}
