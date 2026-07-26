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
daemon. A daemon starts from one manifest snapshot and reads no configuration
again while it is running. Changing a manifest requires a daemon restart; V1
has no file watcher or registry hot reload.

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
  connect/disconnect, tool list/refresh/call, and shutdown.

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
- `status`
- `refresh_server`

`call_tool` requires `server_id`, `tool_name`, and object-valued `arguments`.
It forwards a valid downstream MCP tool result without semantic conversion.
There is no dynamic injection of discovered tools into the host tool list and
no semantic route per downstream tool. CLI commands dispatch the same manager
operations through control IPC.

## Lifecycle And Concurrency

Every registered server has a desired connection (`connected` or
`disconnected`) and an observed lifecycle state (`registered`, `starting`,
`initializing`, `connected`, `disconnected`, `stopped`, or `failed`). A failed
transport remains desired-connected until an explicit reconnect or disconnect;
V1 does not automatically restart it.

Runtime state is isolated per server. A server-local Tokio `Mutex` protects only
short lifecycle state transitions and extraction/replacement of a session; no
network I/O is held under a global lock. Concurrent connects/disconnects for
the same server form a single flight: an operation has an epoch and
`CancellationToken`, waiters use `Notify`, and a stale completion cannot replace
a newer operation. Different servers connect and stop independently. Tool calls
clone the connected RMCP peer and run outside the lifecycle critical section.

Disconnect during startup cancels the connect token, joins its completion, and
then reports the resulting stopped state. An unexpected transport close marks a
desired-connected session as failed and marks its cached tools stale.

## Protocol And SDK Baselines

V1 targets the stable [MCP 2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle), [tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools), and [transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) specifications.
The workspace pins [`rmcp = 2.2.0`](../crates/mcp-host-mcp/Cargo.toml) exactly;
the corresponding [versioned API documentation](https://docs.rs/rmcp/2.2.0/rmcp/),
[crate metadata](https://crates.io/crates/rmcp/2.2.0), and
[release](https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v2.2.0)
are the SDK sources for this implementation.

## Explicit V1 Scope

V1 intentionally has no registry hot reload, implicit connection on tool list
or call, automatic reconnect/retry, OAuth flow, authentication broker,
permissions, marketplace/installation, plugins, skills, dynamic host tools, or
downstream semantic routing. HTTP supports only manifest-configured static
headers; it does not obtain or refresh OAuth credentials.

The remaining accepted, non-blocking limitation is that child-process cleanup
does not manage descendant process groups or Windows Job Objects. It does not
change the implemented daemon, transport, lifecycle, or protocol contract.
