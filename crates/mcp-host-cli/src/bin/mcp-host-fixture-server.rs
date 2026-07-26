use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use mcp_host_mcp::fixture::{FixtureOptions, run_stdio_fixture};

#[derive(Debug, Parser)]
struct Arguments {
    #[arg(long)]
    startup_counter_file: Option<PathBuf>,
    #[arg(long)]
    pid_file: Option<PathBuf>,
    #[arg(long, default_value_t = 0)]
    initialize_delay_ms: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Arguments::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fixture server failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(arguments: Arguments) -> Result<(), Box<dyn std::error::Error>> {
    run_stdio_fixture(FixtureOptions {
        startup_counter_file: arguments.startup_counter_file,
        pid_file: arguments.pid_file,
        initialize_delay_ms: arguments.initialize_delay_ms,
    })
    .await
}
