# V1 Architecture

Dynamic MCP Host is a long-running, per-user daemon. It exposes one stable MCP
server and one control-plane API while it manages manifest-defined downstream
MCP servers. It does not expose downstream tools as host tools, choose a tool,
or interpret an upstream tool's arguments or result.

## Topology

```text
                     control IPC: u32 BE length + JSON envelope
CLI  -------------------------------------------------------------+
                                                               |
MCP client -- stdio -- `mcp-host mcp` raw byte bridge -- MCP IPC |
                                                               v
                                                     +-------------------+
                                                     | per-user daemon   |
                                                     | RuntimeManager    |
                                                     | HostMcpServer     |
                                                     +--------+----------+
                                                              |
                                      +-----------------------+-----------------------+
                                      |                                               |
                              RMCP TokioChildProcess                    RMCP Streamable HTTP
                              downstream stdio process                  downstream HTTP server
```

The CLI is short-lived; connection and child-process state belong to the
daemon. A daemon starts from one validated manifest/policy snapshot, watches the
configuration directory, debounces changes for 500 ms, and atomically publishes
only a fully valid replacement snapshot. Invalid reloads retain the prior state.

## Crate Boundaries

| Crate | Owns |
| --- | --- |
| `mcp-host-core` | Immutable manifest registry, lifecycle domain model, public control and runtime DTOs, stable error codes. |
| `mcp-host-mcp` | `RuntimeManager`, RMCP downstream clients, tool cache, and fixed inbound RMCP `ServerHandler`. |
| `mcp-host` | Daemon process, local IPC, CLI, and the stdio bridge binary mode. |

Dependencies point inward: `mcp-host` depends on `mcp-host-mcp` and
`mcp-host-core`; `mcp-host-mcp` depends on `mcp-host-core`. The core crate has
no RMCP, Tokio, daemon, or local-socket dependency.

## Registry And Domain

`ManifestLoader` synchronously discovers direct, non-hidden lowercase `.toml`
files, parses and validates them, resolves complete `${NAME}` environment
references, and yields raw plus resolved manifests. `RegistryBuilder` sorts by
source filename and creates an immutable, normalized-ID `BTreeMap` snapshot.
Duplicate normalized IDs fail construction deterministically.

Resolved secrets are `SecretValue` values. They cannot be serialized or
displayed accidentally; public inspection reports only environment keys, HTTP
header names, and URL scheme/host/port/path. The registry contains no process,
peer, lifecycle, or tool-cache state.

`mcp-host-core` also owns transport-neutral DTOs such as `ServerSummary`,
`ServerInspection`, `ToolSnapshot`, `ToolDefinition`, `RuntimeError`, and the
versioned control request/response envelopes. These are the shared boundary for
the CLI daemon path and the MCP host adapter.

## Daemon And IPC

The daemon takes an exclusive per-runtime-directory lock, removes only stale
Unix socket files, loads the registry, then binds separate control and MCP
listeners. It writes secret-free `daemon.json` metadata only after both
listeners are bound. Unix uses mode-`0700` runtime directories and mode-`0600`
sockets/files; Windows uses deterministic local named-pipe names.

Control IPC is a request/response protocol:

- Every payload is JSON in a versioned `ControlRequestEnvelope` or
  `ControlResponseEnvelope`.
- Only this control connection uses framing: a four-byte big-endian `u32`
  payload length followed by a payload no larger than 8 MiB.
- The control protocol version is `1`; clients verify both version and request
  ID before accepting a response.
- Control operations are `ping`, `status`, server listing/inspection,
  connect/disconnect, tool list/refresh, single call, batch `call_tools`,
  explicit package install, OAuth start/complete/status/logout, runtime skill
  list/run, and shutdown.
  The protocol version remains `1`; older daemons do not understand additive
  request variants, so the CLI and daemon must be upgraded together.

The MCP IPC listener is deliberately different. It is an unframed bidirectional
byte stream. `mcp-host mcp` connects stdin/stdout directly to that stream with
two `tokio::io::copy` tasks. It is a transparent raw MCP byte relay: it neither
parses, terminates, reframes, nor adds a newline protocol. In particular, it
does **not** convert newline-delimited MCP into length-delimited IPC. The
daemon, not the bridge, terminates the MCP session through RMCP.

## Inbound MCP Surface

Each MCP socket gets a fresh `HostMcpServer` RMCP `ServerHandler` backed by the
daemon's shared `RuntimeManager`. Its tool list is fixed and deterministic:

- `list_servers`
- `inspect_server`
- `connect_server`
- `disconnect_server`
- `list_tools`
- `call_tool`
- `call_tools`
- `status`
- `refresh_server`

`call_tool` requires `server_id`, `tool_name`, and object-valued `arguments`.
`call_tools` accepts 1 through 32 already-connected call items and returns
`structuredContent.data.results` inside the stable `dynamic-mcp/v1` envelope.
It runs them concurrently while retaining input order; individual runtime
failures stay in that result array. `call_tool` preserves downstream LLM-facing
fields and nests the complete raw result under `data.result`. There is no dynamic
injection of discovered tools into the host tool list and no
`tools/list_changed` publication. CLI commands dispatch the same manager
operations through control IPC.

## Lifecycle And Concurrency

Every registered server has a desired connection (`connected` or
`disconnected`) and an observed lifecycle state (`registered`, `starting`,
`initializing`, `connected`, `disconnected`, `stopped`, or `failed`). A failed
transport remains desired-connected until an explicit disconnect. When enabled
in the manifest, automatic recovery performs at most the configured
number of capped exponential-backoff attempts with deterministic jitter.

Runtime state is isolated per server. A server-local Tokio `Mutex` protects only
short lifecycle state transitions and extraction/replacement of a session; no
network I/O is held under a global lock. Concurrent connects/disconnects for
the same server form a single flight: an operation has an epoch and
`CancellationToken`, waiters use `Notify`, and a stale completion cannot replace
a newer operation. Different servers connect and stop independently. Tool calls
clone the connected RMCP peer and run outside the lifecycle critical section.
Batch calls use `join_all` over those independent call futures, including calls
targeting the same server, and reorder results to their original input positions.

Disconnect during startup cancels the connect token, joins its completion, and
then reports the resulting stopped state. An unexpected transport close marks a
desired-connected session as failed and marks its cached tools stale.

## Protocol And SDK Baselines

V1 targets the stable [MCP 2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle), [tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools), and [transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) specifications.
The workspace pins [`rmcp = 3.0.0-beta.2`](../crates/mcp-host-mcp/Cargo.toml)
exactly; the corresponding [versioned API documentation](https://docs.rs/rmcp/3.0.0-beta.2/rmcp/),
[crate metadata](https://crates.io/crates/rmcp/3.0.0-beta.2), and
[release](https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v3.0.0-beta.2)
are the SDK sources for this implementation.

The beta SDK does not change the wire baseline: `ClientEvents::get_info`
explicitly advertises stable MCP `2025-11-25`. RMCP OAuth support is enabled for
authorization-code PKCE, while draft `2026-07-28` protocol, tasks, lifecycle
extensions, and elicitation remain disabled.

## Explicit V1 Scope

V1 intentionally has no implicit connection on tool list or call, marketplace,
plugins, dynamic host tools, or downstream semantic routing. Registry/policy/
skill hot reload, bounded reconnect, policy enforcement, explicit package
installation, HTTP OAuth PKCE, and linear runtime skill execution are
control-plane features; none changes the fixed nine-tool Host MCP surface.

Native service management is also outside the Host MCP surface. systemd and
launchd are invoked with shell-free argv after managed-artifact checks. Windows
SCM uses a safe service API wrapper, LocalSystem auto-start registration, a
managed display-name marker, and a hidden service dispatcher that converts SCM
STOP/SHUTDOWN into daemon root cancellation.

Stdio children are isolated through Unix process groups or Windows Job Objects.
Native runtime behavior still requires platform-specific verification.
