pub mod bridge;
pub mod cli;
pub mod commands;
pub mod daemon;
pub mod doctor;
pub mod harness;
mod harness_config;
mod harness_files;
mod harness_paths;
pub mod ipc;
pub mod output;
pub mod service;
pub mod service_manager;
pub mod shell;
#[cfg(windows)]
mod windows_service_backend;
#[cfg(windows)]
pub use windows_service_backend::run_dispatcher as run_windows_service;

pub use bridge::{run_stdio_bridge, run_stdio_bridge_at};
pub use daemon::{DaemonMetadata, DaemonOptions, run_daemon, run_daemon_with_shutdown};
pub use harness::install_harnesses;
