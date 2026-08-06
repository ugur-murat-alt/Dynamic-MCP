//! Runtime management for outbound MCP client sessions.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    process::Stdio,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::future::join_all;
use http::{HeaderName, HeaderValue};
use mcp_host_core::{
    AuthLoginStartResult, AuthStatusResult, BatchToolCall, BatchToolCallOutcome,
    BatchToolCallResponse, BatchToolCallResult, CallPolicy, ConnectDisposition, ConnectResult,
    DesiredConnection, DisconnectDisposition, DisconnectResult, Lifecycle, LifecycleState,
    MAX_BATCH_CALLS, McpServerRegistry, PackageInstallResult, Policy, PolicyAction, PolicyDecision,
    ReconnectConfig, RegisteredServer, ResolvedTransportConfig, RuntimeError, RuntimeErrorCode,
    ServerId, ServerInspection, ServerSummary, SkillCatalog, SkillRunResult, SkillRunStatus,
    SkillSummary, ToolCallResult, ToolDefinition, ToolSnapshot, ToolSuggestion, TransportKind,
    truncate_json,
};
use process_wrap::tokio::CommandWrap;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use rmcp::transport::auth::AuthClient;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{
    ClientHandler, Peer, ServiceError, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, ClientInfo, ClientRequest, GetPromptRequestParams,
        ProtocolVersion, ReadResourceRequestParams, ServerInfo, ServerResult, Tool,
    },
    service::{PeerRequestOptions, RoleClient, RunningService},
    transport::{StreamableHttpClientTransport, TokioChildProcess},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, Notify, RwLock},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::{auth::ServerAuth, package::PackageInstaller, skill::SkillEngine};

static NEXT_INVOCATION_ID: AtomicU64 = AtomicU64::new(1);

/// Durable, per-server usage memory.
///
/// Aggregates in memory and appends one JSONL event per tool call when a
/// journal root is configured, so a restarted daemon remembers which servers
/// a project has used.
#[derive(Debug, Default)]
struct UsageTracker {
    inner: RwLock<BTreeMap<String, ServerUsage>>,
    journal: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ServerUsage {
    call_count: u64,
    error_count: u64,
    last_used_at_unix_ms: Option<u64>,
    projects: BTreeSet<String>,
}

impl UsageTracker {
    fn load(root: Option<&std::path::Path>) -> Self {
        let journal = root.map(|directory| directory.join("usage.jsonl"));
        let mut inner: BTreeMap<String, ServerUsage> = BTreeMap::new();
        if let Some(path) = &journal
            && let Ok(lines) = std::fs::read_to_string(path)
        {
            for line in lines.lines() {
                let Ok(event) = serde_json::from_str::<UsageEvent>(line) else {
                    continue;
                };
                let usage = inner.entry(event.server_id.clone()).or_default();
                if event.kind == UsageKind::Call {
                    usage.call_count = usage.call_count.saturating_add(1);
                    if !event.success {
                        usage.error_count = usage.error_count.saturating_add(1);
                    }
                    if usage.last_used_at_unix_ms.is_none_or(|ts| event.ts > ts) {
                        usage.last_used_at_unix_ms = Some(event.ts);
                    }
                }
                if let Some(project) = event.project {
                    usage.projects.insert(project);
                }
                while usage.projects.len() > MAX_TRACKED_PROJECTS {
                    usage.projects.pop_first();
                }
            }
        }
        Self {
            inner: RwLock::new(inner),
            journal,
        }
    }

    async fn record_call(&self, server_id: &str, success: bool) {
        self.append(UsageEvent {
            ts: unix_ms(),
            kind: UsageKind::Call,
            server_id: server_id.to_owned(),
            success,
            project: None,
        })
        .await;
    }

    async fn record_project(&self, server_id: &str, project: &str) {
        self.append(UsageEvent {
            ts: unix_ms(),
            kind: UsageKind::Connect,
            server_id: server_id.to_owned(),
            success: true,
            project: Some(project.to_owned()),
        })
        .await;
    }

    async fn append(&self, event: UsageEvent) {
        {
            let mut inner = self.inner.write().await;
            let usage = inner.entry(event.server_id.clone()).or_default();
            if event.kind == UsageKind::Call {
                usage.call_count = usage.call_count.saturating_add(1);
                if !event.success {
                    usage.error_count = usage.error_count.saturating_add(1);
                }
                usage.last_used_at_unix_ms = Some(event.ts);
            }
            if let Some(project) = &event.project {
                usage.projects.insert(project.clone());
                while usage.projects.len() > MAX_TRACKED_PROJECTS {
                    usage.projects.pop_first();
                }
            }
        }
        let Some(journal) = &self.journal else {
            return;
        };
        let Some(line) = serde_json::to_string(&event).ok() else {
            return;
        };
        let journal = journal.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(parent) = journal.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&journal)
            {
                let _ = writeln!(file, "{line}");
            }
        })
        .await;
    }

    async fn snapshot(&self) -> BTreeMap<String, ServerUsage> {
        self.inner.read().await.clone()
    }
}

const MAX_TRACKED_PROJECTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UsageKind {
    Call,
    Connect,
}

#[derive(Debug, Serialize, Deserialize)]
struct UsageEvent {
    ts: u64,
    kind: UsageKind,
    server_id: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

/// Runtime limits for downstream MCP servers.
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    /// Maximum time allowed for a transport to connect and initialize.
    pub connect_timeout: Duration,
    /// Default timeout for an individual tool request.
    pub request_timeout: Duration,
    /// Maximum time allowed for graceful session shutdown.
    pub shutdown_grace: Duration,
    /// Largest caller-supplied tool request timeout.
    pub max_request_timeout: Duration,
    /// Number of latest stderr bytes retained for internal diagnostics.
    pub stderr_tail_bytes: usize,
    /// Root directory for explicit downstream package installations.
    pub package_root: Option<std::path::PathBuf>,
    /// Root directory for persistent OAuth credentials.
    pub auth_root: Option<std::path::PathBuf>,
    /// Root directory for the durable usage journal (`usage.jsonl`).
    pub usage_root: Option<std::path::PathBuf>,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(4),
            max_request_timeout: Duration::from_secs(300),
            stderr_tail_bytes: 8_192,
            package_root: None,
            auth_root: None,
            usage_root: None,
        }
    }
}

/// Result of atomically reconciling a newly loaded registry snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryReloadResult {
    pub generation: u64,
    pub added: u64,
    pub removed: u64,
    pub changed: u64,
    pub policy_changed: bool,
    pub skills_changed: bool,
}

/// Owns the independent runtimes associated with the current registry snapshot.
pub struct RuntimeManager {
    catalog: RwLock<RuntimeCatalog>,
    settings: RuntimeSettings,
    generation: AtomicU64,
    server_count: AtomicU64,
    shutdown: CancellationToken,
    package_installer: Option<PackageInstaller>,
    usage: UsageTracker,
}

struct RuntimeCatalog {
    registry: Arc<McpServerRegistry>,
    servers: BTreeMap<String, Arc<ServerRuntime>>,
    policy: Arc<Policy>,
    skills: Arc<SkillCatalog>,
}

impl RuntimeManager {
    /// Creates one runtime state machine per registered server.
    #[must_use]
    pub fn new(registry: Arc<McpServerRegistry>, settings: RuntimeSettings) -> Arc<Self> {
        Self::new_with_configuration(
            registry,
            settings,
            Policy::default(),
            SkillCatalog::default(),
        )
    }

    /// Creates runtime state machines with an explicit global policy snapshot.
    #[must_use]
    pub fn new_with_policy(
        registry: Arc<McpServerRegistry>,
        settings: RuntimeSettings,
        policy: Policy,
    ) -> Arc<Self> {
        Self::new_with_configuration(registry, settings, policy, SkillCatalog::default())
    }

    /// Creates runtime state machines with explicit policy and skill snapshots.
    #[must_use]
    pub fn new_with_configuration(
        registry: Arc<McpServerRegistry>,
        settings: RuntimeSettings,
        policy: Policy,
        skills: SkillCatalog,
    ) -> Arc<Self> {
        let shutdown = CancellationToken::new();
        let package_installer = settings.package_root.clone().map(PackageInstaller::new);
        let servers: BTreeMap<String, Arc<ServerRuntime>> = registry
            .iter()
            .map(|server| {
                let id = server.id().as_str().to_owned();
                (
                    id.clone(),
                    Arc::new(ServerRuntime::new(
                        id,
                        server.clone(),
                        settings.clone(),
                        shutdown.clone(),
                    )),
                )
            })
            .collect();
        let server_count = servers.len() as u64;
        let usage = UsageTracker::load(settings.usage_root.as_deref());
        Arc::new(Self {
            catalog: RwLock::new(RuntimeCatalog {
                registry,
                servers,
                policy: Arc::new(policy),
                skills: Arc::new(skills),
            }),
            settings,
            generation: AtomicU64::new(1),
            server_count: AtomicU64::new(server_count),
            shutdown,
            package_installer,
            usage,
        })
    }

    /// Returns the current immutable registry snapshot.
    pub async fn registry(&self) -> Arc<McpServerRegistry> {
        Arc::clone(&self.catalog.read().await.registry)
    }

    /// Returns the current registry generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Atomically publishes and reconciles a fully validated registry snapshot.
    pub async fn reload_registry(
        self: &Arc<Self>,
        registry: Arc<McpServerRegistry>,
    ) -> RegistryReloadResult {
        let (policy, skills) = {
            let catalog = self.catalog.read().await;
            (Arc::clone(&catalog.policy), Arc::clone(&catalog.skills))
        };
        self.reload_configuration(registry, policy.as_ref().clone(), skills.as_ref().clone())
            .await
    }

    /// Atomically publishes registry and policy snapshots before reconciliation.
    pub async fn reload_configuration(
        self: &Arc<Self>,
        registry: Arc<McpServerRegistry>,
        policy: Policy,
        skills: SkillCatalog,
    ) -> RegistryReloadResult {
        let mut retired = Vec::new();
        let mut replaced = Vec::new();
        let (added, removed, changed, policy_changed, skills_changed) = {
            let mut catalog = self.catalog.write().await;
            let policy_changed = catalog.policy.as_ref() != &policy;
            let skills_changed = catalog.skills.as_ref() != &skills;
            let mut previous = std::mem::take(&mut catalog.servers);
            let mut next = BTreeMap::new();
            let mut added = 0_u64;
            let mut changed = 0_u64;

            for server in registry.iter() {
                let id = server.id().as_str().to_owned();
                match previous.remove(&id) {
                    Some(runtime) if runtime.registered.raw_manifest() == server.raw_manifest() => {
                        next.insert(id, runtime);
                    }
                    Some(runtime) => {
                        changed = changed.saturating_add(1);
                        let replacement = Arc::new(ServerRuntime::new(
                            id.clone(),
                            server.clone(),
                            self.settings.clone(),
                            self.shutdown.clone(),
                        ));
                        replaced.push((runtime, Arc::clone(&replacement)));
                        next.insert(id, replacement);
                    }
                    None => {
                        added = added.saturating_add(1);
                        next.insert(
                            id.clone(),
                            Arc::new(ServerRuntime::new(
                                id,
                                server.clone(),
                                self.settings.clone(),
                                self.shutdown.clone(),
                            )),
                        );
                    }
                }
            }

            let removed = previous.len() as u64;
            retired.extend(previous.into_values());
            if added == 0 && removed == 0 && changed == 0 && !policy_changed && !skills_changed {
                catalog.registry = registry;
                catalog.servers = next;
                return RegistryReloadResult {
                    generation: self.generation(),
                    added,
                    removed,
                    changed,
                    policy_changed,
                    skills_changed,
                };
            }
            catalog.registry = registry;
            catalog.servers = next;
            catalog.policy = Arc::new(policy);
            catalog.skills = Arc::new(skills);
            self.server_count
                .store(catalog.servers.len() as u64, Ordering::Release);
            (added, removed, changed, policy_changed, skills_changed)
        };

        for runtime in retired {
            let _ = runtime.disconnect().await;
        }
        for (old, replacement) in replaced {
            let reconnect = old.desired().await == DesiredConnection::Connected;
            let _ = old.disconnect().await;
            if reconnect && replacement.registered.enabled() {
                let _ = replacement.connect().await;
            }
        }

        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        RegistryReloadResult {
            generation,
            added,
            removed,
            changed,
            policy_changed,
            skills_changed,
        }
    }

    /// Lists servers in normalized ID order, merged with durable usage memory.
    pub async fn list_servers(&self) -> Vec<ServerSummary> {
        let servers = self.runtimes().await;
        let policy = self.policy().await;
        let usage = self.usage.snapshot().await;
        let futures = servers
            .iter()
            .filter(|runtime| {
                policy.check(PolicyAction::List, &runtime.id, None) == PolicyDecision::Allow
            })
            .map(|runtime| {
                let usage = usage.get(&runtime.id).cloned().unwrap_or_default();
                async move {
                    let mut summary = runtime.summary().await;
                    summary.use_count = Some(usage.call_count);
                    summary.error_count = Some(usage.error_count);
                    summary.last_used_at_unix_ms = usage.last_used_at_unix_ms;
                    summary.projects = usage.projects.into_iter().rev().collect::<Vec<_>>();
                    summary
                }
            });
        join_all(futures).await
    }

    /// Total recorded downstream tool calls across all servers.
    pub async fn total_tool_calls(&self) -> u64 {
        self.usage
            .snapshot()
            .await
            .values()
            .map(|usage| usage.call_count)
            .sum()
    }

    /// Returns public, secret-free metadata and current runtime state for a server.
    pub async fn inspect_server(&self, server_id: &str) -> Result<ServerInspection, RuntimeError> {
        let runtime = self.server(server_id, "inspect_server").await?;
        self.authorize(PolicyAction::Inspect, &runtime.id, None, "inspect_server")
            .await?;
        runtime.inspection().await
    }

    /// Connects, initializes, and discovers all tools for a server.
    ///
    /// When `project` is supplied, the association is stored in durable usage
    /// memory so later sessions can see which projects used this server.
    pub async fn connect_server(
        &self,
        server_id: &str,
        project: Option<&str>,
    ) -> Result<ConnectResult, RuntimeError> {
        let started = Instant::now();
        let runtime = self.server(server_id, "connect_server").await?;
        self.authorize(PolicyAction::Connect, &runtime.id, None, "connect_server")
            .await?;
        let result = runtime.connect().await;
        if result.is_ok()
            && let Some(project) = project
        {
            self.usage.record_project(server_id, project).await;
        }
        trace_operation("connect_server", server_id, started, &result);
        result
    }

    /// Gracefully disconnects a server, cancelling an in-progress startup if necessary.
    pub async fn disconnect_server(
        &self,
        server_id: &str,
    ) -> Result<DisconnectResult, RuntimeError> {
        let started = Instant::now();
        let runtime = self.server(server_id, "disconnect_server").await?;
        self.authorize(
            PolicyAction::Disconnect,
            &runtime.id,
            None,
            "disconnect_server",
        )
        .await?;
        let result = runtime.disconnect().await;
        trace_operation("disconnect_server", server_id, started, &result);
        result
    }

    /// Returns the cached tools, optionally refreshing the cache from the server.
    pub async fn list_tools(
        &self,
        server_id: &str,
        refresh: bool,
    ) -> Result<ToolSnapshot, RuntimeError> {
        let runtime = self.server(server_id, "list_tools").await?;
        self.authorize(
            if refresh {
                PolicyAction::Refresh
            } else {
                PolicyAction::List
            },
            &runtime.id,
            None,
            "list_tools",
        )
        .await?;
        if refresh {
            return runtime.refresh().await;
        }
        runtime.cached_tools().await
    }

    /// Refreshes the discovered tool cache.
    pub async fn refresh_server(&self, server_id: &str) -> Result<ToolSnapshot, RuntimeError> {
        let started = Instant::now();
        let runtime = self.server(server_id, "refresh_server").await?;
        self.authorize(PolicyAction::Refresh, &runtime.id, None, "refresh_server")
            .await?;
        let result = runtime.refresh().await;
        trace_operation("refresh_server", server_id, started, &result);
        result
    }

    /// Calls a discovered tool with object arguments.
    ///
    /// Applies [`CallPolicy`]: implicit connection, a single refresh-retry
    /// recovery pass, did-you-mean suggestions for persistent misses, and an
    /// optional output token budget. Records usage memory for the server.
    pub async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        arguments: Value,
        timeout_ms: Option<u64>,
        policy: CallPolicy,
    ) -> Result<ToolCallResult, RuntimeError> {
        let started = Instant::now();
        let invocation_id = NEXT_INVOCATION_ID.fetch_add(1, Ordering::Relaxed);
        let argument_bytes = serde_json::to_vec(&arguments).map_or(0, |value| value.len());
        let runtime = self.server(server_id, "call_tool").await?;
        self.authorize(
            PolicyAction::Call,
            &runtime.id,
            Some(tool_name),
            "call_tool",
        )
        .await?;
        if !arguments.is_object() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidArguments,
                "call_tool",
                "tool arguments must be a JSON object",
            ));
        }
        if policy.auto_connect && runtime.state().await != LifecycleState::Connected {
            self.authorize(PolicyAction::Connect, &runtime.id, None, "auto_connect")
                .await?;
            runtime.connect().await?;
        }
        let mut result = runtime
            .call_tool(tool_name, arguments.clone(), timeout_ms)
            .await;
        if policy.auto_retry
            && matches!(
                result,
                Err(RuntimeError {
                    code: RuntimeErrorCode::ToolNotFound | RuntimeErrorCode::ToolsNotDiscovered,
                    ..
                })
            )
        {
            if runtime.refresh().await.is_ok() {
                result = runtime
                    .call_tool(tool_name, arguments.clone(), timeout_ms)
                    .await;
            }
            if let Err(RuntimeError {
                code: RuntimeErrorCode::ToolNotFound,
                ..
            }) = &result
            {
                let suggestions = self
                    .search_tools(tool_name, Some(&runtime.id), MAX_TOOL_SUGGESTIONS)
                    .await;
                if !suggestions.is_empty() {
                    result = result.map_err(|error| {
                        error.with_suggestions(suggestions).with_source_summary(
                            "tool was refreshed and retried once before suggesting close names",
                        )
                    });
                }
            }
        }
        if let Ok(call) = &result
            && let Some(max_tokens) = policy.max_output_tokens
            && let Some(truncated) = truncate_json(call.value(), max_tokens)
        {
            result = Ok(ToolCallResult::new(truncated));
        }
        let result_bytes = result
            .as_ref()
            .ok()
            .and_then(|result| serde_json::to_vec(result.value()).ok())
            .map_or(0, |value| value.len());
        let error_code = result
            .as_ref()
            .err()
            .map_or("", |error| error.code.as_str());
        tracing::info!(
            operation = "call_tool",
            server_id,
            tool_name,
            invocation_id,
            argument_bytes,
            result_bytes,
            duration_ms = duration_ms(started.elapsed()),
            success = result.is_ok(),
            auto_connect = policy.auto_connect,
            auto_retry = policy.auto_retry,
            error_code
        );
        self.usage.record_call(&runtime.id, result.is_ok()).await;
        result
    }

    /// Searches cached tool definitions across managed servers.
    ///
    /// Exact names rank first, then prefixes, then substrings, then
    /// multi-word containment. Results respect the list policy.
    pub async fn search_tools(
        &self,
        query: &str,
        server_id: Option<&str>,
        limit: usize,
    ) -> Vec<ToolSuggestion> {
        let normalized = normalize_query(query);
        if normalized.is_empty() {
            return Vec::new();
        }
        let (servers, policy) = {
            let catalog = self.catalog.read().await;
            (
                catalog.servers.values().cloned().collect::<Vec<_>>(),
                Arc::clone(&catalog.policy),
            )
        };
        let mut matches = Vec::new();
        for runtime in servers {
            if let Some(scope) = server_id
                && runtime.id != scope
            {
                continue;
            }
            if policy.check(PolicyAction::List, &runtime.id, None) != PolicyDecision::Allow {
                continue;
            }
            let Some(tools) = runtime.cached_tool_names().await else {
                continue;
            };
            for (name, description) in tools {
                if let Some(score) = score_tool_name(&normalized, &name) {
                    matches.push((
                        score,
                        name.clone(),
                        ToolSuggestion {
                            server_id: runtime.id.clone(),
                            tool_name: name,
                            description: description.map(|text| {
                                let mut truncated: String = text.chars().take(200).collect();
                                if text.chars().count() > 200 {
                                    truncated.push('…');
                                }
                                truncated
                            }),
                        },
                    ));
                }
            }
        }
        matches.sort_by(|(a_score, a_name, _), (b_score, b_name, _)| {
            a_score.cmp(b_score).then_with(|| a_name.cmp(b_name))
        });
        matches
            .into_iter()
            .take(limit)
            .map(|(_, _, suggestion)| suggestion)
            .collect()
    }

    /// Calls up to 32 discovered tools concurrently while preserving input order.
    pub async fn call_tools(
        &self,
        calls: Vec<BatchToolCall>,
    ) -> Result<BatchToolCallResponse, RuntimeError> {
        if calls.is_empty() || calls.len() > MAX_BATCH_CALLS {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidArguments,
                "call_tools",
                format!("batch must contain between 1 and {MAX_BATCH_CALLS} tool calls"),
            ));
        }

        let results = join_all(calls.into_iter().map(|call| async move {
            let BatchToolCall {
                server_id,
                tool_name,
                arguments,
                timeout_ms,
                call_policy,
            } = call;
            let outcome = match self
                .call_tool(&server_id, &tool_name, arguments, timeout_ms, call_policy)
                .await
            {
                Ok(result) => BatchToolCallOutcome::Success { result },
                Err(error) => BatchToolCallOutcome::Error { error },
            };
            BatchToolCallResult {
                server_id,
                tool_name,
                outcome,
            }
        }))
        .await;

        Ok(BatchToolCallResponse { results })
    }

    /// Lists runtime resources advertised by a connected server.
    pub async fn list_resources(&self, server_id: &str) -> Result<Value, RuntimeError> {
        let runtime = self.server(server_id, "list_resources").await?;
        self.authorize(PolicyAction::List, &runtime.id, None, "list_resources")
            .await?;
        runtime.list_resources().await
    }

    /// Reads one resource from a connected server.
    pub async fn read_resource(&self, server_id: &str, uri: &str) -> Result<Value, RuntimeError> {
        let runtime = self.server(server_id, "read_resource").await?;
        self.authorize(PolicyAction::List, &runtime.id, None, "read_resource")
            .await?;
        runtime.read_resource(uri).await
    }

    /// Lists prompts advertised by a connected server.
    pub async fn list_prompts(&self, server_id: &str) -> Result<Value, RuntimeError> {
        let runtime = self.server(server_id, "list_prompts").await?;
        self.authorize(PolicyAction::List, &runtime.id, None, "list_prompts")
            .await?;
        runtime.list_prompts().await
    }

    /// Invokes a prompt on a connected server with object arguments.
    pub async fn call_prompt(
        &self,
        server_id: &str,
        prompt_name: &str,
        arguments: Map<String, Value>,
    ) -> Result<Value, RuntimeError> {
        let runtime = self.server(server_id, "call_prompt").await?;
        self.authorize(
            PolicyAction::Call,
            &runtime.id,
            Some(prompt_name),
            "call_prompt",
        )
        .await?;
        runtime.get_prompt(prompt_name, arguments).await
    }

    /// Returns whether a connected server advertises the given capability key
    /// (for example "resources" or "prompts") in its negotiated capabilities.
    pub async fn supports_capability(&self, server_id: &str, key: &str) -> bool {
        let Ok(runtime) = self.server(server_id, "capabilities").await else {
            return false;
        };
        runtime.upstream_supports(key).await
    }

    /// Lists runtime skills allowed by the current skill policy.
    pub async fn list_skills(&self) -> Vec<SkillSummary> {
        let catalog = self.catalog.read().await;
        catalog
            .skills
            .iter()
            .filter(|skill| catalog.policy.check_skill(skill.id()) == PolicyDecision::Allow)
            .map(|skill| skill.summary())
            .collect()
    }

    /// Runs one immutable skill snapshot sequentially and stops at its first failed step.
    pub async fn run_skill(
        &self,
        skill_id: &str,
        inputs: Value,
    ) -> Result<SkillRunResult, RuntimeError> {
        let started = Instant::now();
        let input_bytes = serde_json::to_vec(&inputs).map_or(0, |encoded| encoded.len());
        let normalized = ServerId::parse(skill_id)
            .map(|id| id.as_str().to_owned())
            .unwrap_or_else(|_| skill_id.to_owned());
        let (skill, policy) = {
            let catalog = self.catalog.read().await;
            (catalog.skills.get(&normalized), Arc::clone(&catalog.policy))
        };
        let skill = skill.ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::SkillNotFound,
                "skill_run",
                "the runtime skill is not registered",
            )
        })?;
        if policy.check_skill(skill.id()) == PolicyDecision::Deny {
            return Err(RuntimeError::new(
                RuntimeErrorCode::PolicyDenied,
                "skill_run",
                "the skill is denied by policy",
            ));
        }

        let result = SkillEngine::run(self, &skill, inputs).await;
        let result_bytes = result
            .as_ref()
            .ok()
            .and_then(|result| serde_json::to_vec(result).ok())
            .map_or(0, |encoded| encoded.len());
        let (success, error_code) = match &result {
            Ok(result) if result.status == SkillRunStatus::Ok => (true, ""),
            Ok(result) => (
                false,
                result
                    .failure
                    .as_ref()
                    .map_or("", |failure| failure.error.code.as_str()),
            ),
            Err(error) => (false, error.code.as_str()),
        };
        tracing::info!(
            operation = "skill_run",
            skill_id = skill.id(),
            input_bytes,
            result_bytes,
            duration_ms = duration_ms(started.elapsed()),
            success,
            error_code
        );
        result
    }

    /// Explicitly installs the package declared by a server manifest.
    pub async fn package_install(
        &self,
        server_id: &str,
    ) -> Result<PackageInstallResult, RuntimeError> {
        let runtime = self.server(server_id, "package_install").await?;
        self.authorize(
            PolicyAction::PackageInstall,
            &runtime.id,
            None,
            "package_install",
        )
        .await?;
        let provision = runtime
            .registered
            .resolved_manifest()
            .provision
            .as_ref()
            .ok_or_else(|| {
                RuntimeError::for_server(
                    RuntimeErrorCode::PackageNotConfigured,
                    "package_install",
                    &runtime.id,
                    "the server manifest has no package configuration",
                )
            })?;
        let installer = self.package_installer.as_ref().ok_or_else(|| {
            RuntimeError::for_server(
                RuntimeErrorCode::PackageInstallFailed,
                "package_install",
                &runtime.id,
                "the package cache is unavailable",
            )
        })?;
        installer.install(&runtime.id, provision).await
    }

    /// Starts an OAuth authorization-code PKCE flow for an HTTP server.
    pub async fn auth_start(
        &self,
        server_id: &str,
        redirect_uri: &str,
    ) -> Result<AuthLoginStartResult, RuntimeError> {
        let runtime = self.server(server_id, "auth_start").await?;
        self.authorize(PolicyAction::AuthStart, &runtime.id, None, "auth_start")
            .await?;
        runtime.auth_start(redirect_uri).await
    }

    /// Completes an in-progress OAuth flow with the loopback callback URL.
    pub async fn auth_complete(
        &self,
        server_id: &str,
        callback_url: &str,
    ) -> Result<AuthStatusResult, RuntimeError> {
        let runtime = self.server(server_id, "auth_complete").await?;
        self.authorize(PolicyAction::AuthStart, &runtime.id, None, "auth_complete")
            .await?;
        runtime.oauth("auth_complete")?.complete(callback_url).await
    }

    /// Returns secret-free OAuth status for one server.
    pub async fn auth_status(&self, server_id: &str) -> Result<AuthStatusResult, RuntimeError> {
        let runtime = self.server(server_id, "auth_status").await?;
        self.authorize(PolicyAction::Inspect, &runtime.id, None, "auth_status")
            .await?;
        runtime.oauth("auth_status")?.status().await
    }

    /// Disconnects a server and clears its locally stored OAuth credentials.
    pub async fn auth_logout(&self, server_id: &str) -> Result<AuthStatusResult, RuntimeError> {
        let runtime = self.server(server_id, "auth_logout").await?;
        self.authorize(PolicyAction::AuthLogout, &runtime.id, None, "auth_logout")
            .await?;
        let auth = runtime.oauth("auth_logout")?;
        let _logout = runtime.begin_auth_logout()?;
        runtime.disconnect().await?;
        auth.logout().await
    }

    /// Gracefully disconnects all registered servers concurrently.
    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.shutdown.cancel();
        let servers = self.runtimes().await;
        for runtime in &servers {
            runtime.cancel_recovery().await;
        }
        let results = join_all(servers.iter().map(|runtime| runtime.disconnect())).await;
        join_all(servers.iter().map(|runtime| runtime.wait_for_recovery())).await;
        if let Some(error) = results.into_iter().find_map(Result::err) {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ShutdownFailed,
                "shutdown",
                "one or more downstream servers did not shut down cleanly",
            )
            .with_source_summary(error.code.as_str()));
        }
        Ok(())
    }

    /// Returns the number of registered servers.
    #[must_use]
    pub fn server_count(&self) -> u64 {
        self.server_count.load(Ordering::Acquire)
    }

    /// Returns the number of currently connected servers.
    pub async fn connected_count(&self) -> u64 {
        let servers = self.runtimes().await;
        join_all(servers.iter().map(|runtime| runtime.state()))
            .await
            .into_iter()
            .filter(|state| *state == LifecycleState::Connected)
            .count() as u64
    }

    /// Returns the number of servers with a recorded runtime failure.
    pub async fn failed_count(&self) -> u64 {
        let servers = self.runtimes().await;
        join_all(servers.iter().map(|runtime| runtime.state()))
            .await
            .into_iter()
            .filter(|state| *state == LifecycleState::Failed)
            .count() as u64
    }

    async fn server(
        &self,
        server_id: &str,
        operation: &str,
    ) -> Result<Arc<ServerRuntime>, RuntimeError> {
        let normalized = ServerId::parse(server_id)
            .map(|id| id.as_str().to_owned())
            .unwrap_or_else(|_| server_id.to_owned());
        self.catalog
            .read()
            .await
            .servers
            .get(&normalized)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::for_server(
                    RuntimeErrorCode::ServerNotFound,
                    operation,
                    server_id,
                    "the server is not registered",
                )
            })
    }

    async fn runtimes(&self) -> Vec<Arc<ServerRuntime>> {
        self.catalog
            .read()
            .await
            .servers
            .values()
            .cloned()
            .collect()
    }

    async fn policy(&self) -> Arc<Policy> {
        Arc::clone(&self.catalog.read().await.policy)
    }

    async fn authorize(
        &self,
        action: PolicyAction,
        server_id: &str,
        tool_name: Option<&str>,
        operation: &str,
    ) -> Result<(), RuntimeError> {
        if self.policy().await.check(action, server_id, tool_name) == PolicyDecision::Allow {
            return Ok(());
        }
        Err(RuntimeError::for_server(
            RuntimeErrorCode::PolicyDenied,
            operation,
            server_id,
            "the operation is denied by policy",
        ))
    }
}

struct ServerRuntime {
    id: String,
    registered: RegisteredServer,
    settings: RuntimeSettings,
    state: Mutex<RuntimeState>,
    changed: Notify,
    tools: Arc<Mutex<Option<ToolSnapshot>>>,
    shutdown: CancellationToken,
    recovery_cancel: Mutex<Option<CancellationToken>>,
    recovery_tasks: AtomicU64,
    recovery_changed: Notify,
    auth: Option<ServerAuth>,
    auth_logout_in_progress: AtomicBool,
}

struct RuntimeState {
    lifecycle: Lifecycle,
    session: Option<ManagedSession>,
    operation: Option<Operation>,
    epoch: u64,
    protocol: Value,
    upstream: Value,
    pid: Option<u32>,
    connected_at_unix_ms: Option<u64>,
    disconnected_at_unix_ms: Option<u64>,
    last_safe_error: Option<RuntimeError>,
}

struct Operation {
    kind: OperationKind,
    epoch: u64,
    cancel: CancellationToken,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OperationKind {
    Connect,
    Disconnect,
}

struct ManagedSession {
    service: RunningService<RoleClient, ClientEvents>,
    close_token: CancellationToken,
    monitor: JoinHandle<()>,
    stderr: Option<JoinHandle<()>>,
    _stderr_tail: Option<Arc<Mutex<VecDeque<u8>>>>,
}

#[derive(Clone)]
struct ClientEvents {
    tools: Arc<Mutex<Option<ToolSnapshot>>>,
}

impl ClientHandler for ClientEvents {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default().with_protocol_version(ProtocolVersion::V_2025_11_25)
    }

    async fn on_tool_list_changed(&self, _context: rmcp::service::NotificationContext<RoleClient>) {
        if let Some(snapshot) = self.tools.lock().await.as_mut() {
            snapshot.stale = true;
        }
    }
}

impl ServerRuntime {
    fn new(
        id: String,
        registered: RegisteredServer,
        settings: RuntimeSettings,
        shutdown: CancellationToken,
    ) -> Self {
        let auth = match (
            registered.resolved_manifest().auth.clone(),
            &registered.resolved_manifest().transport,
            settings.auth_root.as_deref(),
        ) {
            (Some(config), ResolvedTransportConfig::Http { url, .. }, Some(root)) => {
                Some(ServerAuth::new(id.clone(), url.to_string(), config, root))
            }
            _ => None,
        };
        Self {
            id,
            registered,
            settings,
            state: Mutex::new(RuntimeState {
                lifecycle: Lifecycle::default(),
                session: None,
                operation: None,
                epoch: 0,
                protocol: Value::Null,
                upstream: Value::Null,
                pid: None,
                connected_at_unix_ms: None,
                disconnected_at_unix_ms: None,
                last_safe_error: None,
            }),
            changed: Notify::new(),
            tools: Arc::new(Mutex::new(None)),
            shutdown,
            recovery_cancel: Mutex::new(None),
            recovery_tasks: AtomicU64::new(0),
            recovery_changed: Notify::new(),
            auth,
            auth_logout_in_progress: AtomicBool::new(false),
        }
    }

    async fn summary(&self) -> ServerSummary {
        let state = self.state.lock().await;
        let tools = self.tools.lock().await;
        ServerSummary {
            id: self.id.clone(),
            name: self.registered.resolved_manifest().name.clone(),
            description: self.registered.resolved_manifest().description.clone(),
            enabled: self.registered.enabled(),
            transport: transport_kind(&self.registered),
            desired_state: state.lifecycle.desired(),
            observed_state: state.lifecycle.state(),
            tool_count: tools.as_ref().map_or(0, |snapshot| snapshot.tool_count),
            tools_stale: tools.as_ref().is_some_and(|snapshot| snapshot.stale),
            use_count: None,
            error_count: None,
            last_used_at_unix_ms: None,
            projects: Vec::new(),
        }
    }

    async fn inspection(&self) -> Result<ServerInspection, RuntimeError> {
        let state = self.state.lock().await;
        let tools = self.tools.lock().await.clone();
        Ok(ServerInspection {
            server_id: self.id.clone(),
            public_manifest: public_manifest(&self.registered),
            source: self.registered.source_path().display().to_string(),
            transport: transport_kind(&self.registered),
            enabled: self.registered.enabled(),
            desired_state: state.lifecycle.desired(),
            observed_state: state.lifecycle.state(),
            protocol: state.protocol.clone(),
            upstream: state.upstream.clone(),
            tool_snapshot: tools,
            last_safe_error: state.last_safe_error.clone(),
            pid: state.pid,
            connected_at_unix_ms: state.connected_at_unix_ms,
            disconnected_at_unix_ms: state.disconnected_at_unix_ms,
        })
    }

    async fn state(&self) -> LifecycleState {
        self.state.lock().await.lifecycle.state()
    }

    async fn desired(&self) -> DesiredConnection {
        self.state.lock().await.lifecycle.desired()
    }

    async fn connect(self: &Arc<Self>) -> Result<ConnectResult, RuntimeError> {
        self.cancel_recovery().await;
        self.connect_inner().await
    }

    async fn connect_inner(self: &Arc<Self>) -> Result<ConnectResult, RuntimeError> {
        if self.shutdown.is_cancelled() {
            return Err(self.error(
                RuntimeErrorCode::DaemonShuttingDown,
                "connect_server",
                "the daemon is shutting down",
            ));
        }
        if !self.registered.enabled() {
            return Err(self.error(
                RuntimeErrorCode::ServerDisabled,
                "connect_server",
                "the server is disabled",
            ));
        }
        if self.auth_logout_in_progress.load(Ordering::Acquire)
            || matches!(&self.auth, Some(auth) if auth.in_progress().await)
        {
            return Err(self.error(
                RuntimeErrorCode::AuthInProgress,
                "connect_server",
                "an OAuth operation is in progress",
            ));
        }

        loop {
            let action = {
                let mut state = self.state.lock().await;
                if let Some(operation) = &state.operation {
                    ConnectAction::Wait {
                        epoch: operation.epoch,
                        joined_connect: operation.kind == OperationKind::Connect,
                    }
                } else {
                    match state.lifecycle.request_connect() {
                        ConnectDisposition::AlreadyConnected => {
                            return self.connect_result(&state).await;
                        }
                        ConnectDisposition::JoinExisting => ConnectAction::Wait {
                            epoch: state.epoch,
                            joined_connect: true,
                        },
                        ConnectDisposition::Start => {
                            state.epoch = state.epoch.saturating_add(1);
                            let epoch = state.epoch;
                            let cancel = CancellationToken::new();
                            state.operation = Some(Operation {
                                kind: OperationKind::Connect,
                                epoch,
                                cancel: cancel.clone(),
                            });
                            state.last_safe_error = None;
                            ConnectAction::Start {
                                epoch,
                                cancel,
                                old_session: state.session.take(),
                            }
                        }
                    }
                }
            };

            match action {
                ConnectAction::Wait {
                    epoch,
                    joined_connect,
                } => {
                    self.wait_for_operation(epoch).await;
                    if joined_connect {
                        let state = self.state.lock().await;
                        return self.connect_result(&state).await;
                    }
                }
                ConnectAction::Start {
                    epoch,
                    cancel,
                    old_session,
                } => {
                    mark_stale(&self.tools).await;
                    if let Some(session) = old_session {
                        let _ = close_session(session, self.settings.shutdown_grace).await;
                    }
                    let result = self.start_session(cancel, epoch).await;
                    return self.finish_connect(epoch, result).await;
                }
            }
        }
    }

    async fn start_session(
        self: &Arc<Self>,
        cancellation: CancellationToken,
        session_epoch: u64,
    ) -> Result<(ManagedSession, Value, Value, Option<u32>), RuntimeError> {
        let events = ClientEvents {
            tools: Arc::clone(&self.tools),
        };
        let (mut service, pid, mut stderr, stderr_tail) =
            match self.registered.resolved_manifest().transport.clone() {
                ResolvedTransportConfig::Stdio {
                    mut command,
                    args,
                    working_directory,
                    environment,
                } => {
                    if let (Some(root), Some(provision)) = (
                        self.settings.package_root.as_ref(),
                        self.registered.resolved_manifest().provision.as_ref(),
                    ) {
                        let installer = PackageInstaller::new(root.clone());
                        if let Some(installed) = installer.binary_path(&self.id, provision) {
                            command = installed.display().to_string();
                        }
                    }
                    let mut command_line = tokio::process::Command::new(command);
                    command_line.args(args);
                    if let Some(directory) = working_directory {
                        command_line.current_dir(directory);
                    }
                    for (name, value) in &environment {
                        command_line.env(name, value.expose_secret());
                    }
                    let secrets = environment
                        .values()
                        .map(|value| value.expose_secret().to_owned())
                        .filter(|value| !value.is_empty())
                        .collect::<Vec<_>>();
                    let mut command_line = CommandWrap::from(command_line);
                    #[cfg(unix)]
                    command_line.wrap(ProcessGroup::leader());
                    #[cfg(windows)]
                    command_line.wrap(JobObject);
                    let (transport, stderr) = TokioChildProcess::builder(command_line)
                        .stderr(Stdio::piped())
                        .spawn()
                        .map_err(|_| {
                            self.error(
                                RuntimeErrorCode::ServerStartFailed,
                                "connect_server",
                                "failed to start the downstream process",
                            )
                        })?;
                    let pid = transport.id();
                    tracing::info!(
                        operation = "process_start",
                        server_id = self.id,
                        transport = "stdio",
                        pid = pid.unwrap_or_default()
                    );
                    let (mut stderr, stderr_tail) = match stderr {
                        Some(stderr) => {
                            let tail = Arc::new(Mutex::new(VecDeque::with_capacity(
                                self.settings.stderr_tail_bytes,
                            )));
                            (
                                Some(spawn_stderr_drain(
                                    stderr,
                                    secrets,
                                    self.settings.stderr_tail_bytes,
                                    Arc::clone(&tail),
                                )),
                                Some(tail),
                            )
                        }
                        None => (None, None),
                    };
                    if let Err(error) = self.begin_initializing().await {
                        cancellation.cancel();
                        drop(transport);
                        if let Some(task) = stderr.take() {
                            await_task(task, self.settings.shutdown_grace).await;
                        }
                        return Err(error);
                    }
                    let service = match timeout(
                        self.settings.connect_timeout,
                        events.serve_with_ct(transport, cancellation.clone()),
                    )
                    .await
                    {
                        Ok(Ok(service)) => service,
                        outcome => {
                            cancellation.cancel();
                            if let Some(task) = stderr.take() {
                                await_task(task, self.settings.shutdown_grace).await;
                            }
                            let message = if outcome.is_err() {
                                "downstream initialization timed out"
                            } else {
                                "downstream initialization failed"
                            };
                            return Err(self.error(
                                RuntimeErrorCode::ServerInitializationFailed,
                                "connect_server",
                                message,
                            ));
                        }
                    };
                    (service, pid, stderr, stderr_tail)
                }
                ResolvedTransportConfig::Http { url, headers } => {
                    let headers = resolve_headers(headers, &self.id)?;
                    let config = StreamableHttpClientTransportConfig::with_uri(url.to_string())
                        .custom_headers(headers);
                    tracing::info!(
                        operation = "transport_start",
                        server_id = self.id,
                        transport = "http"
                    );
                    let service = if let Some(auth) = &self.auth {
                        let manager = auth.authenticated_manager().await?;
                        let client = reqwest::Client::builder()
                            .pool_max_idle_per_host(0)
                            .redirect(reqwest::redirect::Policy::none())
                            .build()
                            .map_err(|_| {
                                self.error(
                                    RuntimeErrorCode::HttpConnectionFailed,
                                    "connect_server",
                                    "the HTTP client could not be created",
                                )
                            })?;
                        let transport = StreamableHttpClientTransport::with_client(
                            AuthClient::new(client, manager),
                            config,
                        );
                        if let Err(error) = self.begin_initializing().await {
                            cancellation.cancel();
                            drop(transport);
                            return Err(error);
                        }
                        timeout(
                            self.settings.connect_timeout,
                            events.serve_with_ct(transport, cancellation.clone()),
                        )
                        .await
                    } else {
                        if self.registered.resolved_manifest().auth.is_some() {
                            return Err(self.error(
                                RuntimeErrorCode::AuthFailed,
                                "connect_server",
                                "the OAuth credential store is unavailable",
                            ));
                        }
                        let transport = StreamableHttpClientTransport::from_config(config);
                        if let Err(error) = self.begin_initializing().await {
                            cancellation.cancel();
                            drop(transport);
                            return Err(error);
                        }
                        timeout(
                            self.settings.connect_timeout,
                            events.serve_with_ct(transport, cancellation.clone()),
                        )
                        .await
                    }
                    .map_err(|_| {
                        self.error(
                            RuntimeErrorCode::HttpConnectionFailed,
                            "connect_server",
                            "HTTP initialization timed out",
                        )
                    })?
                    .map_err(|_| {
                        self.error(
                            RuntimeErrorCode::HttpConnectionFailed,
                            "connect_server",
                            "HTTP connection or initialization failed",
                        )
                    })?;
                    (service, None, None, None)
                }
            };

        let peer = service.peer().clone();
        let tools = match timeout(self.settings.connect_timeout, peer.list_all_tools()).await {
            Ok(Ok(tools)) => tools,
            Ok(Err(_)) => {
                close_unmanaged_service(&mut service, stderr.take(), self.settings.shutdown_grace)
                    .await;
                return Err(self.error(
                    RuntimeErrorCode::ServerInitializationFailed,
                    "connect_server",
                    "initial tool discovery failed",
                ));
            }
            Err(_) => {
                close_unmanaged_service(&mut service, stderr.take(), self.settings.shutdown_grace)
                    .await;
                return Err(self.error(
                    RuntimeErrorCode::ServerInitializationFailed,
                    "connect_server",
                    "initial tool discovery timed out",
                ));
            }
        };
        let snapshot = match tool_snapshot(&self.id, tools) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                close_unmanaged_service(&mut service, stderr.take(), self.settings.shutdown_grace)
                    .await;
                return Err(error);
            }
        };
        *self.tools.lock().await = Some(snapshot);

        let peer_info = peer.peer_info();
        let (protocol, upstream) = peer_info
            .as_deref()
            .map(safe_peer_info)
            .unwrap_or((Value::Null, Value::Null));
        let close_token = CancellationToken::new();
        let monitor = spawn_monitor(
            Arc::downgrade(self),
            peer,
            close_token.clone(),
            session_epoch,
        );
        Ok((
            ManagedSession {
                service,
                close_token,
                monitor,
                stderr,
                _stderr_tail: stderr_tail,
            },
            protocol,
            upstream,
            pid,
        ))
    }

    async fn finish_connect(
        &self,
        epoch: u64,
        result: Result<(ManagedSession, Value, Value, Option<u32>), RuntimeError>,
    ) -> Result<ConnectResult, RuntimeError> {
        let mut unused_session = None;
        let output = {
            let mut state = self.state.lock().await;
            let current = state.operation.as_ref().is_some_and(|operation| {
                operation.kind == OperationKind::Connect && operation.epoch == epoch
            });
            if !current {
                if let Ok((session, ..)) = result {
                    unused_session = Some(session);
                }
                Err(self.error(
                    RuntimeErrorCode::ServerUnavailable,
                    "connect_server",
                    "connection attempt was superseded",
                ))
            } else {
                state.operation = None;
                match result {
                    Ok((session, protocol, upstream, pid))
                        if state.lifecycle.desired() == DesiredConnection::Connected =>
                    {
                        let _ = state.lifecycle.transition_to(LifecycleState::Connected);
                        state.protocol = protocol;
                        state.upstream = upstream;
                        state.pid = pid;
                        state.connected_at_unix_ms = Some(unix_ms());
                        state.session = Some(session);
                        tracing::info!(
                            operation = "lifecycle_transition",
                            server_id = self.id,
                            state = "connected",
                            success = true
                        );
                        self.connect_result(&state).await
                    }
                    Ok((session, ..)) => {
                        unused_session = Some(session);
                        let _ = state.lifecycle.transition_to(LifecycleState::Stopped);
                        state.disconnected_at_unix_ms = Some(unix_ms());
                        Err(self.error(
                            RuntimeErrorCode::ServerUnavailable,
                            "connect_server",
                            "connection was cancelled",
                        ))
                    }
                    Err(_error) if state.lifecycle.desired() == DesiredConnection::Disconnected => {
                        let _ = state.lifecycle.transition_to(LifecycleState::Stopped);
                        state.disconnected_at_unix_ms = Some(unix_ms());
                        Err(self.error(
                            RuntimeErrorCode::ServerNotConnected,
                            "connect_server",
                            "connection was cancelled",
                        ))
                    }
                    Err(error) => {
                        let _ = state.lifecycle.fail(error.message.clone());
                        if state.last_safe_error.is_none() {
                            state.last_safe_error = Some(error.clone());
                        }
                        Err(error)
                    }
                }
            }
        };
        if let Some(session) = unused_session {
            close_session(session, self.settings.shutdown_grace).await;
        }
        self.changed.notify_waiters();
        output
    }

    async fn connect_result(&self, state: &RuntimeState) -> Result<ConnectResult, RuntimeError> {
        if state.lifecycle.state() != LifecycleState::Connected {
            if state.lifecycle.desired() == DesiredConnection::Disconnected {
                return Err(self.error(
                    RuntimeErrorCode::ServerNotConnected,
                    "connect_server",
                    "connection was cancelled",
                ));
            }
            return Err(state.last_safe_error.clone().unwrap_or_else(|| {
                self.error(
                    RuntimeErrorCode::ServerUnavailable,
                    "connect_server",
                    "the server did not connect",
                )
            }));
        }
        let snapshot = self.tools.lock().await.clone();
        Ok(ConnectResult {
            server_id: self.id.clone(),
            state: LifecycleState::Connected,
            tool_count: snapshot.as_ref().map_or(0, |snapshot| snapshot.tool_count),
            protocol_version: state
                .protocol
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            connected_at_unix_ms: state.connected_at_unix_ms,
            tool_snapshot: snapshot,
        })
    }

    async fn disconnect(self: &Arc<Self>) -> Result<DisconnectResult, RuntimeError> {
        self.cancel_recovery().await;
        loop {
            let action =
                {
                    let mut state = self.state.lock().await;
                    if let Some((kind, epoch, cancel)) = state.operation.as_ref().map(|operation| {
                        (operation.kind, operation.epoch, operation.cancel.clone())
                    }) {
                        if kind == OperationKind::Connect {
                            let _ = state.lifecycle.request_disconnect();
                            cancel.cancel();
                        }
                        DisconnectAction::Wait(epoch)
                    } else {
                        match state.lifecycle.request_disconnect() {
                            DisconnectDisposition::AlreadyInactive => {
                                return self.disconnect_result(&state);
                            }
                            DisconnectDisposition::JoinExisting
                            | DisconnectDisposition::CancelStartup => {
                                DisconnectAction::Wait(state.epoch)
                            }
                            DisconnectDisposition::Stop => {
                                state.epoch = state.epoch.saturating_add(1);
                                let epoch = state.epoch;
                                state.operation = Some(Operation {
                                    kind: OperationKind::Disconnect,
                                    epoch,
                                    cancel: CancellationToken::new(),
                                });
                                DisconnectAction::Stop {
                                    epoch,
                                    session: state.session.take(),
                                }
                            }
                        }
                    }
                };

            match action {
                DisconnectAction::Wait(epoch) => self.wait_for_operation(epoch).await,
                DisconnectAction::Stop { epoch, session } => {
                    let closed_cleanly = match session {
                        Some(session) => close_session(session, self.settings.shutdown_grace).await,
                        None => true,
                    };
                    let mut state = self.state.lock().await;
                    if state
                        .operation
                        .as_ref()
                        .is_some_and(|operation| operation.epoch == epoch)
                    {
                        state.operation = None;
                        if closed_cleanly {
                            if state.lifecycle.state() == LifecycleState::Connected {
                                let _ = state.lifecycle.transition_to(LifecycleState::Disconnected);
                            } else if state.lifecycle.state() == LifecycleState::Failed {
                                let _ = state.lifecycle.transition_to(LifecycleState::Stopped);
                            }
                            tracing::info!(
                                operation = "lifecycle_transition",
                                server_id = self.id,
                                state = "disconnected",
                                success = true
                            );
                        } else {
                            let error = self.error(
                                RuntimeErrorCode::ServerDisconnectFailed,
                                "disconnect_server",
                                "the downstream session did not close within the grace period",
                            );
                            let _ = state.lifecycle.fail(error.message.clone());
                            if state.last_safe_error.is_none() {
                                state.last_safe_error = Some(error.clone());
                            }
                            self.changed.notify_waiters();
                            return Err(error);
                        }
                        state.pid = None;
                        state.disconnected_at_unix_ms = Some(unix_ms());
                    }
                    mark_stale(&self.tools).await;
                    let result = self.disconnect_result(&state);
                    drop(state);
                    self.changed.notify_waiters();
                    return result;
                }
            }
        }
    }

    async fn cached_tools(&self) -> Result<ToolSnapshot, RuntimeError> {
        let state = self.state.lock().await;
        let snapshot = self.tools.lock().await.clone();
        match (state.lifecycle.state(), snapshot) {
            (LifecycleState::Connected, Some(snapshot)) => Ok(snapshot),
            (_, Some(mut snapshot)) => {
                snapshot.stale = true;
                Ok(snapshot)
            }
            (LifecycleState::Connected, None) => Err(self.error(
                RuntimeErrorCode::ToolsNotDiscovered,
                "list_tools",
                "tools have not been discovered",
            )),
            _ => Err(self.error(
                RuntimeErrorCode::ServerNotConnected,
                "list_tools",
                "the server is not connected",
            )),
        }
    }

    /// Returns cached tool names and descriptions without cloning schemas.
    async fn cached_tool_names(&self) -> Option<Vec<(String, Option<String>)>> {
        self.tools.lock().await.as_ref().map(|snapshot| {
            snapshot
                .tools
                .iter()
                .map(|tool| (tool.name.clone(), tool.description.clone()))
                .collect()
        })
    }

    async fn refresh(self: &Arc<Self>) -> Result<ToolSnapshot, RuntimeError> {
        let (peer, session_epoch) = {
            let state = self.state.lock().await;
            if state.lifecycle.state() != LifecycleState::Connected {
                return Err(self.error(
                    RuntimeErrorCode::ServerNotConnected,
                    "refresh_server",
                    "the server is not connected",
                ));
            }
            let peer = state
                .session
                .as_ref()
                .map(|session| session.service.peer().clone())
                .ok_or_else(|| {
                    self.error(
                        RuntimeErrorCode::ServerNotConnected,
                        "refresh_server",
                        "the server session is unavailable",
                    )
                })?;
            (peer, state.epoch)
        };
        let result = timeout(self.settings.request_timeout, peer.list_all_tools()).await;
        match result {
            Ok(Ok(tools)) => {
                let snapshot = tool_snapshot(&self.id, tools)?;
                *self.tools.lock().await = Some(snapshot.clone());
                Ok(snapshot)
            }
            Ok(Err(error)) => {
                mark_stale(&self.tools).await;
                if matches!(error, ServiceError::TransportClosed) {
                    self.mark_transport_closed(session_epoch).await;
                }
                Err(self.error(
                    RuntimeErrorCode::ProtocolError,
                    "refresh_server",
                    "tool refresh failed",
                ))
            }
            Err(_) => {
                mark_stale(&self.tools).await;
                Err(self.error(
                    RuntimeErrorCode::ToolCallTimeout,
                    "refresh_server",
                    "tool refresh timed out",
                ))
            }
        }
    }

    /// Returns a connected peer plus the session epoch, or a lifecycle error.
    async fn connected_peer(
        &self,
        operation: &str,
    ) -> Result<(Peer<RoleClient>, u64), RuntimeError> {
        let state = self.state.lock().await;
        if state.lifecycle.state() != LifecycleState::Connected {
            return Err(self.error(
                RuntimeErrorCode::ServerNotConnected,
                operation,
                "the server is not connected",
            ));
        }
        let peer = state
            .session
            .as_ref()
            .map(|session| session.service.peer().clone())
            .ok_or_else(|| {
                self.error(
                    RuntimeErrorCode::ServerNotConnected,
                    operation,
                    "the server session is unavailable",
                )
            })?;
        Ok((peer, state.epoch))
    }

    /// Returns true when the negotiated upstream capabilities advertise the key.
    async fn upstream_supports(&self, capability: &str) -> bool {
        self.state
            .lock()
            .await
            .upstream
            .get("capabilities")
            .and_then(|capabilities| capabilities.get(capability))
            .is_some()
    }

    async fn list_resources(self: &Arc<Self>) -> Result<Value, RuntimeError> {
        let (peer, session_epoch) = self.connected_peer("list_resources").await?;
        if !self.upstream_supports("resources").await {
            return Err(self.error(
                RuntimeErrorCode::ResourcesNotSupported,
                "list_resources",
                "the server does not advertise resource capabilities",
            ));
        }
        let result = timeout(self.settings.request_timeout, peer.list_all_resources()).await;
        match result {
            Ok(Ok(resources)) => serde_json::to_value(resources).map_err(|_| {
                self.error(
                    RuntimeErrorCode::ProtocolError,
                    "list_resources",
                    "resource list could not be serialized",
                )
            }),
            Ok(Err(error)) => {
                if matches!(error, ServiceError::TransportClosed) {
                    self.mark_transport_closed(session_epoch).await;
                }
                Err(self.request_error("list_resources", error))
            }
            Err(_) => Err(self.error(
                RuntimeErrorCode::ToolCallTimeout,
                "list_resources",
                "resource listing timed out",
            )),
        }
    }

    async fn read_resource(self: &Arc<Self>, uri: &str) -> Result<Value, RuntimeError> {
        let (peer, session_epoch) = self.connected_peer("read_resource").await?;
        if !self.upstream_supports("resources").await {
            return Err(self.error(
                RuntimeErrorCode::ResourcesNotSupported,
                "read_resource",
                "the server does not advertise resource capabilities",
            ));
        }
        let params = ReadResourceRequestParams::new(uri);
        let result = timeout(self.settings.request_timeout, peer.read_resource(params)).await;
        match result {
            Ok(Ok(resource)) => serde_json::to_value(resource).map_err(|_| {
                self.error(
                    RuntimeErrorCode::ProtocolError,
                    "read_resource",
                    "resource content could not be serialized",
                )
            }),
            Ok(Err(error)) => {
                if matches!(error, ServiceError::TransportClosed) {
                    self.mark_transport_closed(session_epoch).await;
                }
                Err(self.request_error("read_resource", error))
            }
            Err(_) => Err(self.error(
                RuntimeErrorCode::ToolCallTimeout,
                "read_resource",
                "resource read timed out",
            )),
        }
    }

    async fn list_prompts(self: &Arc<Self>) -> Result<Value, RuntimeError> {
        let (peer, session_epoch) = self.connected_peer("list_prompts").await?;
        if !self.upstream_supports("prompts").await {
            return Err(self.error(
                RuntimeErrorCode::PromptsNotSupported,
                "list_prompts",
                "the server does not advertise prompt capabilities",
            ));
        }
        let result = timeout(self.settings.request_timeout, peer.list_all_prompts()).await;
        match result {
            Ok(Ok(prompts)) => serde_json::to_value(prompts).map_err(|_| {
                self.error(
                    RuntimeErrorCode::ProtocolError,
                    "list_prompts",
                    "prompt list could not be serialized",
                )
            }),
            Ok(Err(error)) => {
                if matches!(error, ServiceError::TransportClosed) {
                    self.mark_transport_closed(session_epoch).await;
                }
                Err(self.request_error("list_prompts", error))
            }
            Err(_) => Err(self.error(
                RuntimeErrorCode::ToolCallTimeout,
                "list_prompts",
                "prompt listing timed out",
            )),
        }
    }

    async fn get_prompt(
        self: &Arc<Self>,
        name: &str,
        arguments: Map<String, Value>,
    ) -> Result<Value, RuntimeError> {
        let (peer, session_epoch) = self.connected_peer("call_prompt").await?;
        if !self.upstream_supports("prompts").await {
            return Err(self.error(
                RuntimeErrorCode::PromptsNotSupported,
                "call_prompt",
                "the server does not advertise prompt capabilities",
            ));
        }
        let params = GetPromptRequestParams::new(name).with_arguments(arguments);
        let result = timeout(self.settings.request_timeout, peer.get_prompt(params)).await;
        match result {
            Ok(Ok(prompt)) => serde_json::to_value(prompt).map_err(|_| {
                self.error(
                    RuntimeErrorCode::ProtocolError,
                    "call_prompt",
                    "prompt result could not be serialized",
                )
            }),
            Ok(Err(error)) => {
                if matches!(error, ServiceError::TransportClosed) {
                    self.mark_transport_closed(session_epoch).await;
                }
                Err(self.request_error("call_prompt", error))
            }
            Err(_) => Err(self.error(
                RuntimeErrorCode::ToolCallTimeout,
                "call_prompt",
                "prompt call timed out",
            )),
        }
    }

    async fn call_tool(
        self: &Arc<Self>,
        tool_name: &str,
        arguments: Value,
        timeout_ms: Option<u64>,
    ) -> Result<ToolCallResult, RuntimeError> {
        if !self.registered.enabled() {
            return Err(self.error(
                RuntimeErrorCode::ServerDisabled,
                "call_tool",
                "the server is disabled",
            ));
        }
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            _ => {
                return Err(self.error(
                    RuntimeErrorCode::InvalidArguments,
                    "call_tool",
                    "tool arguments must be a JSON object",
                ));
            }
        };
        let (peer, found, session_epoch) = {
            let state = self.state.lock().await;
            if state.lifecycle.state() != LifecycleState::Connected {
                return Err(self.error(
                    RuntimeErrorCode::ServerNotConnected,
                    "call_tool",
                    "the server is not connected",
                ));
            }
            let peer = state
                .session
                .as_ref()
                .map(|session| session.service.peer().clone())
                .ok_or_else(|| {
                    self.error(
                        RuntimeErrorCode::ServerNotConnected,
                        "call_tool",
                        "the server session is unavailable",
                    )
                })?;
            let found =
                self.tools.lock().await.as_ref().is_some_and(|snapshot| {
                    snapshot.tools.iter().any(|tool| tool.name == tool_name)
                });
            (peer, found, state.epoch)
        };
        if !found {
            return Err(self.error(
                RuntimeErrorCode::ToolNotFound,
                "call_tool",
                "the requested tool was not discovered",
            ));
        }
        let timeout = request_timeout(timeout_ms, &self.settings, &self.id)?;
        let params = CallToolRequestParams::new(tool_name.to_owned()).with_arguments(arguments);
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let handle = match peer
            .send_cancellable_request(request, PeerRequestOptions::with_timeout(timeout))
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                if matches!(error, ServiceError::TransportClosed) {
                    self.mark_transport_closed(session_epoch).await;
                }
                return Err(self.request_error("call_tool", error));
            }
        };
        match handle.await_response().await {
            Ok(ServerResult::CallToolResult(result)) => serde_json::to_value(result)
                .map(ToolCallResult::new)
                .map_err(|_| {
                    self.error(
                        RuntimeErrorCode::ProtocolError,
                        "call_tool",
                        "tool result could not be serialized",
                    )
                }),
            Ok(_) => Err(self.error(
                RuntimeErrorCode::ProtocolError,
                "call_tool",
                "received an unexpected tool response",
            )),
            Err(ServiceError::Timeout { .. }) => Err(self.error(
                RuntimeErrorCode::ToolCallTimeout,
                "call_tool",
                "the downstream tool call timed out",
            )),
            Err(ServiceError::TransportClosed) => {
                self.mark_transport_closed(session_epoch).await;
                Err(self.error(
                    RuntimeErrorCode::TransportClosed,
                    "call_tool",
                    "the downstream transport closed during the tool call",
                ))
            }
            Err(_) => Err(self.error(
                RuntimeErrorCode::ToolCallFailed,
                "call_tool",
                "the downstream tool call failed",
            )),
        }
    }

    fn disconnect_result(&self, state: &RuntimeState) -> Result<DisconnectResult, RuntimeError> {
        Ok(DisconnectResult {
            server_id: self.id.clone(),
            state: state.lifecycle.state(),
            disconnected_at_unix_ms: state.disconnected_at_unix_ms,
        })
    }

    async fn mark_transport_closed(self: &Arc<Self>, session_epoch: u64) {
        let should_recover = {
            let mut state = self.state.lock().await;
            if state.epoch != session_epoch
                || state.lifecycle.desired() != DesiredConnection::Connected
                || state.lifecycle.state() != LifecycleState::Connected
            {
                return;
            }
            let error = self.error(
                RuntimeErrorCode::TransportClosed,
                "monitor",
                "the downstream transport closed unexpectedly",
            );
            let _ = state.lifecycle.fail(error.message.clone());
            if state.last_safe_error.is_none() {
                state.last_safe_error = Some(error.clone());
            }
            tracing::warn!(
                operation = "lifecycle_transition",
                server_id = self.id,
                state = "failed",
                success = false,
                error_code = error.code.as_str()
            );
            self.registered.resolved_manifest().reconnect.enabled
        };
        mark_stale(&self.tools).await;
        self.changed.notify_waiters();
        if should_recover {
            self.schedule_recovery().await;
        }
    }

    async fn schedule_recovery(self: &Arc<Self>) {
        if self.shutdown.is_cancelled() {
            return;
        }
        let cancel = {
            let mut recovery = self.recovery_cancel.lock().await;
            if recovery.is_some() {
                return;
            }
            let cancel = CancellationToken::new();
            *recovery = Some(cancel.clone());
            cancel
        };
        self.recovery_tasks.fetch_add(1, Ordering::AcqRel);
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            runtime.reconnect_loop(cancel).await;
            *runtime.recovery_cancel.lock().await = None;
            runtime.recovery_tasks.fetch_sub(1, Ordering::AcqRel);
            runtime.recovery_changed.notify_waiters();
        });
    }

    async fn reconnect_loop(self: &Arc<Self>, cancel: CancellationToken) {
        let config = self.registered.resolved_manifest().reconnect.clone();
        for attempt in 0..config.max_retries {
            let delay = reconnect_delay(&config, attempt, &self.id);
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.shutdown.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            if self.desired().await != DesiredConnection::Connected {
                return;
            }
            tracing::info!(
                operation = "automatic_reconnect",
                server_id = self.id,
                attempt = attempt + 1,
                delay_ms = duration_ms(delay)
            );
            match self.connect_inner().await {
                Ok(_) => return,
                Err(error) if error.code == RuntimeErrorCode::AuthRequired => return,
                Err(_) => {}
            }
        }
    }

    async fn cancel_recovery(&self) {
        if let Some(cancel) = self.recovery_cancel.lock().await.as_ref() {
            cancel.cancel();
        }
    }

    async fn wait_for_recovery(&self) {
        loop {
            let notified = self.recovery_changed.notified();
            if self.recovery_tasks.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    fn error(&self, code: RuntimeErrorCode, operation: &str, message: &str) -> RuntimeError {
        RuntimeError::for_server(code, operation, &self.id, message)
    }

    async fn auth_start(&self, redirect_uri: &str) -> Result<AuthLoginStartResult, RuntimeError> {
        if self.auth_logout_in_progress.load(Ordering::Acquire) {
            return Err(self.error(
                RuntimeErrorCode::AuthInProgress,
                "auth_start",
                "an OAuth logout is in progress",
            ));
        }
        let state = self.state.lock().await;
        if state.lifecycle.state() == LifecycleState::Connected || state.operation.is_some() {
            return Err(self.error(
                RuntimeErrorCode::AuthInProgress,
                "auth_start",
                "disconnect the server before starting OAuth authorization",
            ));
        }
        drop(state);
        self.oauth("auth_start")?.start(redirect_uri).await
    }

    fn begin_auth_logout(&self) -> Result<AuthLogoutGuard<'_>, RuntimeError> {
        self.auth_logout_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                self.error(
                    RuntimeErrorCode::AuthInProgress,
                    "auth_logout",
                    "an OAuth logout is already in progress",
                )
            })?;
        Ok(AuthLogoutGuard(&self.auth_logout_in_progress))
    }

    fn oauth(&self, operation: &str) -> Result<&ServerAuth, RuntimeError> {
        if self.registered.resolved_manifest().auth.is_none() {
            return Err(self.error(
                RuntimeErrorCode::AuthNotConfigured,
                operation,
                "the server manifest has no OAuth configuration",
            ));
        }
        self.auth.as_ref().ok_or_else(|| {
            self.error(
                RuntimeErrorCode::AuthFailed,
                operation,
                "the OAuth credential store is unavailable",
            )
        })
    }

    fn request_error(&self, operation: &str, error: ServiceError) -> RuntimeError {
        match error {
            ServiceError::TransportClosed => self.error(
                RuntimeErrorCode::TransportClosed,
                operation,
                "the downstream transport is closed",
            ),
            ServiceError::Timeout { .. } => self.error(
                RuntimeErrorCode::ToolCallTimeout,
                operation,
                "the downstream request timed out",
            ),
            _ => self.error(
                RuntimeErrorCode::ToolCallFailed,
                operation,
                "the downstream request could not be sent",
            ),
        }
    }

    async fn begin_initializing(&self) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().await;
        if state.lifecycle.desired() != DesiredConnection::Connected {
            return Err(self.error(
                RuntimeErrorCode::ServerNotConnected,
                "connect_server",
                "connection was cancelled",
            ));
        }
        state
            .lifecycle
            .transition_to(LifecycleState::Initializing)
            .map_err(|_| {
                self.error(
                    RuntimeErrorCode::ServerInitializationFailed,
                    "connect_server",
                    "the server could not enter initialization",
                )
            })
    }

    async fn wait_for_operation(&self, epoch: u64) {
        loop {
            let notified = self.changed.notified();
            let active = self
                .state
                .lock()
                .await
                .operation
                .as_ref()
                .is_some_and(|operation| operation.epoch == epoch);
            if !active {
                return;
            }
            notified.await;
        }
    }
}

struct AuthLogoutGuard<'a>(&'a AtomicBool);

impl Drop for AuthLogoutGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

enum ConnectAction {
    Wait {
        epoch: u64,
        joined_connect: bool,
    },
    Start {
        epoch: u64,
        cancel: CancellationToken,
        old_session: Option<ManagedSession>,
    },
}

enum DisconnectAction {
    Wait(u64),
    Stop {
        epoch: u64,
        session: Option<ManagedSession>,
    },
}

fn transport_kind(server: &RegisteredServer) -> TransportKind {
    match server.resolved_manifest().transport {
        ResolvedTransportConfig::Stdio { .. } => TransportKind::Stdio,
        ResolvedTransportConfig::Http { .. } => TransportKind::Http,
    }
}

fn public_manifest(server: &RegisteredServer) -> Value {
    let manifest = server.resolved_manifest();
    let transport = match &manifest.transport {
        ResolvedTransportConfig::Stdio {
            command,
            args,
            working_directory,
            environment,
        } => json!({
            "type": "stdio",
            "command": command,
            "args": args,
            "workingDirectory": working_directory,
            "environmentKeys": environment.keys().collect::<Vec<_>>(),
        }),
        ResolvedTransportConfig::Http { url, headers } => json!({
            "type": "http",
            "scheme": url.scheme(),
            "host": url.host_str(),
            "port": url.port(),
            "path": url.path(),
            "headerNames": headers.keys().collect::<Vec<_>>(),
            "oauth": manifest.auth.as_ref().map(|auth| json!({
                "enabled": true,
                "clientIdConfigured": auth.client_id.is_some(),
                "scopes": auth.scopes,
            })),
        }),
    };
    json!({
        "id": manifest.id.as_str(),
        "name": manifest.name,
        "description": manifest.description,
        "enabled": manifest.enabled,
        "transport": transport,
    })
}

fn resolve_headers(
    headers: std::collections::BTreeMap<String, mcp_host_core::SecretValue>,
    server_id: &str,
) -> Result<HashMap<HeaderName, HeaderValue>, RuntimeError> {
    headers
        .into_iter()
        .map(|(name, value)| {
            let name = HeaderName::try_from(name).map_err(|_| {
                RuntimeError::for_server(
                    RuntimeErrorCode::HttpConnectionFailed,
                    "connect_server",
                    server_id,
                    "the configured HTTP header name is invalid",
                )
            })?;
            let value = HeaderValue::try_from(value.expose_secret()).map_err(|_| {
                RuntimeError::for_server(
                    RuntimeErrorCode::HttpConnectionFailed,
                    "connect_server",
                    server_id,
                    "the configured HTTP header value is invalid",
                )
            })?;
            Ok((name, value))
        })
        .collect()
}

fn tool_snapshot(server_id: &str, tools: Vec<Tool>) -> Result<ToolSnapshot, RuntimeError> {
    let definitions = tools
        .into_iter()
        .map(tool_definition)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ToolSnapshot {
        server_id: server_id.to_owned(),
        fetched_at_unix_ms: unix_ms(),
        tool_count: definitions.len() as u64,
        tools: definitions,
        stale: false,
    })
}

fn tool_definition(tool: Tool) -> Result<ToolDefinition, RuntimeError> {
    Ok(ToolDefinition {
        name: tool.name.into_owned(),
        title: tool.title,
        description: tool.description.map(|description| description.into_owned()),
        input_schema: Value::Object(tool.input_schema.as_ref().clone()),
        output_schema: tool
            .output_schema
            .map(|schema| Value::Object(schema.as_ref().clone())),
        annotations: value_or_none(tool.annotations)?,
        icons: value_or_none(tool.icons)?,
        meta: value_or_none(tool.meta)?,
    })
}

fn value_or_none<T: serde::Serialize>(value: Option<T>) -> Result<Option<Value>, RuntimeError> {
    value.map(serde_json::to_value).transpose().map_err(|_| {
        RuntimeError::new(
            RuntimeErrorCode::ProtocolError,
            "tool_conversion",
            "tool metadata could not be serialized",
        )
    })
}

fn safe_peer_info(info: &ServerInfo) -> (Value, Value) {
    let protocol = json!({ "version": info.protocol_version.as_str() });
    let upstream = serde_json::to_value(json!({
        "serverInfo": info.server_info,
        "capabilities": info.capabilities,
        "instructions": info.instructions,
    }))
    .unwrap_or(Value::Null);
    (protocol, upstream)
}

fn request_timeout(
    timeout_ms: Option<u64>,
    settings: &RuntimeSettings,
    server_id: &str,
) -> Result<Duration, RuntimeError> {
    let Some(milliseconds) = timeout_ms else {
        return Ok(settings.request_timeout);
    };
    if !(1..=300_000).contains(&milliseconds)
        || Duration::from_millis(milliseconds) > settings.max_request_timeout
    {
        return Err(RuntimeError::for_server(
            RuntimeErrorCode::InvalidArguments,
            "call_tool",
            server_id,
            "timeout_ms must be between 1 and 300000 milliseconds",
        ));
    }
    Ok(Duration::from_millis(milliseconds))
}

const MAX_TOOL_SUGGESTIONS: usize = 8;

/// Lowercases, trims, and collapses whitespace; returns an empty string when
/// the query contains nothing indexable.
fn normalize_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ranks a tool name against a normalized query: exact = 0, prefix = 1,
/// substring = 2, all query words contained = 3, otherwise no match.
fn score_tool_name(normalized_query: &str, tool_name: &str) -> Option<u32> {
    if normalized_query.is_empty() {
        return None;
    }
    let normalized_name = normalize_query(tool_name);
    if normalized_name == normalized_query {
        return Some(0);
    }
    if normalized_name.starts_with(normalized_query) {
        return Some(1);
    }
    if normalized_name.contains(normalized_query) {
        return Some(2);
    }
    let words = normalized_query
        .split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if words.len() > 1 && words.iter().all(|word| normalized_name.contains(word)) {
        return Some(3);
    }
    None
}

fn spawn_monitor(
    runtime: Weak<ServerRuntime>,
    peer: Peer<RoleClient>,
    close_token: CancellationToken,
    session_epoch: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = close_token.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if peer.is_transport_closed() {
                        if let Some(runtime) = runtime.upgrade() {
                            runtime.mark_transport_closed(session_epoch).await;
                        }
                        return;
                    }
                }
            }
        }
    })
}

fn reconnect_delay(config: &ReconnectConfig, attempt: u32, server_id: &str) -> Duration {
    let exponent = attempt.min(31);
    let base = config
        .initial_backoff_ms
        .saturating_mul(1_u64 << exponent)
        .min(config.max_backoff_ms);
    if !config.jitter || base < 5 {
        return Duration::from_millis(base);
    }

    let spread = base / 5;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in server_id.bytes().chain(attempt.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let width = spread.saturating_mul(2).saturating_add(1);
    let offset = hash % width;
    Duration::from_millis(base.saturating_sub(spread).saturating_add(offset))
}

fn spawn_stderr_drain(
    mut stderr: tokio::process::ChildStderr,
    secrets: Vec<String>,
    capacity: usize,
    tail: Arc<Mutex<VecDeque<u8>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 1024];
        let mut pending = Vec::new();
        loop {
            let read = match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            pending.extend_from_slice(&buffer[..read]);
            drain_redacted_tail(&mut pending, &secrets, capacity, &tail, false).await;
        }
        drain_redacted_tail(&mut pending, &secrets, capacity, &tail, true).await;
    })
}

async fn drain_redacted_tail(
    pending: &mut Vec<u8>,
    secrets: &[String],
    capacity: usize,
    tail: &Mutex<VecDeque<u8>>,
    end_of_stream: bool,
) {
    let mut sanitized = Vec::new();
    sanitize_pending(pending, secrets, end_of_stream, &mut sanitized);
    let mut tail = tail.lock().await;
    for byte in sanitized {
        if tail.len() == capacity {
            let _ = tail.pop_front();
        }
        if capacity != 0 {
            tail.push_back(byte);
        }
    }
}

fn sanitize_pending(
    pending: &mut Vec<u8>,
    secrets: &[String],
    end_of_stream: bool,
    sanitized: &mut Vec<u8>,
) {
    const REDACTED: &[u8] = b"<redacted>";
    let mut consumed = 0;
    while consumed < pending.len() {
        let remaining = &pending[consumed..];
        let complete = secrets
            .iter()
            .filter(|secret| remaining.starts_with(secret.as_bytes()))
            .map(String::len)
            .max();
        let can_extend = secrets.iter().any(|secret| {
            secret.as_bytes().starts_with(remaining) && secret.len() > remaining.len()
        });

        if can_extend {
            if end_of_stream {
                sanitized.extend_from_slice(REDACTED);
                consumed = pending.len();
            }
            break;
        }
        if let Some(length) = complete {
            consumed += length;
            sanitized.extend_from_slice(REDACTED);
        } else {
            sanitized.push(remaining[0]);
            consumed += 1;
        }
    }
    pending.drain(..consumed);
}

async fn close_session(mut session: ManagedSession, grace: Duration) -> bool {
    session.close_token.cancel();
    let closed_cleanly = matches!(session.service.close_with_timeout(grace).await, Ok(Some(_)));
    await_task(session.monitor, grace).await;
    if let Some(stderr) = session.stderr.take() {
        await_task(stderr, grace).await;
    }
    closed_cleanly
}

async fn close_unmanaged_service(
    service: &mut RunningService<RoleClient, ClientEvents>,
    stderr: Option<JoinHandle<()>>,
    grace: Duration,
) {
    let _ = service.close_with_timeout(grace).await;
    if let Some(stderr) = stderr {
        await_task(stderr, grace).await;
    }
}

async fn await_task(handle: JoinHandle<()>, grace: Duration) {
    let mut handle = handle;
    if timeout(grace, &mut handle).await.is_err() {
        handle.abort();
        let _ = handle.await;
    }
}

async fn mark_stale(tools: &Mutex<Option<ToolSnapshot>>) {
    if let Some(snapshot) = tools.lock().await.as_mut() {
        snapshot.stale = true;
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn trace_operation<T>(
    operation: &'static str,
    server_id: &str,
    started: Instant,
    result: &Result<T, RuntimeError>,
) {
    tracing::info!(
        operation,
        server_id,
        duration_ms = duration_ms(started.elapsed()),
        success = result.is_ok(),
        error_code = result
            .as_ref()
            .err()
            .map_or("", |error| error.code.as_str())
    );
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use mcp_host_core::{
        CallPolicy, EnvironmentAccessError, EnvironmentProvider, ManifestLoader, McpServerRegistry,
        Policy, ReconnectConfig, RegistryBuilder, SkillCatalog,
    };
    use rmcp::model::Tool;
    use serde_json::{Map, json};
    use tempfile::tempdir;

    use super::{
        RuntimeManager, RuntimeSettings, normalize_query, reconnect_delay, sanitize_pending,
        score_tool_name, tool_definition,
    };

    struct EmptyEnvironment;

    impl EnvironmentProvider for EmptyEnvironment {
        fn get(&self, _name: &str) -> Result<Option<String>, EnvironmentAccessError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn unknown_and_disabled_servers_are_rejected() {
        let manager = manager("disabled", false);
        let unknown = manager
            .connect_server("missing", None)
            .await
            .expect_err("unknown must fail");
        let disabled = manager
            .connect_server("disabled", None)
            .await
            .expect_err("disabled must fail");
        assert_eq!(unknown.code.as_str(), "SERVER_NOT_FOUND");
        assert_eq!(disabled.code.as_str(), "SERVER_DISABLED");
    }

    #[tokio::test]
    async fn invalid_tool_arguments_are_rejected_before_connection() {
        let manager = manager("enabled", true);
        let error = manager
            .call_tool(
                "enabled",
                "tool",
                json!("not-an-object"),
                None,
                CallPolicy::default(),
            )
            .await
            .expect_err("invalid arguments must fail");
        assert_eq!(error.code.as_str(), "INVALID_ARGUMENTS");
    }

    #[test]
    fn tool_name_scoring_ranks_exact_then_prefix_then_substring_then_words() {
        let cases = [
            ("echo", "echo", Some(0)),
            ("echo", "echos", Some(1)),
            ("echo", "multiecho", Some(2)),
            ("get user", "get_user_by_id", Some(3)),
            ("get user", "get_user", Some(3)),
            ("zebra", "echo", None),
            ("", "echo", None),
            ("ECHO", "echo", Some(0)),
            ("read file", "write_file", None),
        ];
        for (query, name, expected) in cases {
            let normalized = normalize_query(query);
            assert_eq!(
                score_tool_name(&normalized, name),
                expected,
                "query {query:?} vs name {name:?}"
            );
        }
    }

    #[test]
    fn normalize_query_lowercases_and_collapses_whitespace() {
        assert_eq!(normalize_query("  Get  User "), "get user");
        assert_eq!(normalize_query(""), "");
        assert_eq!(normalize_query("   "), "");
    }

    #[tokio::test]
    async fn server_entries_are_deterministic() {
        let manager = manager_with("zeta", true, "alpha", true);
        let entries = manager.list_servers().await;
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
    }

    #[tokio::test]
    async fn registry_reload_reconciles_add_change_remove_and_noop() {
        let manager = manager("alpha", true);

        let added = manager
            .reload_registry(Arc::new(registry(&[("alpha", true), ("beta", true)])))
            .await;
        assert_eq!((added.added, added.changed, added.removed), (1, 0, 0));
        assert_eq!(added.generation, 2);
        assert_eq!(manager.server_count(), 2);

        let noop = manager
            .reload_registry(Arc::new(registry(&[("alpha", true), ("beta", true)])))
            .await;
        assert_eq!((noop.added, noop.changed, noop.removed), (0, 0, 0));
        assert_eq!(noop.generation, 2);

        let changed = manager
            .reload_registry(Arc::new(registry(&[("alpha", false)])))
            .await;
        assert_eq!((changed.added, changed.changed, changed.removed), (0, 1, 1));
        assert_eq!(changed.generation, 3);
        let entries = manager.list_servers().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "alpha");
        assert!(!entries[0].enabled);
    }

    #[tokio::test]
    async fn policy_denial_is_enforced_and_reloads_atomically() {
        let registry = Arc::new(registry(&[("alpha", true)]));
        let manager = RuntimeManager::new_with_policy(
            Arc::clone(&registry),
            RuntimeSettings::default(),
            policy(
                r#"
                    [[rules]]
                    id = "deny-connect"
                    action = "connect"
                    effect = "deny"
                    server = "alpha"
                "#,
            ),
        );
        let error = manager
            .connect_server("alpha", None)
            .await
            .expect_err("policy should deny connect before process startup");
        assert_eq!(error.code.as_str(), "POLICY_DENIED");

        let result = manager
            .reload_configuration(
                registry,
                policy(
                    r#"
                        [[rules]]
                        id = "hide-alpha"
                        action = "list"
                        effect = "deny"
                        server = "alpha"
                    "#,
                ),
                SkillCatalog::default(),
            )
            .await;
        assert!(result.policy_changed);
        assert_eq!(result.generation, 2);
        assert!(manager.list_servers().await.is_empty());
    }

    #[tokio::test]
    async fn skill_catalog_reloads_with_the_same_atomic_generation() {
        let registry = Arc::new(registry(&[("alpha", true)]));
        let manager = RuntimeManager::new(Arc::clone(&registry), RuntimeSettings::default());
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("one.skill.toml"),
            "id='one'\nname='One'\ndescription='One'\n[[steps]]\nid='run'\nserver='alpha'\ntool='echo'\n",
        )
        .expect("skill fixture");
        let skills = SkillCatalog::load_directory(directory.path()).expect("skill catalog");

        let result = manager
            .reload_configuration(registry, Policy::default(), skills)
            .await;
        assert!(result.skills_changed);
        assert!(!result.policy_changed);
        assert_eq!(result.generation, 2);
        assert_eq!(manager.list_skills().await[0].id, "one");
    }

    #[test]
    fn tool_conversion_preserves_optional_metadata() {
        let tool = Tool::new("echo", "Echo", Arc::new(Map::new())).with_title("Echo title");
        let definition = tool_definition(tool).expect("tool should convert");
        assert_eq!(definition.name, "echo");
        assert_eq!(definition.title.as_deref(), Some("Echo title"));
        assert_eq!(definition.input_schema, json!({}));
    }

    #[test]
    fn stderr_redaction_handles_secrets_split_across_read_chunks() {
        let secrets = vec!["sentinel-secret".to_owned()];
        let mut pending = b"prefix-sentinel".to_vec();
        let mut sanitized = Vec::new();
        sanitize_pending(&mut pending, &secrets, false, &mut sanitized);
        assert_eq!(sanitized, b"prefix-");

        pending.extend_from_slice(b"-secret-suffix");
        sanitize_pending(&mut pending, &secrets, false, &mut sanitized);
        assert_eq!(sanitized, b"prefix-<redacted>-suffix");
        assert!(pending.is_empty());
    }

    #[test]
    fn stderr_redaction_hides_a_partial_secret_at_end_of_stream() {
        let secrets = vec!["sentinel-secret".to_owned()];
        let mut pending = b"sentinel-sec".to_vec();
        let mut sanitized = Vec::new();
        sanitize_pending(&mut pending, &secrets, true, &mut sanitized);
        assert_eq!(sanitized, b"<redacted>");
    }

    #[test]
    fn stderr_redaction_handles_overlapping_secret_prefixes() {
        let secrets = vec!["ab".to_owned(), "abcde".to_owned()];
        let mut pending = b"prefix-abc".to_vec();
        let mut sanitized = Vec::new();
        sanitize_pending(&mut pending, &secrets, false, &mut sanitized);
        assert_eq!(sanitized, b"prefix-");

        pending.extend_from_slice(b"de-suffix");
        sanitize_pending(&mut pending, &secrets, false, &mut sanitized);
        assert_eq!(sanitized, b"prefix-<redacted>-suffix");
        assert!(pending.is_empty());
    }

    #[test]
    fn reconnect_delay_is_capped_and_jitter_is_bounded() {
        let mut config = ReconnectConfig {
            enabled: true,
            max_retries: 5,
            initial_backoff_ms: 100,
            max_backoff_ms: 350,
            jitter: false,
        };
        assert_eq!(reconnect_delay(&config, 0, "server").as_millis(), 100);
        assert_eq!(reconnect_delay(&config, 1, "server").as_millis(), 200);
        assert_eq!(reconnect_delay(&config, 2, "server").as_millis(), 350);
        assert_eq!(reconnect_delay(&config, 10, "server").as_millis(), 350);

        config.jitter = true;
        let jittered = reconnect_delay(&config, 2, "server").as_millis();
        assert!((280..=420).contains(&jittered));
    }

    fn manager(id: &str, enabled: bool) -> Arc<RuntimeManager> {
        let registry = registry(&[(id, enabled)]);
        RuntimeManager::new(Arc::new(registry), RuntimeSettings::default())
    }

    fn manager_with(
        first: &str,
        first_enabled: bool,
        second: &str,
        second_enabled: bool,
    ) -> Arc<RuntimeManager> {
        RuntimeManager::new(
            Arc::new(registry(&[
                (first, first_enabled),
                (second, second_enabled),
            ])),
            RuntimeSettings::default(),
        )
    }

    fn registry(entries: &[(&str, bool)]) -> McpServerRegistry {
        let directory = tempdir().expect("temporary directory should be created");
        for (index, (id, enabled)) in entries.iter().enumerate() {
            fs::write(
                directory.path().join(format!("{index}.toml")),
                format!(
                    "id = {id:?}\nname = {id:?}\ndescription = \"test\"\nenabled = {enabled}\n[transport]\ntype = \"stdio\"\ncommand = \"unused\"\n"
                ),
            )
            .expect("manifest should be written");
        }
        let loaded = ManifestLoader::new(EmptyEnvironment)
            .load_directory(directory.path())
            .expect("manifests should load");
        RegistryBuilder::build(loaded).expect("registry should build")
    }

    fn policy(contents: &str) -> Policy {
        let directory = tempdir().expect("temporary policy directory");
        fs::write(directory.path().join("policy.toml"), contents)
            .expect("policy fixture should write");
        Policy::load_optional(directory.path()).expect("policy fixture should load")
    }
}
