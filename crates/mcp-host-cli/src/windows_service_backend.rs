use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;
use windows_service::{
    Error as WindowsServiceError, define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus as WindowsServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

use crate::{
    DaemonOptions,
    daemon::run_daemon_with_ready,
    service::{ServiceArtifact, ServiceDescriptor, ServiceStatus, WindowsServiceSpec},
    service_manager::{
        ServiceInstallResult, ServiceOperationError, ServiceReport, ServiceRunState,
        ServiceUninstallResult, foreign_error, manager_unavailable, operation_error,
        permission_error,
    },
};

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const SERVICE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct ServiceRuntime {
    name: String,
    options: DaemonOptions,
}

static SERVICE_RUNTIME: OnceLock<ServiceRuntime> = OnceLock::new();
static SERVICE_RESULT: OnceLock<Mutex<Option<Result<(), ServiceOperationError>>>> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

pub fn run_dispatcher(name: String, options: DaemonOptions) -> Result<(), ServiceOperationError> {
    SERVICE_RUNTIME
        .set(ServiceRuntime {
            name: name.clone(),
            options,
        })
        .map_err(|_| operation_error("Windows service runtime was already initialized"))?;
    SERVICE_RESULT.get_or_init(|| Mutex::new(None));
    service_dispatcher::start(&name, ffi_service_main).map_err(map_manager_error)?;
    SERVICE_RESULT
        .get()
        .and_then(|result| result.lock().ok()?.take())
        .unwrap_or_else(|| Err(operation_error("Windows service exited without a result")))
}

pub fn install(
    descriptor: &ServiceDescriptor,
    no_start: bool,
) -> Result<ServiceInstallResult, ServiceOperationError> {
    let spec = windows_spec(descriptor)?;
    let manager = ServiceManager::local_computer(
        None::<&OsStr>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(map_manager_error)?;
    let access = service_access();
    let existing = match manager.open_service(&spec.service_name, access) {
        Ok(service) => Some(service),
        Err(error) if error_code(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => None,
        Err(error) => return Err(map_manager_error(error)),
    };
    let desired = service_info(spec);
    let (service, artifact_updated, was_running) = if let Some(service) = existing {
        let config = service.query_config().map_err(map_operation_error)?;
        ensure_managed(spec, &config.display_name)?;
        let drifted = config.display_name != desired.display_name
            || config.executable_path.to_string_lossy() != spec.binary_path
            || config.start_type != ServiceStartType::AutoStart
            || config.service_type != ServiceType::OWN_PROCESS;
        let status = service.query_status().map_err(map_operation_error)?;
        let was_running = is_running(status.current_state);
        if was_running && !no_start {
            stop_and_wait(&service)?;
        }
        if drifted {
            service
                .change_config(&desired)
                .map_err(map_operation_error)?;
            service
                .set_description(&spec.description)
                .map_err(map_operation_error)?;
        }
        (service, drifted, was_running)
    } else {
        let service = manager
            .create_service(&desired, access)
            .map_err(map_operation_error)?;
        service
            .set_description(&spec.description)
            .map_err(map_operation_error)?;
        (service, true, false)
    };

    let mut started = false;
    if !no_start {
        let state = service
            .query_status()
            .map_err(map_operation_error)?
            .current_state;
        if state != ServiceState::Running {
            service.start::<&OsStr>(&[]).map_err(map_operation_error)?;
            wait_for_state(&service, ServiceState::Running)?;
            started = true;
        } else if artifact_updated && was_running {
            started = true;
        }
    }
    Ok(ServiceInstallResult {
        artifact_updated,
        enabled: true,
        started,
    })
}

pub fn uninstall(
    descriptor: &ServiceDescriptor,
) -> Result<ServiceUninstallResult, ServiceOperationError> {
    let spec = windows_spec(descriptor)?;
    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
        .map_err(map_manager_error)?;
    let service = match manager.open_service(&spec.service_name, service_access()) {
        Ok(service) => service,
        Err(error) if error_code(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            return Ok(ServiceUninstallResult {
                artifact_removed: false,
            });
        }
        Err(error) => return Err(map_manager_error(error)),
    };
    let config = service.query_config().map_err(map_operation_error)?;
    ensure_managed(spec, &config.display_name)?;
    stop_and_wait(&service)?;
    service.delete().map_err(map_operation_error)?;
    Ok(ServiceUninstallResult {
        artifact_removed: true,
    })
}

pub fn status(descriptor: &ServiceDescriptor) -> Result<ServiceReport, ServiceOperationError> {
    let spec = windows_spec(descriptor)?;
    let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
        .map_err(map_manager_error)?;
    let service = match manager.open_service(
        &spec.service_name,
        ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(service) => service,
        Err(error) if error_code(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            return Ok(ServiceReport {
                artifact: ServiceStatus::NotInstalled,
                loaded: false,
                enabled: false,
                run_state: ServiceRunState::Stopped,
            });
        }
        Err(error) => return Err(map_manager_error(error)),
    };
    let config = service.query_config().map_err(map_operation_error)?;
    let status = service.query_status().map_err(map_operation_error)?;
    let ownership = ownership_status(spec, &config.display_name);
    let artifact = if ownership == ServiceStatus::Foreign {
        ServiceStatus::Foreign
    } else {
        ServiceStatus::Installed {
            drifted: config.display_name != service_info(spec).display_name
                || config.executable_path.to_string_lossy() != spec.binary_path
                || config.start_type != ServiceStartType::AutoStart
                || config.service_type != ServiceType::OWN_PROCESS,
        }
    };
    Ok(ServiceReport {
        artifact,
        loaded: true,
        enabled: config.start_type != ServiceStartType::Disabled,
        run_state: service_run_state(&status),
    })
}

fn service_info(spec: &WindowsServiceSpec) -> ServiceInfo {
    ServiceInfo {
        name: OsString::from(&spec.service_name),
        display_name: OsString::from(spec.managed_display_name()),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: PathBuf::from(&spec.argv[0]),
        launch_arguments: spec.argv[1..].iter().map(OsString::from).collect(),
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    }
}

fn service_access() -> ServiceAccess {
    ServiceAccess::QUERY_CONFIG
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::DELETE
}

fn windows_spec(
    descriptor: &ServiceDescriptor,
) -> Result<&WindowsServiceSpec, ServiceOperationError> {
    match &descriptor.artifact {
        ServiceArtifact::WindowsScm(spec) => Ok(spec),
        ServiceArtifact::File { .. } => Err(operation_error("Windows SCM descriptor is required")),
    }
}

fn ensure_managed(
    spec: &WindowsServiceSpec,
    display_name: &OsStr,
) -> Result<(), ServiceOperationError> {
    if ownership_status(spec, display_name) == ServiceStatus::Foreign {
        Err(foreign_error())
    } else {
        Ok(())
    }
}

fn ownership_status(spec: &WindowsServiceSpec, display_name: &OsStr) -> ServiceStatus {
    spec.inspect_display_name(&display_name.to_string_lossy())
}

fn stop_and_wait(service: &windows_service::service::Service) -> Result<(), ServiceOperationError> {
    let mut state = service
        .query_status()
        .map_err(map_operation_error)?
        .current_state;
    if state == ServiceState::Stopped {
        return Ok(());
    }
    if matches!(
        state,
        ServiceState::StartPending | ServiceState::ContinuePending
    ) {
        wait_for_state(service, ServiceState::Running)?;
        state = ServiceState::Running;
    }
    if state == ServiceState::PausePending {
        wait_for_state(service, ServiceState::Paused)?;
        state = ServiceState::Paused;
    }
    if state != ServiceState::StopPending {
        service.stop().map_err(map_operation_error)?;
    }
    wait_for_state(service, ServiceState::Stopped)
}

fn wait_for_state(
    service: &windows_service::service::Service,
    expected: ServiceState,
) -> Result<(), ServiceOperationError> {
    let started = Instant::now();
    while started.elapsed() < SERVICE_OPERATION_TIMEOUT {
        let state = service
            .query_status()
            .map_err(map_operation_error)?
            .current_state;
        if state == expected {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err(operation_error(
        "Windows service state transition timed out",
    ))
}

fn is_running(state: ServiceState) -> bool {
    matches!(
        state,
        ServiceState::Running | ServiceState::StartPending | ServiceState::ContinuePending
    )
}

fn service_run_state(status: &WindowsServiceStatus) -> ServiceRunState {
    match status.current_state {
        ServiceState::Running | ServiceState::StartPending | ServiceState::ContinuePending => {
            ServiceRunState::Running
        }
        ServiceState::Stopped if status.exit_code != ServiceExitCode::NO_ERROR => {
            ServiceRunState::Failed
        }
        ServiceState::Stopped | ServiceState::StopPending => ServiceRunState::Stopped,
        ServiceState::Paused | ServiceState::PausePending => ServiceRunState::Unknown,
    }
}

fn error_code(error: &WindowsServiceError) -> Option<i32> {
    match error {
        WindowsServiceError::Winapi(error) => error.raw_os_error(),
        _ => None,
    }
}

fn map_manager_error(error: WindowsServiceError) -> ServiceOperationError {
    if error_code(&error) == Some(ERROR_ACCESS_DENIED) {
        permission_error()
    } else {
        manager_unavailable("Windows Service Control Manager is unavailable")
    }
}

fn map_operation_error(error: WindowsServiceError) -> ServiceOperationError {
    if error_code(&error) == Some(ERROR_ACCESS_DENIED) {
        permission_error()
    } else {
        operation_error("Windows service operation failed")
    }
}

fn service_main(_arguments: Vec<OsString>) {
    let result = run_service_main();
    if let Some(slot) = SERVICE_RESULT.get()
        && let Ok(mut slot) = slot.lock()
    {
        *slot = Some(result);
    }
}

fn run_service_main() -> Result<(), ServiceOperationError> {
    let runtime = SERVICE_RUNTIME
        .get()
        .cloned()
        .ok_or_else(|| operation_error("Windows service runtime is not configured"))?;
    let cancellation = CancellationToken::new();
    let status_slot: Arc<Mutex<Option<ServiceStatusHandle>>> = Arc::new(Mutex::new(None));
    let handler_status = Arc::clone(&status_slot);
    let handler_cancellation = cancellation.clone();
    let event_handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Ok(slot) = handler_status.lock()
                && let Some(handle) = *slot
            {
                let _ = handle.set_service_status(service_status(
                    ServiceState::StopPending,
                    ServiceControlAccept::empty(),
                    ServiceExitCode::NO_ERROR,
                    1,
                    SERVICE_OPERATION_TIMEOUT,
                ));
            }
            handler_cancellation.cancel();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(&runtime.name, event_handler)
        .map_err(map_operation_error)?;
    *status_slot
        .lock()
        .map_err(|_| operation_error("Windows service status lock failed"))? = Some(status_handle);
    status_handle
        .set_service_status(service_status(
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            ServiceExitCode::NO_ERROR,
            1,
            Duration::from_secs(10),
        ))
        .map_err(map_operation_error)?;
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| operation_error("Windows service async runtime could not be created"))?;
    let daemon_result = tokio.block_on(async {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let mut daemon = tokio::spawn(run_daemon_with_ready(
            runtime.options,
            cancellation.clone(),
            ready_tx,
        ));
        tokio::select! {
            ready = ready_rx => {
                ready.map_err(|_| operation_error("Windows service daemon stopped before becoming ready"))?;
                if !cancellation.is_cancelled() {
                    status_handle
                        .set_service_status(service_status(
                            ServiceState::Running,
                            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                            ServiceExitCode::NO_ERROR,
                            0,
                            Duration::ZERO,
                        ))
                        .map_err(map_operation_error)?;
                }
                daemon.await.map_err(|_| operation_error("Windows service daemon task failed"))?
                    .map_err(|_| operation_error("Windows service daemon stopped with an error"))
            }
            result = &mut daemon => {
                result.map_err(|_| operation_error("Windows service daemon task failed"))?
                    .map_err(|_| operation_error("Windows service daemon stopped before becoming ready"))
            }
            () = tokio::time::sleep(Duration::from_secs(10)) => {
                cancellation.cancel();
                let _ = daemon.await;
                Err(operation_error("Windows service daemon readiness timed out"))
            }
        }
    });
    let exit_code = if daemon_result.is_ok() {
        ServiceExitCode::NO_ERROR
    } else {
        ServiceExitCode::ServiceSpecific(1)
    };
    status_handle
        .set_service_status(service_status(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            exit_code,
            0,
            Duration::ZERO,
        ))
        .map_err(map_operation_error)?;
    daemon_result
}

fn service_status(
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    exit_code: ServiceExitCode,
    checkpoint: u32,
    wait_hint: Duration,
) -> WindowsServiceStatus {
    WindowsServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state,
        controls_accepted,
        exit_code,
        checkpoint,
        wait_hint,
        process_id: None,
    }
}
