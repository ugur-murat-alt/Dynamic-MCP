# Host Server

Dynamic MCP Host exposes one stable inbound MCP server. It manages
manifest-defined downstream MCP servers as outbound clients; it does not expose
their tools directly as inbound tools.

## Metadata And Instructions

The server advertises tools capability only. Its implementation metadata is:

- Name: `mcp-host`
- Version: the package version
- Title: `Dynamic MCP Host`
- Description: `A long-running MCP runtime and process manager that presents one stable MCP server to AI clients while managing manifest-defined MCP servers as downstream clients.`
- Capabilities: `{ "tools": {} }`

The server does not advertise `tools.listChanged`. Its inbound tool list is
fixed for the life of the process, so it never dynamically publishes upstream
tools or sends a list-changed notification. A downstream `tools/list_changed`
notification only marks that downstream server's cached tool snapshot stale.
Clients use `refresh_server` or `list_tools` with `refresh: true` to obtain a
new snapshot.

The advertised instructions are:

```text
1. Call list_servers to discover available downstream MCP servers.
2. Call inspect_server to review a server's public configuration and current state.
3. Call connect_server before using a disconnected server.
4. Call list_tools after connecting to discover that server's available tools.
5. Call call_tool for one invocation or call_tools for up to 32 parallel invocations.
6. Use refresh_server when tools may have changed, then disconnect_server when the server is no longer needed.
```

## Fixed Tool Surface

These are the exact nine inbound tools. No downstream tool is added to this
list.

| Tool | Input summary | Output summary |
| --- | --- | --- |
| `list_servers` | No arguments. | Structured `{ "servers": ServerSummary[] }`, where each summary includes identity, public description, enabled state, transport, desired and observed state, tool count, and stale flag. |
| `inspect_server` | `{ "server_id": string }` | Structured `ServerInspection`: secret-free public manifest, source path, transport and lifecycle state, protocol and upstream metadata, cached tool snapshot, safe error, PID, and connection timestamps. |
| `connect_server` | `{ "server_id": string }` | Structured `ConnectResult`: server ID, lifecycle state, discovered tool count, protocol version, connection time, and tool snapshot. It initializes the downstream server and performs initial tool discovery. |
| `disconnect_server` | `{ "server_id": string }` | Structured `DisconnectResult`: server ID, resulting lifecycle state, and disconnect time. It also cancels an in-progress startup when necessary. |
| `list_tools` | `{ "server_id": string, "refresh": boolean }`; `refresh` defaults to `false`. | Structured `ToolSnapshot`: server ID, fetch time, count, tool definitions, and stale flag. Each tool definition includes input and optional output schemas plus optional metadata. |
| `call_tool` | `{ "server_id": string, "tool_name": string, "arguments": object, "timeout_ms"?: integer }`. `arguments` is required and must be a JSON object. `timeout_ms`, when supplied, must be 1 through 300000 milliseconds and must not exceed the host limit. | The original downstream MCP `CallToolResult`, not a host wrapper. |
| `call_tools` | `{ "calls": [{ "server_id": string, "tool_name": string, "arguments"?: object, "timeout_ms"?: integer }] }`, with 1 through 32 items. `arguments` defaults to `{}`; each server must already be connected. | `structuredContent.results`; each item is a `success` result or an `error` with `RuntimeError`, in input order. |
| `status` | No arguments. | Structured `HostStatus`: daemon and protocol versions, start time and uptime, registry, connected, failed, and active-session counts, listener readiness, and shutdown state. |
| `refresh_server` | `{ "server_id": string }` | Structured `ToolSnapshot` fetched again from the connected downstream server. |

All host-produced structured outputs contain both a short human-readable text
content block and `structuredContent`. The downstream result from `call_tool`
is the exception because it is passed through as an MCP result.

`call_tools` runs items in parallel but preserves input order. Each successful
item preserves the original downstream `content`, `structuredContent`,
`isError`, and `_meta`; `isError: true` is not a host error. Only a batch-level
validation failure, such as an empty or over-32-item `calls` array, becomes a
top-level MCP error. Item runtime failures remain result items and do not cancel
other calls.

## Results And Errors

`call_tool` serializes the downstream `CallToolResult` at the runtime boundary
and decodes that same value for the inbound response. This preserves upstream
content, `structuredContent`, `_meta`, and `isError`. In particular, an
upstream tool-level error remains a valid MCP result with `isError: true`; it is
not converted into a host protocol error.

Host runtime failures are converted to MCP `ErrorData` safely:

- Invalid routing and state errors (`INVALID_ARGUMENTS`, `SERVER_NOT_FOUND`,
  `SERVER_DISABLED`, `SERVER_NOT_CONNECTED`, `TOOL_NOT_FOUND`, and
  `TOOLS_NOT_DISCOVERED`) become `invalid_params` errors.
- Other runtime failures become internal errors.
- The `ErrorData` payload contains the serialized safe `RuntimeError` only
  when serialization succeeds. That DTO contains code, operation, optional
  server ID, message, retryability, and optional source summary; it does not
  contain tool arguments.
- If safe error serialization fails, the host returns the generic internal
  error `internal error` with no data rather than exposing an unsafe fallback.

## AI Flow

Use the host as a control plane for downstream MCP servers:

1. Call `list_servers` and choose a registered server.
2. Call `inspect_server` when its public configuration or current state matters.
3. Call `connect_server` before the first use of a disconnected server.
4. Call `list_tools` and select a discovered downstream tool from its schema.
5. Call `call_tool` with the selected `server_id`, `tool_name`, and object
   `arguments`.
6. Use `call_tools` when several already-connected calls should run in parallel.
7. Preserve and interpret an upstream `isError: true` result as a tool result,
   not as a transport failure.
8. Call `refresh_server` when tool availability may have changed, then use the
   refreshed snapshot before making another routed call.
9. Call `disconnect_server` when that downstream server is no longer needed.

`status` is available at any point to observe host and downstream runtime
health.
