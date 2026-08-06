use std::{io::Write as _, process::ExitCode};

use clap::Parser as _;
use mcp_host::cli::{Cli, Command, ExitCode as CliExitCode};
use mcp_host::commands::{dispatch, exit_code_for_error, write_value};
use serde_json::json;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let cli = Cli::parse();
    let json_output = cli.json;
    let is_mcp_bridge = matches!(&cli.command, Command::Mcp(_));

    match dispatch(cli).await {
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

fn report_error(error: &mcp_host_core::RuntimeError, json_output: bool) {
    if json_output && write_value(&json!({ "error": error }), true).is_ok() {
        return;
    }

    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(stderr, "{error}");
}

fn process_exit_code(exit_code: CliExitCode) -> ExitCode {
    ExitCode::from(exit_code as u8)
}
