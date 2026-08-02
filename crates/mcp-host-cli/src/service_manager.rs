use std::{ffi::OsString, io, process::Command};

use crate::service::{
    ServiceArtifact, ServiceDescriptor, ServiceManagerKind, ServiceScope, ServiceStatus,
    inspect_descriptor, install_descriptor, remove_descriptor, validate_descriptor,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceRunState {
    Running,
    Stopped,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceReport {
    pub artifact: ServiceStatus,
    pub loaded: bool,
    pub enabled: bool,
    pub run_state: ServiceRunState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceInstallResult {
    pub artifact_updated: bool,
    pub enabled: bool,
    pub started: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceUninstallResult {
    pub artifact_removed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceOperationErrorKind {
    InvalidArguments,
    Foreign,
    PermissionDenied,
    ManagerUnavailable,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceOperationError {
    pub kind: ServiceOperationErrorKind,
    pub message: &'static str,
}

pub trait ServiceCommandRunner {
    fn run(&self, program: &str, arguments: &[OsString]) -> io::Result<ServiceCommandOutput>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceCommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct SystemServiceCommandRunner;

impl ServiceCommandRunner for SystemServiceCommandRunner {
    fn run(&self, program: &str, arguments: &[OsString]) -> io::Result<ServiceCommandOutput> {
        let output = Command::new(program).args(arguments).output()?;
        Ok(ServiceCommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

pub fn install_service(
    descriptor: &ServiceDescriptor,
    no_start: bool,
) -> Result<ServiceInstallResult, ServiceOperationError> {
    install_service_with_runner(descriptor, no_start, &SystemServiceCommandRunner)
}

pub fn uninstall_service(
    descriptor: &ServiceDescriptor,
) -> Result<ServiceUninstallResult, ServiceOperationError> {
    uninstall_service_with_runner(descriptor, &SystemServiceCommandRunner)
}

pub fn service_status(
    descriptor: &ServiceDescriptor,
) -> Result<ServiceReport, ServiceOperationError> {
    service_status_with_runner(descriptor, &SystemServiceCommandRunner)
}

pub fn install_service_with_runner(
    descriptor: &ServiceDescriptor,
    no_start: bool,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceInstallResult, ServiceOperationError> {
    validate_descriptor(descriptor).map_err(|_| invalid_error())?;
    match descriptor.kind {
        ServiceManagerKind::Systemd => install_systemd(descriptor, no_start, runner),
        ServiceManagerKind::Launchd => install_launchd(descriptor, no_start, runner),
        ServiceManagerKind::WindowsScm => install_windows(descriptor, no_start),
    }
}

pub fn uninstall_service_with_runner(
    descriptor: &ServiceDescriptor,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceUninstallResult, ServiceOperationError> {
    match descriptor.kind {
        ServiceManagerKind::Systemd => uninstall_systemd(descriptor, runner),
        ServiceManagerKind::Launchd => uninstall_launchd(descriptor, runner),
        ServiceManagerKind::WindowsScm => uninstall_windows(descriptor),
    }
}

pub fn service_status_with_runner(
    descriptor: &ServiceDescriptor,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceReport, ServiceOperationError> {
    match descriptor.kind {
        ServiceManagerKind::Systemd => status_systemd(descriptor, runner),
        ServiceManagerKind::Launchd => status_launchd(descriptor, runner),
        ServiceManagerKind::WindowsScm => status_windows(descriptor),
    }
}

fn install_systemd(
    descriptor: &ServiceDescriptor,
    no_start: bool,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceInstallResult, ServiceOperationError> {
    let before = artifact_status(descriptor)?;
    reject_foreign(&before)?;
    let runtime_before = query_systemd(descriptor, runner)?;
    if before == ServiceStatus::NotInstalled && runtime_before.loaded {
        return Err(foreign_error());
    }
    let artifact_updated = install_descriptor(descriptor).map_err(artifact_error)?;
    run_required(
        runner,
        "systemctl",
        systemd_args(descriptor.scope, ["daemon-reload"]),
    )?;
    let unit = systemd_unit(descriptor);
    run_required(
        runner,
        "systemctl",
        systemd_args(descriptor.scope, ["enable", unit.as_str()]),
    )?;

    let mut started = false;
    if !no_start {
        match runtime_before {
            ServiceReport {
                run_state: ServiceRunState::Running,
                ..
            } => {
                run_required(
                    runner,
                    "systemctl",
                    systemd_args(descriptor.scope, ["restart", unit.as_str()]),
                )?;
                started = true;
            }
            _ => {
                run_required(
                    runner,
                    "systemctl",
                    systemd_args(descriptor.scope, ["start", unit.as_str()]),
                )?;
                started = true;
            }
        }
    }
    Ok(ServiceInstallResult {
        artifact_updated,
        enabled: true,
        started,
    })
}

fn uninstall_systemd(
    descriptor: &ServiceDescriptor,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceUninstallResult, ServiceOperationError> {
    let artifact = artifact_status(descriptor)?;
    reject_foreign(&artifact)?;
    if artifact == ServiceStatus::NotInstalled {
        return Ok(ServiceUninstallResult {
            artifact_removed: false,
        });
    }
    let unit = systemd_unit(descriptor);
    let output = run_command(
        runner,
        "systemctl",
        systemd_args(descriptor.scope, ["disable", "--now", unit.as_str()]),
    )?;
    if !output.success {
        let status = query_systemd(descriptor, runner)?;
        if status.enabled || status.run_state == ServiceRunState::Running {
            return Err(command_error("systemctl", &output));
        }
    }
    let artifact_removed = remove_descriptor(descriptor).map_err(artifact_error)?;
    run_required(
        runner,
        "systemctl",
        systemd_args(descriptor.scope, ["daemon-reload"]),
    )?;
    Ok(ServiceUninstallResult { artifact_removed })
}

fn status_systemd(
    descriptor: &ServiceDescriptor,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceReport, ServiceOperationError> {
    let mut report = query_systemd(descriptor, runner)?;
    report.artifact = artifact_status(descriptor)?;
    Ok(report)
}

fn query_systemd(
    descriptor: &ServiceDescriptor,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceReport, ServiceOperationError> {
    let unit = systemd_unit(descriptor);
    let output = run_required(
        runner,
        "systemctl",
        systemd_args(
            descriptor.scope,
            [
                "show",
                unit.as_str(),
                "--property=LoadState",
                "--property=UnitFileState",
                "--property=ActiveState",
            ],
        ),
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut load_state = None;
    let mut unit_state = None;
    let mut active_state = None;
    for line in stdout.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        match name {
            "LoadState" => load_state = Some(value),
            "UnitFileState" => unit_state = Some(value),
            "ActiveState" => active_state = Some(value),
            _ => {}
        }
    }
    let load_state = load_state.unwrap_or_default();
    let unit_state = unit_state.unwrap_or_default();
    let active_state = active_state.unwrap_or_default();
    if load_state.is_empty() || active_state.is_empty() {
        return Err(operation_error(
            "service manager returned an invalid status response",
        ));
    }
    let enabled = matches!(
        unit_state,
        "enabled" | "enabled-runtime" | "linked" | "linked-runtime"
    );
    let run_state = match active_state {
        "active" | "activating" | "reloading" => ServiceRunState::Running,
        "inactive" | "deactivating" => ServiceRunState::Stopped,
        "failed" => ServiceRunState::Failed,
        _ if load_state == "not-found" => ServiceRunState::Stopped,
        _ => ServiceRunState::Unknown,
    };
    Ok(ServiceReport {
        artifact: ServiceStatus::NotInstalled,
        loaded: load_state != "not-found",
        enabled,
        run_state,
    })
}

fn install_launchd(
    descriptor: &ServiceDescriptor,
    no_start: bool,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceInstallResult, ServiceOperationError> {
    let before = artifact_status(descriptor)?;
    reject_foreign(&before)?;
    let runtime_before = query_launchd(descriptor, runner)?;
    if before == ServiceStatus::NotInstalled && runtime_before.enabled {
        return Err(foreign_error());
    }
    let artifact_updated = install_descriptor(descriptor).map_err(artifact_error)?;
    if no_start {
        return Ok(ServiceInstallResult {
            artifact_updated,
            enabled: runtime_before.enabled,
            started: false,
        });
    }

    let domain = launchd_domain(descriptor)?;
    let target = launchd_target(descriptor, &domain);
    if runtime_before.enabled {
        run_required(runner, "launchctl", os_args(["bootout", target.as_str()]))?;
    }
    let path = descriptor_path(descriptor)?;
    run_required(
        runner,
        "launchctl",
        vec![
            OsString::from("bootstrap"),
            OsString::from(&domain),
            path.as_os_str().to_owned(),
        ],
    )?;
    Ok(ServiceInstallResult {
        artifact_updated,
        enabled: true,
        started: true,
    })
}

fn uninstall_launchd(
    descriptor: &ServiceDescriptor,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceUninstallResult, ServiceOperationError> {
    let artifact = artifact_status(descriptor)?;
    reject_foreign(&artifact)?;
    if artifact == ServiceStatus::NotInstalled {
        return Ok(ServiceUninstallResult {
            artifact_removed: false,
        });
    }
    let status = query_launchd(descriptor, runner)?;
    if status.enabled {
        let domain = launchd_domain(descriptor)?;
        let target = launchd_target(descriptor, &domain);
        run_required(runner, "launchctl", os_args(["bootout", target.as_str()]))?;
    }
    let artifact_removed = remove_descriptor(descriptor).map_err(artifact_error)?;
    Ok(ServiceUninstallResult { artifact_removed })
}

fn status_launchd(
    descriptor: &ServiceDescriptor,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceReport, ServiceOperationError> {
    let mut report = query_launchd(descriptor, runner)?;
    report.artifact = artifact_status(descriptor)?;
    Ok(report)
}

fn query_launchd(
    descriptor: &ServiceDescriptor,
    runner: &impl ServiceCommandRunner,
) -> Result<ServiceReport, ServiceOperationError> {
    let domain = launchd_domain(descriptor)?;
    let target = launchd_target(descriptor, &domain);
    let output = run_command(runner, "launchctl", os_args(["print", target.as_str()]))?;
    if !output.success {
        if output_contains(
            &output,
            &["could not find service", "service not found", "not found"],
        ) {
            return Ok(ServiceReport {
                artifact: ServiceStatus::NotInstalled,
                loaded: false,
                enabled: false,
                run_state: ServiceRunState::Stopped,
            });
        }
        return Err(command_error("launchctl", &output));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let run_state = launchd_run_state(&stdout);
    Ok(ServiceReport {
        artifact: ServiceStatus::NotInstalled,
        loaded: true,
        enabled: true,
        run_state,
    })
}

fn launchd_run_state(output: &str) -> ServiceRunState {
    let output = output.to_ascii_lowercase();
    if output.contains("state = running") {
        ServiceRunState::Running
    } else if output.contains("state = waiting") {
        ServiceRunState::Stopped
    } else if output.contains("state = exited") {
        if launchd_last_exit_code(&output).is_some_and(|code| code != 0) {
            ServiceRunState::Failed
        } else {
            ServiceRunState::Stopped
        }
    } else {
        ServiceRunState::Unknown
    }
}

fn launchd_last_exit_code(output: &str) -> Option<i32> {
    output.lines().find_map(|line| {
        let (_, value) = line.split_once("last exit code =")?;
        value
            .split_ascii_whitespace()
            .next()?
            .trim_end_matches(':')
            .parse::<i32>()
            .ok()
    })
}

fn systemd_args<const N: usize>(scope: ServiceScope, tail: [&str; N]) -> Vec<OsString> {
    let mut arguments = Vec::with_capacity(N + usize::from(scope == ServiceScope::User));
    if scope == ServiceScope::User {
        arguments.push(OsString::from("--user"));
    }
    arguments.extend(tail.into_iter().map(OsString::from));
    arguments
}

fn os_args<const N: usize>(arguments: [&str; N]) -> Vec<OsString> {
    arguments.into_iter().map(OsString::from).collect()
}

fn systemd_unit(descriptor: &ServiceDescriptor) -> String {
    format!("{}.service", descriptor.name)
}

fn launchd_domain(descriptor: &ServiceDescriptor) -> Result<String, ServiceOperationError> {
    match descriptor.scope {
        ServiceScope::System => Ok("system".to_owned()),
        ServiceScope::User => {
            #[cfg(unix)]
            {
                Ok(format!("gui/{}", rustix::process::getuid().as_raw()))
            }
            #[cfg(not(unix))]
            {
                Err(manager_unavailable("launchd user domains require Unix"))
            }
        }
    }
}

fn launchd_target(descriptor: &ServiceDescriptor, domain: &str) -> String {
    format!("{domain}/dev.dynamic-mcp.{}", descriptor.name)
}

fn descriptor_path(
    descriptor: &ServiceDescriptor,
) -> Result<&std::path::Path, ServiceOperationError> {
    match &descriptor.artifact {
        ServiceArtifact::File { path, .. } => Ok(path),
        ServiceArtifact::WindowsScm(_) => {
            Err(operation_error("service descriptor path is unavailable"))
        }
    }
}

fn artifact_status(descriptor: &ServiceDescriptor) -> Result<ServiceStatus, ServiceOperationError> {
    inspect_descriptor(descriptor).map_err(artifact_error)
}

fn reject_foreign(status: &ServiceStatus) -> Result<(), ServiceOperationError> {
    if *status == ServiceStatus::Foreign {
        Err(foreign_error())
    } else {
        Ok(())
    }
}

fn run_required(
    runner: &impl ServiceCommandRunner,
    program: &str,
    arguments: Vec<OsString>,
) -> Result<ServiceCommandOutput, ServiceOperationError> {
    let output = run_command(runner, program, arguments)?;
    if output.success {
        Ok(output)
    } else {
        Err(command_error(program, &output))
    }
}

fn run_command(
    runner: &impl ServiceCommandRunner,
    program: &str,
    arguments: Vec<OsString>,
) -> Result<ServiceCommandOutput, ServiceOperationError> {
    runner
        .run(program, &arguments)
        .map_err(|error| io_error(program, &error))
}

fn output_contains(output: &ServiceCommandOutput, needles: &[&str]) -> bool {
    let mut text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    text.push_str(&String::from_utf8_lossy(&output.stderr).to_ascii_lowercase());
    needles.iter().any(|needle| text.contains(needle))
}

fn io_error(program: &str, error: &io::Error) -> ServiceOperationError {
    match error.kind() {
        io::ErrorKind::NotFound => manager_unavailable(if program == "systemctl" {
            "systemctl is unavailable"
        } else {
            "launchctl is unavailable"
        }),
        io::ErrorKind::PermissionDenied => permission_error(),
        _ => operation_error("service manager command could not be executed"),
    }
}

fn command_error(program: &str, output: &ServiceCommandOutput) -> ServiceOperationError {
    if output_contains(
        output,
        &[
            "permission denied",
            "access denied",
            "authentication is required",
            "not authorized",
        ],
    ) {
        return permission_error();
    }
    if output_contains(
        output,
        &[
            "failed to connect to bus",
            "not been booted with systemd",
            "could not find domain",
        ],
    ) {
        return manager_unavailable(if program == "systemctl" {
            "systemd service manager is unavailable"
        } else {
            "launchd service manager is unavailable"
        });
    }
    operation_error("service manager command failed")
}

fn artifact_error(message: String) -> ServiceOperationError {
    let lower = message.to_ascii_lowercase();
    if lower.contains("not managed") {
        foreign_error()
    } else if lower.contains("permission denied") || lower.contains("access is denied") {
        permission_error()
    } else {
        operation_error("service artifact operation failed")
    }
}

pub(crate) const fn foreign_error() -> ServiceOperationError {
    ServiceOperationError {
        kind: ServiceOperationErrorKind::Foreign,
        message: "the existing service is not managed by Dynamic MCP",
    }
}

const fn invalid_error() -> ServiceOperationError {
    ServiceOperationError {
        kind: ServiceOperationErrorKind::InvalidArguments,
        message: "the service installation configuration is invalid",
    }
}

pub(crate) const fn permission_error() -> ServiceOperationError {
    ServiceOperationError {
        kind: ServiceOperationErrorKind::PermissionDenied,
        message: "the service operation requires additional permission",
    }
}

pub(crate) const fn manager_unavailable(message: &'static str) -> ServiceOperationError {
    ServiceOperationError {
        kind: ServiceOperationErrorKind::ManagerUnavailable,
        message,
    }
}

pub(crate) const fn operation_error(message: &'static str) -> ServiceOperationError {
    ServiceOperationError {
        kind: ServiceOperationErrorKind::Failed,
        message,
    }
}

#[cfg(windows)]
fn install_windows(
    descriptor: &ServiceDescriptor,
    no_start: bool,
) -> Result<ServiceInstallResult, ServiceOperationError> {
    crate::windows_service_backend::install(descriptor, no_start)
}

#[cfg(not(windows))]
fn install_windows(
    _descriptor: &ServiceDescriptor,
    _no_start: bool,
) -> Result<ServiceInstallResult, ServiceOperationError> {
    Err(manager_unavailable(
        "Windows SCM is unavailable on this platform",
    ))
}

#[cfg(windows)]
fn uninstall_windows(
    descriptor: &ServiceDescriptor,
) -> Result<ServiceUninstallResult, ServiceOperationError> {
    crate::windows_service_backend::uninstall(descriptor)
}

#[cfg(not(windows))]
fn uninstall_windows(
    _descriptor: &ServiceDescriptor,
) -> Result<ServiceUninstallResult, ServiceOperationError> {
    Err(manager_unavailable(
        "Windows SCM is unavailable on this platform",
    ))
}

#[cfg(windows)]
fn status_windows(descriptor: &ServiceDescriptor) -> Result<ServiceReport, ServiceOperationError> {
    crate::windows_service_backend::status(descriptor)
}

#[cfg(not(windows))]
fn status_windows(_descriptor: &ServiceDescriptor) -> Result<ServiceReport, ServiceOperationError> {
    Err(manager_unavailable(
        "Windows SCM is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, ffi::OsString, fs, io, path::Path, sync::Mutex};

    use tempfile::tempdir;

    use crate::service::{
        ServiceInstallOptions, ServiceManagerKind, ServiceScope, render_descriptor,
    };

    use super::{
        ServiceCommandOutput, ServiceCommandRunner, ServiceOperationErrorKind, ServiceRunState,
        install_service_with_runner, service_status_with_runner, uninstall_service_with_runner,
    };

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<Vec<String>>>,
        outputs: Mutex<VecDeque<ServiceCommandOutput>>,
    }

    impl FakeRunner {
        fn with_outputs(outputs: Vec<ServiceCommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs.into()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    impl ServiceCommandRunner for FakeRunner {
        fn run(&self, program: &str, arguments: &[OsString]) -> io::Result<ServiceCommandOutput> {
            let mut call = vec![program.to_owned()];
            call.extend(
                arguments
                    .iter()
                    .map(|value| value.to_string_lossy().into_owned()),
            );
            self.calls.lock().expect("calls lock").push(call);
            self.outputs
                .lock()
                .expect("outputs lock")
                .pop_front()
                .ok_or_else(|| io::Error::other("missing fake output"))
        }
    }

    #[test]
    fn systemd_install_enables_starts_and_no_start_only_enables() {
        let root = tempdir().expect("temporary directory");
        let user_descriptor =
            descriptor(root.path(), ServiceManagerKind::Systemd, ServiceScope::User);
        let runner = FakeRunner::with_outputs(vec![systemd_not_found(), ok(""), ok(""), ok("")]);
        let result =
            install_service_with_runner(&user_descriptor, false, &runner).expect("install");
        assert!(result.artifact_updated);
        assert!(result.started);
        assert_eq!(
            runner.calls(),
            [
                systemd_user_show(),
                strings(&["systemctl", "--user", "daemon-reload"]),
                strings(&["systemctl", "--user", "enable", "dynamic-mcp.service"]),
                strings(&["systemctl", "--user", "start", "dynamic-mcp.service"]),
            ]
        );

        let second = tempdir().expect("temporary directory");
        let system_descriptor = descriptor(
            second.path(),
            ServiceManagerKind::Systemd,
            ServiceScope::System,
        );
        let runner = FakeRunner::with_outputs(vec![systemd_not_found(), ok(""), ok("")]);
        let result =
            install_service_with_runner(&system_descriptor, true, &runner).expect("install");
        assert!(!result.started);
        assert_eq!(
            runner.calls(),
            [
                systemd_system_show(),
                strings(&["systemctl", "daemon-reload"]),
                strings(&["systemctl", "enable", "dynamic-mcp.service"]),
            ]
        );
    }

    #[test]
    fn systemd_status_and_uninstall_preserve_manager_order() {
        let root = tempdir().expect("temporary directory");
        let descriptor = descriptor(root.path(), ServiceManagerKind::Systemd, ServiceScope::User);
        let install_runner = FakeRunner::with_outputs(vec![systemd_not_found(), ok(""), ok("")]);
        install_service_with_runner(&descriptor, true, &install_runner).expect("install");

        let status_runner = FakeRunner::with_outputs(vec![ok(
            "LoadState=loaded\nUnitFileState=enabled\nActiveState=active\n",
        )]);
        let report = service_status_with_runner(&descriptor, &status_runner).expect("status");
        assert!(report.enabled);
        assert_eq!(report.run_state, ServiceRunState::Running);

        let remove_runner = FakeRunner::with_outputs(vec![ok(""), ok("")]);
        let result = uninstall_service_with_runner(&descriptor, &remove_runner).expect("uninstall");
        assert!(result.artifact_removed);
        assert_eq!(
            remove_runner.calls(),
            [
                strings(&[
                    "systemctl",
                    "--user",
                    "disable",
                    "--now",
                    "dynamic-mcp.service",
                ]),
                strings(&["systemctl", "--user", "daemon-reload"]),
            ]
        );
    }

    #[test]
    fn launchd_install_bootstraps_and_uninstall_boots_out() {
        let root = tempdir().expect("temporary directory");
        let descriptor = descriptor(
            root.path(),
            ServiceManagerKind::Launchd,
            ServiceScope::System,
        );
        let runner = FakeRunner::with_outputs(vec![not_found(), ok("")]);
        let result = install_service_with_runner(&descriptor, false, &runner).expect("install");
        assert!(result.started);
        assert_eq!(
            runner.calls()[0],
            strings(&["launchctl", "print", "system/dev.dynamic-mcp.dynamic-mcp"])
        );
        assert_eq!(
            runner.calls()[1][0..3],
            ["launchctl", "bootstrap", "system"]
        );

        let remove_runner = FakeRunner::with_outputs(vec![ok("state = running\n"), ok("")]);
        let result = uninstall_service_with_runner(&descriptor, &remove_runner).expect("uninstall");
        assert!(result.artifact_removed);
        assert_eq!(
            remove_runner.calls(),
            [
                strings(&["launchctl", "print", "system/dev.dynamic-mcp.dynamic-mcp"]),
                strings(&["launchctl", "bootout", "system/dev.dynamic-mcp.dynamic-mcp"]),
            ]
        );
    }

    #[test]
    fn launchd_status_distinguishes_waiting_clean_exit_and_failure() {
        let root = tempdir().expect("temporary directory");
        let descriptor = descriptor(
            root.path(),
            ServiceManagerKind::Launchd,
            ServiceScope::System,
        );
        for (output, expected) in [
            (
                "state = waiting\nlast exit code = 78: EX_CONFIG\n",
                ServiceRunState::Stopped,
            ),
            (
                "state = exited\nlast exit code = 0\n",
                ServiceRunState::Stopped,
            ),
            (
                "state = exited\nlast exit code = 78: EX_CONFIG\n",
                ServiceRunState::Failed,
            ),
        ] {
            let runner = FakeRunner::with_outputs(vec![ok(output)]);
            let report = service_status_with_runner(&descriptor, &runner).expect("status");
            assert_eq!(report.run_state, expected);
        }
    }

    #[test]
    fn reinstall_restarts_active_systemd_and_launchd_no_start_only_writes() {
        let root = tempdir().expect("temporary directory");
        let systemd_descriptor =
            descriptor(root.path(), ServiceManagerKind::Systemd, ServiceScope::User);
        let initial = FakeRunner::with_outputs(vec![systemd_not_found(), ok(""), ok("")]);
        install_service_with_runner(&systemd_descriptor, true, &initial).expect("initial install");
        let reinstall = FakeRunner::with_outputs(vec![
            ok("LoadState=loaded\nUnitFileState=enabled\nActiveState=active\n"),
            ok(""),
            ok(""),
            ok(""),
        ]);
        let result =
            install_service_with_runner(&systemd_descriptor, false, &reinstall).expect("reinstall");
        assert!(result.started);
        assert_eq!(
            reinstall.calls().last(),
            Some(&strings(&[
                "systemctl",
                "--user",
                "restart",
                "dynamic-mcp.service",
            ]))
        );

        let launchd_root = tempdir().expect("temporary directory");
        let launchd = descriptor(
            launchd_root.path(),
            ServiceManagerKind::Launchd,
            ServiceScope::System,
        );
        let runner = FakeRunner::with_outputs(vec![not_found()]);
        let result = install_service_with_runner(&launchd, true, &runner).expect("no-start");
        assert!(!result.started);
        assert!(!result.enabled);
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn foreign_artifacts_are_rejected_before_manager_invocation() {
        let root = tempdir().expect("temporary directory");
        let descriptor = descriptor(root.path(), ServiceManagerKind::Systemd, ServiceScope::User);
        let crate::service::ServiceArtifact::File { path, .. } = &descriptor.artifact else {
            panic!("expected file descriptor");
        };
        fs::create_dir_all(path.parent().expect("descriptor parent")).expect("parent");
        fs::write(path, "foreign").expect("foreign artifact");
        let runner = FakeRunner::default();

        let error = install_service_with_runner(&descriptor, false, &runner)
            .expect_err("foreign install should fail");
        assert_eq!(error.kind, ServiceOperationErrorKind::Foreign);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn missing_service_manager_has_a_stable_error_kind() {
        struct MissingRunner;
        impl ServiceCommandRunner for MissingRunner {
            fn run(
                &self,
                _program: &str,
                _arguments: &[OsString],
            ) -> io::Result<ServiceCommandOutput> {
                Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
            }
        }

        let root = tempdir().expect("temporary directory");
        let descriptor = descriptor(root.path(), ServiceManagerKind::Systemd, ServiceScope::User);
        let error = service_status_with_runner(&descriptor, &MissingRunner)
            .expect_err("missing manager should fail");
        assert_eq!(error.kind, ServiceOperationErrorKind::ManagerUnavailable);
    }

    fn descriptor(
        root: &Path,
        kind: ServiceManagerKind,
        scope: ServiceScope,
    ) -> crate::service::ServiceDescriptor {
        let executable = root.join("mcp-host");
        fs::write(&executable, "binary").expect("binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .expect("permissions");
        }
        let config_dir = root.join("config");
        let runtime_dir = root.join("runtime");
        fs::create_dir(&config_dir).expect("config directory");
        fs::create_dir(&runtime_dir).expect("runtime directory");
        render_descriptor(
            kind,
            &ServiceInstallOptions {
                scope,
                name: "dynamic-mcp".to_owned(),
                description: "Dynamic MCP Host".to_owned(),
                executable,
                config_dir,
                runtime_dir,
                descriptor_dir: root.join("descriptors"),
            },
        )
        .expect("descriptor")
    }

    fn ok(stdout: &str) -> ServiceCommandOutput {
        ServiceCommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn not_found() -> ServiceCommandOutput {
        ServiceCommandOutput {
            success: false,
            code: Some(113),
            stdout: Vec::new(),
            stderr: b"Could not find service".to_vec(),
        }
    }

    fn systemd_not_found() -> ServiceCommandOutput {
        ok("LoadState=not-found\nUnitFileState=\nActiveState=inactive\n")
    }

    fn systemd_user_show() -> Vec<String> {
        let mut call = strings(&["systemctl", "--user"]);
        call.extend(systemd_show_tail());
        call
    }

    fn systemd_system_show() -> Vec<String> {
        let mut call = strings(&["systemctl"]);
        call.extend(systemd_show_tail());
        call
    }

    fn systemd_show_tail() -> Vec<String> {
        strings(&[
            "show",
            "dynamic-mcp.service",
            "--property=LoadState",
            "--property=UnitFileState",
            "--property=ActiveState",
        ])
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }
}
