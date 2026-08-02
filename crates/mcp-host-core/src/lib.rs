//! Domain and runtime services for Dynamic MCP Host.

pub mod environment;
pub mod lifecycle;
pub mod loader;
pub mod manifest;
pub mod policy;
pub mod registry;
pub mod runtime;
pub mod skill;
mod validation;

pub use environment::{EnvironmentAccessError, EnvironmentProvider, ProcessEnvironment};
pub use lifecycle::{
    ConnectDisposition, DesiredConnection, DisconnectDisposition, Lifecycle, LifecycleError,
    LifecycleState,
};
pub use loader::{LoadedManifest, ManifestLoadError, ManifestLoader};
pub use manifest::{
    OAuthConfig, PackageProvider, ProvisionConfig, ReconnectConfig, ResolvedServerManifest,
    ResolvedTransportConfig, SecretValue, ServerId, ServerIdError, ServerManifest, TransportConfig,
};
pub use policy::{Policy, PolicyAction, PolicyDecision, PolicyError};
pub use registry::{McpServerRegistry, RegisteredServer, RegistryBuildError, RegistryBuilder};
pub use runtime::{
    AuthLoginStartResult, AuthStatusResult, BatchToolCall, BatchToolCallOutcome,
    BatchToolCallResponse, BatchToolCallResult, CONTROL_PROTOCOL_VERSION, ConnectResult,
    ControlRequest, ControlRequestEnvelope, ControlResponseEnvelope, DisconnectResult, HostStatus,
    MAX_BATCH_CALLS, PackageInstallResult, RuntimeError, RuntimeErrorCode, ServerInspection,
    ServerSummary, SkillRunFailure, SkillRunResult, SkillRunStatus, SkillStepResult,
    ToolCallResult, ToolDefinition, ToolSnapshot, TransportKind,
};
pub use skill::{
    MAX_SKILL_STEPS, RuntimeSkill, SkillCatalog, SkillInput, SkillInputSummary, SkillInputType,
    SkillLoadError, SkillReferenceError, SkillStep, SkillSummary, SkillTemplateError,
    SkillTemplatePart, SkillTemplateReference, SkillValidationError, parse_skill_template,
};
pub use validation::{EnvironmentResolutionError, ManifestValidationError};
