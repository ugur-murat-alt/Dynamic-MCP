//! Inbound MCP server adapter for Dynamic MCP Host.

mod auth;
mod envelope;
pub mod fixture;
pub mod host;
pub mod package;
pub mod runtime;
mod skill;

pub use host::{DownstreamSessionGuard, HostMcpServer, HostRuntimeState};
pub use runtime::{RuntimeManager, RuntimeSettings};
