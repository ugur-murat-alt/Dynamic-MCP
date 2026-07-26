//! Inbound MCP server adapter for Dynamic MCP Host.

pub mod fixture;
pub mod host;
pub mod runtime;

pub use host::{DownstreamSessionGuard, HostMcpServer, HostRuntimeState};
pub use runtime::{RuntimeManager, RuntimeSettings};
