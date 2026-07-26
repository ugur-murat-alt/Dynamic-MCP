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
remain secret-safe outside that boundary. HTTP has no OAuth acquisition,
refresh, browser flow, or other authentication state in V1.

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
| Downstream process or transport closes unexpectedly | Desired-connected runtime becomes `failed`; cache becomes stale; no auto-restart. |
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
documents. SDK behavior is tied to the exact [RMCP 2.2.0 API](https://docs.rs/rmcp/2.2.0/rmcp/)
and [2.2.0 release](https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v2.2.0),
not `latest` documentation.
