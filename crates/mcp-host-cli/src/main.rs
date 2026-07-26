use std::{
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Parser as _;
use directories::ProjectDirs;
use mcp_host::cli::{Call, Cli, Command, DaemonCommand, ExitCode as CliExitCode, HarnessCommand};
use mcp_host::ipc::send_control;
use mcp_host::{DaemonOptions, install_harnesses, run_daemon, run_stdio_bridge};
use mcp_host_core::{ControlRequest, RuntimeError, RuntimeErrorCode};
use serde_json::{Value, json};

const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(65);
const MCP_BRIDGE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> std::process::ExitCode {
    init_tracing();

    let cli = Cli::parse();
    let json_output = cli.json;
    let is_mcp_bridge = matches!(&cli.command, Command::Mcp);

    match execute(cli).await {
        // Tokio's stdin reader uses an uncancellable blocking read. The bridge has already
        // flushed protocol output and joined its socket copy tasks, so terminate this short-lived
        // shim without waiting for the runtime's blocking pool when the daemon closes first.
        Ok(exit_code) if is_mcp_bridge => std::process::exit(exit_code as i32),
        Ok(exit_code) => process_exit_code(exit_code),
        Err(error) => {
            // The MCP bridge reserves stdout for protocol bytes, including on failure.
            report_error(&error, json_output && !is_mcp_bridge);
            process_exit_code(exit_code_for_error(&error))
        }
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn execute(cli: Cli) -> Result<CliExitCode, RuntimeError> {
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
        command => command,
    };
    let runtime_dir = resolve_runtime_dir(runtime_dir)?;
    let control_timeout = timeout.map_or(DEFAULT_CONTROL_TIMEOUT, Duration::from_millis);

    match command {
        Command::Daemon(daemon) => match daemon.command {
            DaemonCommand::Run(run) => {
                run_daemon(DaemonOptions {
                    config_dir: run.config_dir,
                    runtime_dir,
                })
                .await?;
                Ok(CliExitCode::Success)
            }
            DaemonCommand::Status => {
                execute_control(&runtime_dir, ControlRequest::Status, control_timeout, json).await
            }
            DaemonCommand::Stop => {
                execute_control(
                    &runtime_dir,
                    ControlRequest::Shutdown,
                    control_timeout,
                    json,
                )
                .await
            }
        },
        Command::List => {
            execute_control(
                &runtime_dir,
                ControlRequest::ListServers,
                control_timeout,
                json,
            )
            .await
        }
        Command::Inspect { server_id } => {
            execute_control(
                &runtime_dir,
                ControlRequest::InspectServer { server_id },
                control_timeout,
                json,
            )
            .await
        }
        Command::Connect { server_id } => {
            execute_control(
                &runtime_dir,
                ControlRequest::ConnectServer { server_id },
                control_timeout,
                json,
            )
            .await
        }
        Command::Disconnect { server_id } => {
            execute_control(
                &runtime_dir,
                ControlRequest::DisconnectServer { server_id },
                control_timeout,
                json,
            )
            .await
        }
        Command::Tools { server_id, refresh } => {
            execute_control(
                &runtime_dir,
                ControlRequest::ListTools { server_id, refresh },
                control_timeout,
                json,
            )
            .await
        }
        Command::Refresh { server_id } => {
            execute_control(
                &runtime_dir,
                ControlRequest::RefreshServer { server_id },
                control_timeout,
                json,
            )
            .await
        }
        Command::Call(call) => {
            let arguments = read_call_arguments(&call).await?;
            let result = send_control(
                &runtime_dir,
                &ControlRequest::CallTool {
                    server_id: call.server_id,
                    tool_name: call.tool_name,
                    arguments,
                    timeout_ms: timeout,
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
        Command::Status => {
            execute_control(&runtime_dir, ControlRequest::Status, control_timeout, json).await
        }
        Command::Harness(_) => unreachable!("harness commands return before runtime resolution"),
        Command::Mcp => {
            run_stdio_bridge(&runtime_dir, MCP_BRIDGE_CONNECT_TIMEOUT).await?;
            Ok(CliExitCode::Success)
        }
    }
}

async fn execute_control(
    runtime_dir: &Path,
    request: ControlRequest,
    timeout: Duration,
    json_output: bool,
) -> Result<CliExitCode, RuntimeError> {
    let result = send_control(runtime_dir, &request, timeout).await?;
    write_value(&result, json_output)?;
    Ok(CliExitCode::Success)
}

fn resolve_runtime_dir(runtime_dir: Option<PathBuf>) -> Result<PathBuf, RuntimeError> {
    if let Some(runtime_dir) = runtime_dir {
        return Ok(runtime_dir);
    }

    #[cfg(unix)]
    if let Some(runtime_dir) =
        std::env::var_os("XDG_RUNTIME_DIR").filter(|runtime_dir| !runtime_dir.is_empty())
    {
        return Ok(PathBuf::from(runtime_dir).join("mcp-host"));
    }

    ProjectDirs::from("org", "mcp-host", "mcp-host")
        .map(|directories| directories.data_local_dir().join("runtime"))
        .ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ProtocolError,
                "runtime_dir",
                "could not determine a local runtime directory",
            )
        })
}

async fn read_call_arguments(call: &Call) -> Result<Value, RuntimeError> {
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

fn invalid_arguments_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::InvalidArguments,
        "call_arguments",
        "tool arguments must be a JSON object",
    )
}

fn write_value(value: &Value, json_output: bool) -> Result<(), RuntimeError> {
    let rendered = if json_output {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    }
    .map_err(|_| output_error())?;

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{rendered}").map_err(|_| output_error())
}

fn report_error(error: &RuntimeError, json_output: bool) {
    if json_output && write_value(&json!({ "error": error }), true).is_ok() {
        return;
    }

    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(stderr, "{error}");
}

fn output_error() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::ProtocolError,
        "output",
        "failed to write command output",
    )
}

fn exit_code_for_error(error: &RuntimeError) -> CliExitCode {
    match error.code {
        RuntimeErrorCode::IpcUnavailable | RuntimeErrorCode::DaemonNotRunning => {
            CliExitCode::DaemonUnavailable
        }
        _ => CliExitCode::RuntimeFailure,
    }
}

fn process_exit_code(exit_code: CliExitCode) -> std::process::ExitCode {
    std::process::ExitCode::from(exit_code as u8)
}
