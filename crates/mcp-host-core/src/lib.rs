//! Domain and runtime services for Dynamic MCP Host.

pub mod environment;
pub mod lifecycle;
pub mod loader;
pub mod manifest;
pub mod registry;
pub mod runtime;
mod validation;

pub use environment::{EnvironmentAccessError, EnvironmentProvider, ProcessEnvironment};
pub use lifecycle::{
    ConnectDisposition, DesiredConnection, DisconnectDisposition, Lifecycle, LifecycleError,
    LifecycleState,
};
pub use loader::{LoadedManifest, ManifestLoadError, ManifestLoader};
pub use manifest::{
    ResolvedServerManifest, ResolvedTransportConfig, SecretValue, ServerId, ServerIdError,
    ServerManifest, TransportConfig,
};
pub use registry::{McpServerRegistry, RegisteredServer, RegistryBuildError, RegistryBuilder};
pub use runtime::{
    CONTROL_PROTOCOL_VERSION, ConnectResult, ControlRequest, ControlRequestEnvelope,
    ControlResponseEnvelope, DisconnectResult, HostStatus, RuntimeError, RuntimeErrorCode,
    ServerInspection, ServerSummary, ToolCallResult, ToolDefinition, ToolSnapshot, TransportKind,
};
pub use validation::{EnvironmentResolutionError, ManifestValidationError};
