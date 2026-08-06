use std::{
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use mcp_host_core::{
    AuthLoginStartResult, BatchToolCall, BatchToolCallOutcome, BatchToolCallResponse, CallPolicy,
    ControlRequest, RuntimeError, RuntimeErrorCode, SkillRunResult, SkillRunStatus,
};
use serde_json::{Value, json};

use crate::cli::{
    AuthCommand, Batch, Call, Cli, Command, DaemonCommand, ExitCode as CliExitCode, HarnessCommand,
    PackageCommand, ServiceCommand, ServiceManager, ServiceOptions, SkillCommand,
    SkillRun as SkillRunArgs,
};
use crate::ipc::send_control;
use crate::output::{render_human, render_status_stats};
use crate::service::{
    ServiceArtifact, ServiceDescriptor, ServiceInstallOptions, ServiceManagerKind,
    ServiceScope as DescriptorScope, ServiceStatus, render_descriptor,
};
use crate::service_manager::{
    ServiceOperationError, ServiceOperationErrorKind, ServiceRunState, install_service,
    service_status, uninstall_service,
};
use crate::{DaemonOptions, install_harnesses, run_daemon, run_stdio_bridge_at};

pub const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(65);

/// Returns the current working directory as a usage-memory project label.
fn project_hint() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|directory| {
            let mut label = directory.display().to_string();
            label.truncate(256);
            label
        })
        .filter(|label| !label.is_empty())
}
pub const DEFAULT_AUTH_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const AUTH_CALLBACK_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_SKILL_CONTROL_TIMEOUT: Duration = Duration::from_secs(4_805);
pub const MCP_BRIDGE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn dispatch(cli: Cli) -> Result<CliExitCode, RuntimeError> {
    let Cli {
        runtime_dir,
        json,
        timeout,
        command,
    } = cli;
    let command = match command {
        Command::Shell => {
            let resolved = resolve_runtime_dir(runtime_dir)?;
            let control_timeout = timeout.map_or(DEFAULT_CONTROL_TIMEOUT, Duration::from_millis);
            crate::shell::run_shell(resolved, control_timeout).await?;
            return Ok(CliExitCode::Success);
        }
        command => command,
    };

    dispatch_local(Cli {
        runtime_dir,
        json,
        timeout,
        command,
    })
    .await
}

/// Dispatch commands that do not require the daemon. Shell sessions route
/// these here so the interactive loop never recurses through `dispatch`.
pub async fn dispatch_local(cli: Cli) -> Result<CliExitCode, RuntimeError> {
    let Cli {
        runtime_dir,
        json,
        timeout,
        command,
    } = cli;
    let runtime_dir_override = runtime_dir.clone();
    let command = match command {
        Command::Harness(harness) => match harness.command {
            HarnessCommand::Install(install) => {
                let result = install_harnesses(install, runtime_dir_override.as_deref()).await?;
                write_value(&result, json)?;
                return Ok(CliExitCode::Success);
            }
        },
        Command::Completions(completions) => {
            write_completions(completions.shell)?;
            return Ok(CliExitCode::Success);
        }
        Command::Init(init) => {
            init_configuration(init.dir, init.force, json)?;
            return Ok(CliExitCode::Success);
        }
        Command::Doctor => {
            let resolved = resolve_runtime_dir(runtime_dir)?;
            let report = crate::doctor::run_doctor(&resolved, timeout, json).await;
            if !json {
                report.print();
            }
            return Ok(if report.has_errors() {
                CliExitCode::RuntimeFailure
            } else {
                CliExitCode::Success
            });
        }
        command => command,
    };

    dispatch_daemon(Cli {
        runtime_dir,
        json,
        timeout,
        command,
    })
    .await
}

/// Dispatch a daemon-bound command. Shell sessions call this directly so the
/// interactive loop never recurses through `dispatch`.
pub async fn dispatch_daemon(cli: Cli) -> Result<CliExitCode, RuntimeError> {
    let Cli {
        runtime_dir,
        json,
        timeout,
        command,
    } = cli;
    let runtime_dir_override = runtime_dir.clone();
    let runtime_dir = resolve_runtime_dir(runtime_dir)?;
    let control_timeout = timeout.map_or(DEFAULT_CONTROL_TIMEOUT, Duration::from_millis);
    let auth_login_timeout = timeout.map_or(DEFAULT_AUTH_LOGIN_TIMEOUT, Duration::from_millis);

    match command {
        Command::Daemon(daemon) => match daemon.command {
            DaemonCommand::Run(run) => {
                run_daemon(DaemonOptions {
                    config_dir: run.config_dir,
                    runtime_dir,
                    opencode_serve_url: run.opencode_serve_url.clone(),
                })
                .await?;
                Ok(CliExitCode::Success)
            }
            DaemonCommand::Status => {
                execute_control(
                    &runtime_dir,
                    ControlRequest::Status,
                    control_timeout,
                    json,
                    Some("status"),
                )
                .await
            }
            DaemonCommand::Stop => {
                execute_control(
                    &runtime_dir,
                    ControlRequest::Shutdown,
                    control_timeout,
                    json,
                    None,
                )
                .await
            }
            DaemonCommand::Service(service) => execute_service(
                service.command,
                &runtime_dir,
                runtime_dir_override.is_some(),
                json,
            ),
            #[cfg(windows)]
            DaemonCommand::ServiceRun(run) => {
                crate::run_windows_service(
                    run.name,
                    DaemonOptions {
                        config_dir: run.config_dir,
                        runtime_dir,
                        opencode_serve_url: None,
                    },
                )
                .map_err(service_operation_error)?;
                Ok(CliExitCode::Success)
            }
        },
        Command::List => {
            execute_control(
                &runtime_dir,
                ControlRequest::ListServers,
                control_timeout,
                json,
                Some("list"),
            )
            .await
        }
        Command::Inspect { server_id } => {
            execute_control(
                &runtime_dir,
                ControlRequest::InspectServer { server_id },
                control_timeout,
                json,
                None,
            )
            .await
        }
        Command::Connect { server_id } => {
            execute_control(
                &runtime_dir,
                ControlRequest::ConnectServer {
                    server_id,
                    project: project_hint(),
                },
                control_timeout,
                json,
                Some("connect"),
            )
            .await
        }
        Command::Disconnect { server_id } => {
            execute_control(
                &runtime_dir,
                ControlRequest::DisconnectServer { server_id },
                control_timeout,
                json,
                Some("disconnect"),
            )
            .await
        }
        Command::Tools { server_id, refresh } => {
            execute_control(
                &runtime_dir,
                ControlRequest::ListTools { server_id, refresh },
                control_timeout,
                json,
                Some("tools"),
            )
            .await
        }
        Command::Refresh { server_id } => {
            execute_control(
                &runtime_dir,
                ControlRequest::RefreshServer { server_id },
                control_timeout,
                json,
                None,
            )
            .await
        }
        Command::Call(call) => {
            let arguments = read_call_arguments(&call).await?;
            let call_policy = CallPolicy {
                auto_connect: !call.no_auto_connect,
                auto_retry: !call.no_retry,
                max_output_tokens: call.max_output_tokens,
            };
            let result = send_control(
                &runtime_dir,
                &ControlRequest::CallTool {
                    server_id: call.server_id,
                    tool_name: call.tool_name,
                    arguments,
                    timeout_ms: timeout,
                    call_policy,
                },
                control_timeout,
            )
            .await?;
            write_value(&result, json)?;

            Ok(
                if result.get("isError").and_then(Value::as_bool) == Some(true) {
                    CliExitCode::UpstreamToolError
                } else {
                    CliExitCode::Success
                },
            )
        }
        Command::Batch(batch) => {
            let mut calls = read_batch_calls(&batch).await?;
            for call in &mut calls {
                if call.timeout_ms.is_none() {
                    call.timeout_ms = timeout;
                }
            }
            let batch_timeout = batch_control_timeout(control_timeout, &calls);
            let result = send_control(
                &runtime_dir,
                &ControlRequest::CallTools { calls },
                batch_timeout,
            )
            .await?;
            let response: BatchToolCallResponse =
                serde_json::from_value(result.clone()).map_err(|_| batch_response_error())?;
            let exit_code = batch_exit_code(&response);
            write_value(&result, json)?;
            Ok(exit_code)
        }
        Command::Status(status) => {
            if status.stats {
                let status_value =
                    send_control(&runtime_dir, &ControlRequest::Status, control_timeout).await?;
                let servers_value =
                    send_control(&runtime_dir, &ControlRequest::ListServers, control_timeout)
                        .await?;
                let combined = json!({ "status": status_value, "servers": servers_value });
                if json {
                    write_value(&combined, true)?;
                } else {
                    if let Some(rendered) = render_status_stats(&combined) {
                        println!("{rendered}");
                    } else {
                        write_value(&combined, false)?;
                    }
                }
                return Ok(CliExitCode::Success);
            }
            execute_control(
                &runtime_dir,
                ControlRequest::Status,
                control_timeout,
                json,
                Some("status"),
            )
            .await
        }
        Command::Auth(auth) => match auth.command {
            AuthCommand::Login { server_id } => {
                execute_auth_login(&runtime_dir, server_id, auth_login_timeout, json).await
            }
            AuthCommand::Status { server_id } => {
                execute_control(
                    &runtime_dir,
                    ControlRequest::AuthStatus { server_id },
                    control_timeout,
                    json,
                    None,
                )
                .await
            }
            AuthCommand::Logout { server_id } => {
                execute_control(
                    &runtime_dir,
                    ControlRequest::AuthLogout { server_id },
                    control_timeout,
                    json,
                    None,
                )
                .await
            }
        },
        Command::Skill(skill) => match skill.command {
            SkillCommand::List => {
                execute_control(
                    &runtime_dir,
                    ControlRequest::SkillList,
                    control_timeout,
                    json,
                    Some("skill-list"),
                )
                .await
            }
            SkillCommand::Run(run) => {
                let inputs = read_skill_inputs(&run).await?;
                let skill_timeout =
                    timeout.map_or(DEFAULT_SKILL_CONTROL_TIMEOUT, Duration::from_millis);
                let result = send_control(
                    &runtime_dir,
                    &ControlRequest::SkillRun {
                        skill_id: run.skill_id,
                        inputs,
                    },
                    skill_timeout,
                )
                .await?;
                let response: SkillRunResult =
                    serde_json::from_value(result.clone()).map_err(|_| skill_response_error())?;
                let exit_code = skill_exit_code(&response);
                write_value(&result, json)?;
                Ok(exit_code)
            }
        },
        Command::Package(package) => match package.command {
            PackageCommand::Install { server_id } => {
                let package_timeout =
                    timeout.map_or(Duration::from_secs(300), Duration::from_millis);
                execute_control(
                    &runtime_dir,
                    ControlRequest::PackageInstall { server_id },
                    package_timeout,
                    json,
                    None,
                )
                .await
            }
        },
        Command::Harness(_) => unreachable!("harness commands return before runtime resolution"),
        Command::Mcp(mcp) => {
            let endpoint = match mcp.endpoint.as_deref() {
                Some(endpoint) => std::borrow::Cow::Borrowed(endpoint),
                None => std::borrow::Cow::Owned(std::path::PathBuf::from(
                    crate::ipc::EndpointSet::for_runtime_dir(&runtime_dir)
                        .map_err(|_| {
                            RuntimeError::new(
                                RuntimeErrorCode::IpcUnavailable,
                                "mcp",
                                "failed to resolve the MCP endpoint for this runtime directory",
                            )
                        })?
                        .address(crate::ipc::EndpointKind::Mcp)
                        .to_owned(),
                )),
            };
            run_stdio_bridge_at(&endpoint, MCP_BRIDGE_CONNECT_TIMEOUT).await?;
            Ok(CliExitCode::Success)
        }
        Command::Shell | Command::Completions(_) | Command::Init(_) | Command::Doctor => {
            unreachable!("non-daemon commands return before runtime resolution")
        }
    }
}

fn write_completions(shell: crate::cli::CompletionShell) -> Result<(), RuntimeError> {
    use clap::CommandFactory as _;
    use clap_complete::Shell;

    let shell = match shell {
        crate::cli::CompletionShell::Bash => Shell::Bash,
        crate::cli::CompletionShell::Zsh => Shell::Zsh,
        crate::cli::CompletionShell::Fish => Shell::Fish,
        crate::cli::CompletionShell::Powershell => Shell::PowerShell,
        crate::cli::CompletionShell::Elvish => Shell::Elvish,
    };
    let mut command = Cli::command();
    let mut buffer = Vec::new();
    clap_complete::generate(shell, &mut command, "mcp-host", &mut buffer);
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    if let Err(error) = stdout.write_all(&buffer).and_then(|()| stdout.flush())
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        eprintln!("completions: {error}");
    }
    Ok(())
}

fn init_configuration(
    directory: Option<PathBuf>,
    force: bool,
    json_output: bool,
) -> Result<(), RuntimeError> {
    let directory = directory.unwrap_or_else(|| PathBuf::from("config"));
    std::fs::create_dir_all(&directory).map_err(|error| {
        init_error(format!(
            "could not create the configuration directory {}: {error}",
            directory.display()
        ))
    })?;
    let files: [(&str, &str); 3] = [
        ("example.toml", SAMPLE_MANIFEST),
        ("policy.toml", SAMPLE_POLICY),
        ("README.md", INIT_README),
    ];
    let mut written = Vec::new();
    let mut skipped = Vec::new();
    for (name, content) in files {
        let path = directory.join(name);
        if path.exists() && !force {
            skipped.push(path.clone());
            continue;
        }
        std::fs::write(&path, content)
            .map_err(|error| init_error(format!("could not write {}: {error}", path.display())))?;
        written.push(path);
    }
    let value = json!({
        "directory": directory,
        "written": written.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
    });
    if json_output {
        write_value(&value, true)?;
    } else {
        for path in &written {
            println!("created {}", path.display());
        }
        for path in &skipped {
            println!("skipped existing {}", path.display());
        }
        if written.is_empty() {
            println!(
                "nothing to create in {} (use --force to overwrite)",
                directory.display()
            );
        } else {
            println!(
                "\nnext: mcp-host daemon run --config-dir {}",
                directory.display()
            );
        }
    }
    Ok(())
}

const SAMPLE_MANIFEST: &str = r#"# Example downstream MCP server manifest for Dynamic MCP Host.
# Every *.toml file in the configuration directory is a server manifest.

id = "example"
name = "Example Server"
description = "Example stdio MCP server"

[transport]
type = "stdio"
# The command must be on the daemon's PATH or an absolute path.
command = "your-mcp-server"
# arguments = ["--port", "${PORT}"]
# working_directory = "/path/to/work"
# [transport.environment]
# PORT = "${EXAMPLE_PORT}"
"#;

const SAMPLE_POLICY: &str = r#"# Call policy for Dynamic MCP Host.
# Rules are evaluated in order; the first match wins. Unmatched calls are allowed.

# [[rules]]
# id = "deny-dangerous"
# action = "call"
# effect = "deny"
# server = "example"
# tool = "dangerous_*"
"#;

const INIT_README: &str = r#"# Dynamic MCP Host configuration

This directory is the daemon configuration directory.

- `*.toml` server manifests declare the downstream MCP servers.
- `policy.toml` holds call and skill policy rules.
- `*.skill.toml` files (same directory) define runtime skills.

Start the daemon:

    mcp-host daemon run --config-dir .

Then control it from a second terminal:

    mcp-host list
    mcp-host connect example
    mcp-host tools example
    mcp-host call example your-tool --arguments '{"key":"value"}'

See https://github.com/ugur-murat-alt/MCP-Host for full documentation.
"#;

fn init_error(message: String) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::InvalidArguments, "init", message)
}

pub async fn execute_auth_login(
    runtime_dir: &Path,
    server_id: String,
    timeout: Duration,
    json_output: bool,
) -> Result<CliExitCode, RuntimeError> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|_| auth_login_error("the OAuth loopback listener could not be opened"))?;
    let address = listener
        .local_addr()
        .map_err(|_| auth_login_error("the OAuth loopback address is unavailable"))?;
    let redirect_uri = format!("http://{address}/callback");
    let started = send_control(
        runtime_dir,
        &ControlRequest::AuthStart {
            server_id: server_id.clone(),
            redirect_uri: redirect_uri.clone(),
        },
        timeout,
    )
    .await?;
    let started: AuthLoginStartResult = serde_json::from_value(started)
        .map_err(|_| auth_login_error("the daemon returned an invalid OAuth response"))?;
    writeln!(
        std::io::stderr(),
        "Open this URL in your browser:\n{}",
        started.authorization_url
    )
    .map_err(|_| output_error())?;

    let (mut stream, callback_url) =
        receive_oauth_callback(listener, &redirect_uri, timeout).await?;
    let completed = send_control(
        runtime_dir,
        &ControlRequest::AuthComplete {
            server_id,
            callback_url,
        },
        timeout,
    )
    .await;
    let success = completed.is_ok();
    write_oauth_browser_response(&mut stream, success).await;
    let completed = completed?;
    write_value(&completed, json_output)?;
    Ok(CliExitCode::Success)
}

async fn receive_oauth_callback(
    listener: tokio::net::TcpListener,
    redirect_uri: &str,
    timeout: Duration,
) -> Result<(tokio::net::TcpStream, String), RuntimeError> {
    receive_oauth_callback_with_request_timeout(
        listener,
        redirect_uri,
        timeout,
        AUTH_CALLBACK_REQUEST_TIMEOUT,
    )
    .await
}

async fn receive_oauth_callback_with_request_timeout(
    listener: tokio::net::TcpListener,
    redirect_uri: &str,
    timeout: Duration,
    request_timeout: Duration,
) -> Result<(tokio::net::TcpStream, String), RuntimeError> {
    tokio::time::timeout(timeout, async move {
        loop {
            let (mut stream, peer) = listener.accept().await.map_err(|_| {
                auth_login_error("the OAuth loopback callback could not be accepted")
            })?;
            if !peer.ip().is_loopback() {
                write_oauth_browser_response(&mut stream, false).await;
                continue;
            }
            let target =
                match tokio::time::timeout(request_timeout, read_oauth_request_target(&mut stream))
                    .await
                {
                    Ok(Ok(Some(target))) => target,
                    Ok(Ok(None) | Err(_)) | Err(_) => {
                        write_oauth_browser_response(&mut stream, false).await;
                        continue;
                    }
                };
            if !target.starts_with("/callback?") {
                write_oauth_browser_response(&mut stream, false).await;
                continue;
            }
            return Ok((
                stream,
                format!("{redirect_uri}{suffix}", suffix = &target[9..]),
            ));
        }
    })
    .await
    .map_err(|_| auth_login_error("the OAuth loopback callback timed out"))?
}

async fn read_oauth_request_target(
    stream: &mut tokio::net::TcpStream,
) -> Result<Option<String>, RuntimeError> {
    use tokio::io::AsyncReadExt as _;

    const MAX_REQUEST_BYTES: usize = 16 * 1024;
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while request.len() < MAX_REQUEST_BYTES {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|_| auth_login_error("the OAuth loopback request could not be read"))?;
        if read == 0 {
            return Ok(None);
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    if request.len() >= MAX_REQUEST_BYTES {
        return Ok(None);
    }
    let request = std::str::from_utf8(&request).ok();
    let mut parts = request
        .and_then(|request| request.lines().next())
        .map(str::split_ascii_whitespace)
        .into_iter()
        .flatten();
    let method = parts.next();
    let target = parts.next();
    let version = parts.next();
    if method != Some("GET")
        || !version.is_some_and(|version| version.starts_with("HTTP/"))
        || parts.next().is_some()
    {
        return Ok(None);
    }
    Ok(target.map(str::to_owned))
}

async fn write_oauth_browser_response(stream: &mut tokio::net::TcpStream, success: bool) {
    use tokio::io::AsyncWriteExt as _;

    let (status, body) = if success {
        ("200 OK", "OAuth login completed. You may close this tab.")
    } else {
        (
            "400 Bad Request",
            "OAuth login failed. Return to the terminal for details.",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn auth_login_error(message: &'static str) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::AuthFailed, "auth_login", message)
}

fn execute_service(
    command: ServiceCommand,
    runtime_dir: &Path,
    runtime_dir_explicit: bool,
    json_output: bool,
) -> Result<CliExitCode, RuntimeError> {
    let options = match &command {
        ServiceCommand::Install(install) => &install.options,
        ServiceCommand::Uninstall(options) | ServiceCommand::Status(options) => options,
    };
    let descriptor = service_descriptor(options, runtime_dir, runtime_dir_explicit)?;
    let value = match command {
        ServiceCommand::Install(install) => {
            let result =
                install_service(&descriptor, install.no_start).map_err(service_operation_error)?;
            json!({
                "installed": true,
                "updated": result.artifact_updated,
                "enabled": result.enabled,
                "started": result.started,
                "descriptor": service_path(&descriptor),
            })
        }
        ServiceCommand::Uninstall(_) => {
            let result = uninstall_service(&descriptor).map_err(service_operation_error)?;
            json!({
                "installed": false,
                "removed": result.artifact_removed,
                "descriptor": service_path(&descriptor),
            })
        }
        ServiceCommand::Status(_) => {
            let report = service_status(&descriptor).map_err(service_operation_error)?;
            json!({
                "artifact": service_artifact_status(&report.artifact),
                "loaded": report.loaded,
                "enabled": report.enabled,
                "active": service_run_state(report.run_state),
                "descriptor": service_path(&descriptor),
            })
        }
    };
    write_value(&value, json_output)?;
    Ok(CliExitCode::Success)
}

fn service_descriptor(
    options: &ServiceOptions,
    runtime_dir: &Path,
    runtime_dir_explicit: bool,
) -> Result<ServiceDescriptor, RuntimeError> {
    let executable = std::fs::canonicalize(std::env::current_exe().map_err(service_error)?)
        .map_err(service_error)?;
    let config_dir = std::fs::canonicalize(&options.config_dir).map_err(service_error)?;
    let manager = match options.manager {
        ServiceManager::Auto => native_service_manager()?,
        ServiceManager::Systemd => ServiceManagerKind::Systemd,
        ServiceManager::Launchd => ServiceManagerKind::Launchd,
        ServiceManager::WindowsScm => ServiceManagerKind::WindowsScm,
    };
    let scope = match options.scope {
        Some(crate::cli::ServiceScope::User) => DescriptorScope::User,
        Some(crate::cli::ServiceScope::System) => DescriptorScope::System,
        None if manager == ServiceManagerKind::WindowsScm => DescriptorScope::System,
        None => DescriptorScope::User,
    };
    let runtime_dir = service_runtime_dir(manager, scope, runtime_dir, runtime_dir_explicit)?;
    std::fs::create_dir_all(&runtime_dir).map_err(service_error)?;
    let runtime_dir = std::fs::canonicalize(runtime_dir).map_err(service_error)?;
    let base = directories::BaseDirs::new();
    let descriptor_dir = match (manager, scope) {
        (ServiceManagerKind::Systemd, DescriptorScope::User) => std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| base.as_ref().map(|base| base.home_dir().join(".config")))
            .ok_or_else(|| service_error("could not determine the user config directory"))?
            .join("systemd/user"),
        (ServiceManagerKind::Systemd, DescriptorScope::System) => {
            PathBuf::from("/etc/systemd/system")
        }
        (ServiceManagerKind::Launchd, DescriptorScope::User) => base
            .as_ref()
            .ok_or_else(|| service_error("could not determine the user home directory"))?
            .home_dir()
            .join("Library/LaunchAgents"),
        (ServiceManagerKind::Launchd, DescriptorScope::System) => {
            PathBuf::from("/Library/LaunchDaemons")
        }
        (ServiceManagerKind::WindowsScm, _) => runtime_dir.clone(),
    };
    render_descriptor(
        manager,
        &ServiceInstallOptions {
            scope,
            name: options.name.clone(),
            description: "Dynamic MCP Host daemon".to_owned(),
            executable,
            config_dir,
            runtime_dir,
            descriptor_dir,
        },
    )
    .map_err(service_error)
}

fn service_runtime_dir(
    manager: ServiceManagerKind,
    scope: DescriptorScope,
    runtime_dir: &Path,
    runtime_dir_explicit: bool,
) -> Result<PathBuf, RuntimeError> {
    #[cfg(windows)]
    if manager == ServiceManagerKind::WindowsScm
        && scope == DescriptorScope::System
        && !runtime_dir_explicit
    {
        return std::env::var_os("ProgramData")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join("Dynamic MCP/runtime"))
            .ok_or_else(|| service_error("could not determine the ProgramData directory"));
    }
    let _ = (manager, scope, runtime_dir_explicit);
    Ok(runtime_dir.to_path_buf())
}

fn native_service_manager() -> Result<ServiceManagerKind, RuntimeError> {
    #[cfg(target_os = "linux")]
    return Ok(ServiceManagerKind::Systemd);
    #[cfg(target_os = "macos")]
    return Ok(ServiceManagerKind::Launchd);
    #[cfg(windows)]
    return Ok(ServiceManagerKind::WindowsScm);
    #[allow(unreachable_code)]
    Err(service_error(
        "this platform has no supported service manager",
    ))
}

fn service_path(descriptor: &ServiceDescriptor) -> Value {
    match &descriptor.artifact {
        ServiceArtifact::File { path, .. } => json!(path),
        ServiceArtifact::WindowsScm(spec) => json!(spec.service_name),
    }
}

fn service_artifact_status(status: &ServiceStatus) -> &'static str {
    match status {
        ServiceStatus::NotInstalled => "not_installed",
        ServiceStatus::Installed { drifted: false } => "installed",
        ServiceStatus::Installed { drifted: true } => "drifted",
        ServiceStatus::Foreign => "foreign",
    }
}

const fn service_run_state(state: ServiceRunState) -> &'static str {
    match state {
        ServiceRunState::Running => "running",
        ServiceRunState::Stopped => "stopped",
        ServiceRunState::Failed => "failed",
        ServiceRunState::Unknown => "unknown",
    }
}

fn service_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::InvalidArguments,
        "daemon_service",
        error.to_string(),
    )
}

fn service_operation_error(error: ServiceOperationError) -> RuntimeError {
    let code = match error.kind {
        ServiceOperationErrorKind::InvalidArguments => RuntimeErrorCode::InvalidArguments,
        ServiceOperationErrorKind::Foreign => RuntimeErrorCode::ServiceForeign,
        ServiceOperationErrorKind::PermissionDenied => RuntimeErrorCode::ServicePermissionDenied,
        ServiceOperationErrorKind::ManagerUnavailable => {
            RuntimeErrorCode::ServiceManagerUnavailable
        }
        ServiceOperationErrorKind::Failed => RuntimeErrorCode::ServiceOperationFailed,
    };
    RuntimeError::new(code, "daemon_service", error.message)
}

async fn execute_control(
    runtime_dir: &Path,
    request: ControlRequest,
    timeout: Duration,
    json_output: bool,
    human: Option<&str>,
) -> Result<CliExitCode, RuntimeError> {
    let result = send_control(runtime_dir, &request, timeout).await?;
    write_rendered(
        &result,
        json_output,
        human.and_then(|label| render_human(label, &result)),
    )?;
    Ok(CliExitCode::Success)
}

pub fn resolve_runtime_dir(runtime_dir: Option<PathBuf>) -> Result<PathBuf, RuntimeError> {
    if let Some(runtime_dir) = runtime_dir {
        return Ok(runtime_dir);
    }

    #[cfg(unix)]
    if let Some(runtime_dir) =
        std::env::var_os("XDG_RUNTIME_DIR").filter(|runtime_dir| !runtime_dir.is_empty())
    {
        return Ok(PathBuf::from(runtime_dir).join("mcp-host"));
    }

    directories::ProjectDirs::from("org", "mcp-host", "mcp-host")
        .map(|directories| directories.data_local_dir().join("runtime"))
        .ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ProtocolError,
                "runtime_dir",
                "could not determine a local runtime directory",
            )
        })
}

pub async fn read_call_arguments(call: &Call) -> Result<Value, RuntimeError> {
    let arguments = match (&call.arguments, &call.arguments_file) {
        (Some(arguments), None) => arguments.as_bytes().to_vec(),
        (None, Some(path)) if path == Path::new("-") => {
            let mut arguments = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut tokio::io::stdin(), &mut arguments)
                .await
                .map_err(|_| invalid_arguments_error())?;
            arguments
        }
        (None, Some(path)) => tokio::fs::read(path)
            .await
            .map_err(|_| invalid_arguments_error())?,
        (None, None) => b"{}".to_vec(),
        (Some(_), Some(_)) => return Err(invalid_arguments_error()),
    };

    let value: Value = serde_json::from_slice(&arguments).map_err(|_| invalid_arguments_error())?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(invalid_arguments_error())
    }
}

pub async fn read_skill_inputs(run: &SkillRunArgs) -> Result<Value, RuntimeError> {
    let inputs = match (&run.input, &run.input_file) {
        (Some(inputs), None) => inputs.as_bytes().to_vec(),
        (None, Some(path)) if path == Path::new("-") => {
            let mut inputs = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut tokio::io::stdin(), &mut inputs)
                .await
                .map_err(|_| skill_arguments_error())?;
            inputs
        }
        (None, Some(path)) => tokio::fs::read(path)
            .await
            .map_err(|_| skill_arguments_error())?,
        (None, None) => b"{}".to_vec(),
        (Some(_), Some(_)) => return Err(skill_arguments_error()),
    };
    let value: Value = serde_json::from_slice(&inputs).map_err(|_| skill_arguments_error())?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(skill_arguments_error())
    }
}

pub async fn read_batch_calls(batch: &Batch) -> Result<Vec<BatchToolCall>, RuntimeError> {
    let calls = match (&batch.calls, &batch.calls_file) {
        (Some(calls), None) => calls.as_bytes().to_vec(),
        (None, Some(path)) if path == Path::new("-") => {
            let mut calls = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut tokio::io::stdin(), &mut calls)
                .await
                .map_err(|_| batch_arguments_error())?;
            calls
        }
        (None, Some(path)) => tokio::fs::read(path)
            .await
            .map_err(|_| batch_arguments_error())?,
        _ => return Err(batch_arguments_error()),
    };

    serde_json::from_slice(&calls).map_err(|_| batch_arguments_error())
}

fn batch_control_timeout(base: Duration, calls: &[BatchToolCall]) -> Duration {
    calls
        .iter()
        .filter_map(|call| call.timeout_ms)
        .max()
        .map_or(base, |timeout_ms| {
            base.max(Duration::from_millis(timeout_ms).saturating_add(Duration::from_secs(5)))
        })
}

fn batch_exit_code(response: &BatchToolCallResponse) -> CliExitCode {
    if response
        .results
        .iter()
        .any(|result| matches!(result.outcome, BatchToolCallOutcome::Error { .. }))
    {
        return CliExitCode::RuntimeFailure;
    }
    if response.results.iter().any(|item| {
        matches!(
            &item.outcome,
            BatchToolCallOutcome::Success { result }
                if result.value().get("isError").and_then(Value::as_bool) == Some(true)
        )
    }) {
        return CliExitCode::UpstreamToolError;
    }
    CliExitCode::Success
}

fn skill_exit_code(response: &SkillRunResult) -> CliExitCode {
    if response.status == SkillRunStatus::Ok {
        return CliExitCode::Success;
    }
    if response
        .failure
        .as_ref()
        .is_some_and(|failure| failure.error.code == RuntimeErrorCode::SkillUpstreamError)
    {
        return CliExitCode::UpstreamToolError;
    }
    CliExitCode::RuntimeFailure
}

fn invalid_arguments_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::InvalidArguments,
        "call_arguments",
        "tool arguments must be a JSON object",
    )
}

fn batch_arguments_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::InvalidArguments,
        "batch_arguments",
        "batch calls must be a JSON array of {server_id, tool_name, arguments?, timeout_ms?} items",
    )
}

fn batch_response_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ProtocolError,
        "call_tools",
        "daemon returned an invalid batch response",
    )
}

fn skill_arguments_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::SkillInputInvalid,
        "skill_inputs",
        "skill inputs must be a JSON object",
    )
}

fn skill_response_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ProtocolError,
        "skill_run",
        "daemon returned an invalid skill response",
    )
}

pub fn write_value(value: &Value, json_output: bool) -> Result<(), RuntimeError> {
    write_rendered(value, json_output, None)
}

fn write_rendered(
    value: &Value,
    json_output: bool,
    human: Option<String>,
) -> Result<(), RuntimeError> {
    let rendered = if !json_output {
        if let Some(human) = human {
            human
        } else {
            serde_json::to_string_pretty(value).map_err(|_| output_error())?
        }
    } else {
        serde_json::to_string(value).map_err(|_| output_error())?
    };

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{rendered}").map_err(|_| output_error())
}

pub fn output_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ProtocolError,
        "output",
        "failed to write command output",
    )
}

pub fn exit_code_for_error(error: &RuntimeError) -> CliExitCode {
    match error.code {
        RuntimeErrorCode::IpcUnavailable | RuntimeErrorCode::DaemonNotRunning => {
            CliExitCode::DaemonUnavailable
        }
        _ => CliExitCode::RuntimeFailure,
    }
}

#[cfg(test)]
mod tests {
    use mcp_host_core::{
        BatchToolCallOutcome, BatchToolCallResponse, BatchToolCallResult, CallPolicy, RuntimeError,
        RuntimeErrorCode, SkillRunFailure, SkillRunResult, SkillRunStatus, ToolCallResult,
    };
    use serde_json::json;

    use super::{
        BatchToolCall, CliExitCode, Duration, batch_control_timeout, batch_exit_code,
        read_oauth_request_target, receive_oauth_callback_with_request_timeout, skill_exit_code,
    };

    #[test]
    fn batch_exit_code_reflects_item_errors_and_upstream_tool_errors() {
        let clean = response(BatchToolCallOutcome::Success {
            result: ToolCallResult::new(json!({"isError": false})),
        });
        let upstream_error = response(BatchToolCallOutcome::Success {
            result: ToolCallResult::new(json!({"isError": true})),
        });
        let runtime_error = response(BatchToolCallOutcome::Error {
            error: RuntimeError::for_server(
                RuntimeErrorCode::ToolNotFound,
                "call_tool",
                "fixture",
                "the requested tool was not discovered",
            ),
        });

        assert_eq!(batch_exit_code(&clean), CliExitCode::Success);
        assert_eq!(
            batch_exit_code(&upstream_error),
            CliExitCode::UpstreamToolError
        );
        assert_eq!(batch_exit_code(&runtime_error), CliExitCode::RuntimeFailure);
    }

    #[tokio::test]
    async fn oauth_loopback_parser_accepts_only_a_get_request_target() {
        use tokio::io::AsyncWriteExt as _;

        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let reader = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("callback should connect");
            read_oauth_request_target(&mut stream).await
        });
        let mut client = tokio::net::TcpStream::connect(address)
            .await
            .expect("callback client should connect");
        client
            .write_all(
                format!(
                    "GET /callback?code=sentinel&state=csrf HTTP/1.1\r\nHost: {address}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("callback request should write");

        assert_eq!(
            reader
                .await
                .expect("reader task should join")
                .expect("request should parse")
                .as_deref(),
            Some("/callback?code=sentinel&state=csrf")
        );
    }

    #[tokio::test]
    async fn oauth_loopback_skips_a_stalled_connection() {
        use tokio::io::AsyncWriteExt as _;

        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let redirect_uri = format!("http://{address}/callback");
        let receiver = tokio::spawn(async move {
            receive_oauth_callback_with_request_timeout(
                listener,
                &redirect_uri,
                Duration::from_secs(1),
                Duration::from_millis(20),
            )
            .await
        });
        let _stalled = tokio::net::TcpStream::connect(address)
            .await
            .expect("stalled client should connect");
        let mut callback = tokio::net::TcpStream::connect(address)
            .await
            .expect("callback client should connect");
        callback
            .write_all(b"GET /callback?code=sentinel&state=csrf HTTP/1.1\r\n\r\n")
            .await
            .expect("callback request should write");

        let (_, callback_url) = receiver
            .await
            .expect("receiver task should join")
            .expect("valid callback should be accepted");
        assert_eq!(
            callback_url,
            format!("http://{address}/callback?code=sentinel&state=csrf")
        );
    }

    #[test]
    fn batch_control_timeout_covers_the_longest_item() {
        let calls = [BatchToolCall {
            server_id: "fixture".to_owned(),
            tool_name: "sleep".to_owned(),
            arguments: json!({}),
            timeout_ms: Some(300_000),
            call_policy: CallPolicy::default(),
        }];

        assert_eq!(
            batch_control_timeout(Duration::from_secs(65), &calls),
            Duration::from_secs(305)
        );
    }

    #[test]
    fn skill_exit_code_distinguishes_upstream_and_runtime_failures() {
        let success = skill_response(SkillRunStatus::Ok, None);
        let upstream = skill_response(
            SkillRunStatus::Error,
            Some(RuntimeErrorCode::SkillUpstreamError),
        );
        let runtime = skill_response(SkillRunStatus::Error, Some(RuntimeErrorCode::PolicyDenied));

        assert_eq!(skill_exit_code(&success), CliExitCode::Success);
        assert_eq!(skill_exit_code(&upstream), CliExitCode::UpstreamToolError);
        assert_eq!(skill_exit_code(&runtime), CliExitCode::RuntimeFailure);
    }

    fn response(outcome: BatchToolCallOutcome) -> BatchToolCallResponse {
        BatchToolCallResponse {
            results: vec![BatchToolCallResult {
                server_id: "fixture".to_owned(),
                tool_name: "echo".to_owned(),
                outcome,
            }],
        }
    }

    fn skill_response(status: SkillRunStatus, code: Option<RuntimeErrorCode>) -> SkillRunResult {
        SkillRunResult {
            skill_id: "skill".to_owned(),
            status,
            steps_completed: 0,
            steps_total: 1,
            results: Vec::new(),
            failure: code.map(|code| SkillRunFailure {
                step_index: 0,
                step_id: "step".to_owned(),
                server_id: "server".to_owned(),
                tool_name: "tool".to_owned(),
                error: RuntimeError::new(code, "skill_run", "safe failure"),
            }),
        }
    }
}
