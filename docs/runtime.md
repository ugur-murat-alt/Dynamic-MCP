# V1 Runtime

`RuntimeManager` owns one `ServerRuntime` for each entry in the immutable
registry snapshot. It is constructed once by the daemon and is shared by every
control request and inbound MCP session.

```text
connect_server(id)
       |
       v
server-local state mutex -- start/join operation (epoch + CancellationToken)
       |                                      |
       |                                      +--> waiters use Notify
       v
transport-specific RMCP client
       |
       +--> initialize / notifications/initialized (RMCP serve_with_ct)
       +--> peer.list_all_tools() (all pages)
       v
atomic replacement of ToolSnapshot, then Connected
```

## Connection Semantics

`connect_server` never happens implicitly. `list_tools`, `refresh_server`, and
`call_tool` require an existing appropriate session; a disconnected server is
reported as such. Disabled entries cannot connect or call.

The first successful connection performs the RMCP initialization handshake and
then `list_all_tools`. The paginated discovery result is converted to a
`ToolSnapshot` and installed only after the entire result is available. The
snapshot includes a fetch timestamp, count, full tool definitions, and a stale
bit. A failed initial discovery does not create a connected session.

`list_tools(refresh = false)` returns the cache. If a snapshot still exists
after a disconnect or failure, it is returned as stale rather than discarded.
`refresh_server` and `list_tools(refresh = true)` call `list_all_tools`; a
successful refresh atomically replaces the snapshot, while a failed or timed-out
refresh retains the prior snapshot and marks it stale. RMCP
`notifications/tools/list_changed` also only marks the cache stale: it carries
no delta and V1 never advertises a dynamic host tool list.

## Per-Server Operation Model

Only one lifecycle operation is active for a server at a time. The local state
holds `Lifecycle`, optional `ManagedSession`, optional operation, and an epoch.
The operation records whether it is connect or disconnect plus a cancellation
token.

1. The first caller changes desired state and creates an operation with the next
   epoch.
2. Same-server callers observe that operation and wait on `Notify`; they do not
   start another process or shutdown.
3. I/O occurs after the lifecycle mutex is released.
4. Completion installs its result only if its epoch is still current. It then
   wakes waiters.

There is no runtime-wide mutex around connection, discovery, or tool I/O.
`RuntimeManager::shutdown` disconnects all registered servers concurrently.
Calls use a clone of the connected RMCP `Peer`, so unrelated tool requests may
proceed concurrently and do not hold the lifecycle mutex.

## Batch Invocation

`call_tools` accepts 1 through 32 call items and requires every referenced
server to be connected already. It never starts an implicit connection. The
runtime dispatches all items with `join_all`, so calls to the same server and to
different servers make real concurrent progress. Results are reordered to match
the input array.

Each item has `server_id`, `tool_name`, optional object `arguments` (default
`{}`), and optional `timeout_ms`. A runtime failure produces an item with
`status: "error"` and a safe `RuntimeError`; it does not cancel sibling calls.
A downstream response produces `status: "success"` and is preserved unchanged,
including `content`, `structuredContent`, `isError`, and `_meta`. In particular,
`isError: true` is a successful upstream transport response.

## Runtime Skill Invocation

`run_skill` clones one immutable definition from the atomically loaded skill
catalog and executes its 1-16 steps in order. Unlike batch invocation it is
strictly sequential and fail-fast. A skill-level `skill_run` policy check occurs
before execution; every step then calls the public `call_tool` path and therefore
rechecks normal server/tool call policy. A downstream `isError: true`, runtime
error, or unresolved template path stops the run and returns ordered partial
results. Runs are synchronous and are not retained in daemon state. See
[Runtime Skills](runtime-skills.md) for the file, template, result, and CLI
contracts.

## Stdio Upstream

For a `stdio` manifest, the manager creates an RMCP `TokioChildProcess` from a
Tokio command. The child inherits the daemon parent environment by default;
manifest environment entries are then applied as explicit overrides. The
manifest working directory is used when configured.

Child stderr is piped and drained continuously. The manager retains only a
bounded 8 KiB tail by default (`RuntimeSettings::stderr_tail_bytes`) and replaces
the configured, non-empty secret values with `<redacted>` before retaining
bytes. That tail is internal diagnostic state, not part of inspect/status or
wire errors.

On disconnect, the manager stops its monitor, invokes RMCP service closure with
the configured grace period (4 seconds by default), waits for stderr draining,
and treats failure to close in time as `SERVER_DISCONNECT_FAILED`. The RMCP
child transport performs graceful close, kill escalation, and reaping; the
end-to-end tests verify that the fixture PID exits. A cancelled initialization
also drops/cleans the incomplete transport and waits for its stderr task.

## HTTP Upstream

For an `http` manifest, the manager creates RMCP
`StreamableHttpClientTransport` with the configured URI and static manifest
headers. Header names and values are validated at connection time, and values
remain secret-safe outside that boundary.

When the manifest has `[auth]`, connect first loads credentials bound to the
resource URL and fails with `AUTH_REQUIRED` before opening the MCP transport when
none exist. The transport wraps reqwest with RMCP `AuthClient`, which injects a
Bearer token for every MCP request and refreshes it when expiry is less than 30
seconds away. Authorization-code PKCE discovery, dynamic registration or a
pre-registered client ID, callback exchange, status, and logout are exposed only
through control IPC and the CLI. PKCE state remains in memory; tokens are
atomically persisted below `RuntimeSettings::auth_root`. The daemon points that
root at the platform's durable local-data directory rather than ephemeral IPC
runtime storage.

Both `http://` and `https://` endpoints are supported through RMCP's native-TLS
reqwest client; an offline regression test verifies that an HTTPS endpoint
opens a TCP connection and begins a TLS ClientHello.

The same RMCP initialization and initial `list_all_tools` sequence applies to
HTTP. HTTP sessions have no child PID or stderr tail.

## Timeouts And Cancellation

Runtime defaults are exact and centralized in `RuntimeSettings`:

| Operation | Default | Bound / effect |
| --- | --- | --- |
| Transport connect, initialize, initial discovery | 15 seconds | A timeout fails the connection; the active stdio service also receives its connect cancellation token. |
| Tool call without `timeout_ms` | 60 seconds | RMCP cancellable request timeout. |
| Caller `timeout_ms` | caller value | Must be 1..=300000 ms and no greater than the configured 300-second maximum. |
| Batch control request | `max(base control timeout, longest effective item timeout + 5 seconds)` | Each control request and response remains limited to 8 MiB. |
| Explicit tool-cache refresh | 60 seconds | On failure or timeout, prior cache remains stale. |
| Graceful session close | 4 seconds | Failure is a disconnect failure. |

Tool calls use RMCP `send_cancellable_request` with
`PeerRequestOptions::with_timeout`; a timed-out call returns
`TOOL_CALL_TIMEOUT`. The request timeout is not reset by progress notifications
and the configured value is the total request allowance. Cancellation is
protocol-cooperative: a timeout asks the downstream server to stop, but cannot
prove that the tool's external side effects have stopped. The session remains
usable when the transport remains open.

## Failure Behavior

| Event | V1 behavior |
| --- | --- |
| Downstream process or transport closes unexpectedly | Desired-connected runtime becomes `failed`; cache becomes stale; configured reconnect policy applies capped exponential backoff with jitter. |
| OAuth credentials are absent or refresh requires authorization | Connect fails with `AUTH_REQUIRED`; automatic reconnect stops until login is completed. |
| Explicit reconnect from `failed` | Starts a new session and clears the prior safe error on start. |
| Disconnect during start/initialize | Cancels the connect flight, waits for cleanup, ends stopped. |
| Unknown, disabled, or disconnected server | Stable typed runtime error; no connection attempt is made except explicit connect. |
| Unknown tool | `TOOL_NOT_FOUND` based on the discovered snapshot. |
| Upstream tool returns an MCP tool error result | Returned unchanged as a valid MCP result, including `isError`, structured content, and `_meta`. |

## Structured Diagnostics

Daemon and runtime diagnostics use `tracing` on stderr. Control operations log
the request correlation ID, operation, request/response byte counts, duration,
success, and safe error code. Tool invocation events log a generated invocation
ID, server ID, tool name, argument/result byte counts, duration, success, and
safe error code. Process startup records the server ID, transport, and PID.

Logs never include control payloads, raw MCP messages, tool arguments, tool
results, environment values, HTTP header values, or unredacted child stderr.
The `mcp-host mcp` process reserves stdout exclusively for MCP bytes.

## Source Anchors

Implementation anchors: [`RuntimeManager`](../crates/mcp-host-mcp/src/runtime.rs),
[`HostMcpServer`](../crates/mcp-host-mcp/src/host.rs),
[daemon IPC and handlers](../crates/mcp-host-cli/src/daemon.rs), and the
[transparent bridge](../crates/mcp-host-cli/src/bridge.rs).

The normative protocol sources are the stable [MCP 2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle),
[tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools),
and [transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
documents. SDK behavior is tied to the exact [RMCP 3.0.0-beta.2 API](https://docs.rs/rmcp/3.0.0-beta.2/rmcp/)
and [3.0.0-beta.2 release](https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v3.0.0-beta.2),
not `latest` documentation.
