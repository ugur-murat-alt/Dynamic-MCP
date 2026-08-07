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
7. Read machine results from structuredContent.data in the dynamic-mcp/v1 envelope.
8. For call_tool and successful call_tools items, also inspect the downstream isError field.
```

## Fixed Tool Surface

These are the exact nine inbound tools. No downstream tool is added to this
list.

| Tool | Input summary | Output summary |
| --- | --- | --- |
| `list_servers` | No arguments. | Envelope `data.servers` contains `ServerSummary[]`: identity, public description, enabled state, transport, desired and observed state, tool count, and stale flag. |
| `inspect_server` | `{ "server_id": string }` | Envelope `data` is `ServerInspection`: secret-free public manifest, source path, transport and lifecycle state, protocol and upstream metadata, cached tool snapshot, safe error, PID, and connection timestamps. |
| `connect_server` | `{ "server_id": string }` | Envelope `data` is `ConnectResult`: server ID, lifecycle state, discovered tool count, protocol version, connection time, and tool snapshot. It initializes the downstream server and performs initial tool discovery. |
| `disconnect_server` | `{ "server_id": string }` | Envelope `data` is `DisconnectResult`: server ID, resulting lifecycle state, and disconnect time. It also cancels an in-progress startup when necessary. |
| `list_tools` | `{ "server_id": string, "refresh": boolean }`; `refresh` defaults to `false`. | Envelope `data` is `ToolSnapshot`: server ID, fetch time, count, tool definitions, and stale flag. Each definition includes input and optional output schemas plus optional metadata. |
| `call_tool` | `{ "server_id": string, "tool_name": string, "arguments": object, "timeout_ms"?: integer, "max_output_tokens"?: integer }`. `arguments` is required and must be a JSON object. `timeout_ms`, when supplied, must be 1 through 300000 milliseconds and must not exceed the host limit. `max_output_tokens`, when supplied, truncates oversized serialized output using a 4-byte-per-token estimate. | Preserves downstream top-level `content`, `isError`, and `_meta`; envelope `data.result` contains the complete raw downstream result. |
| `call_tools` | `{ "calls": [{ "server_id": string, "tool_name": string, "arguments"?: object, "timeout_ms"?: integer }] }`, with 1 through 32 items. `arguments` defaults to `{}`; each server must already be connected. | Envelope `data.results` contains each `success` result or safe runtime `error`, in input order. |
| `status` | No arguments. | Envelope `data` is `HostStatus`: daemon and protocol versions, start time and uptime, registry, connected, failed, and active-session counts, listener readiness, and shutdown state. |
| `refresh_server` | `{ "server_id": string }` | Envelope `data` is a `ToolSnapshot` fetched again from the connected downstream server. |

All nine tools advertise the same stable output schema. Successful
`structuredContent` has this outer shape:

```json
{
  "schema_version": "dynamic-mcp/v1",
  "operation": "list_servers",
  "ok": true,
  "data": {}
}
```

Host-produced operations also contain a short human-readable text content
block. `call_tool` instead preserves downstream text/image/resource content,
top-level `isError`, and `_meta`, while replacing only `structuredContent` with
the Host envelope. The original structured content remains under
`data.result.structuredContent`.

`call_tools` runs items in parallel but preserves input order. Each successful
item preserves the original downstream `content`, `structuredContent`,
`isError`, and `_meta`; `isError: true` is not a host error. Only a batch-level
validation failure, such as an empty or over-32-item `calls` array, becomes a
top-level MCP error. Item runtime failures remain result items and do not cancel
other calls.

## Results And Errors

`call_tool` serializes the downstream `CallToolResult` at the runtime boundary
and decodes that same value for the inbound response. This preserves upstream
content, `_meta`, and `isError`; the complete raw value, including original
`structuredContent`, is nested in the envelope. In particular, an upstream
tool-level error remains a valid MCP result with `isError: true`; it is not
converted into a host protocol error. Envelope `ok: true` means routing and
transport completed, not that the downstream tool reported success.

The downstream result is intentionally available both as normal MCP content
and inside `data.result`: clients that only consume content retain standard MCP
behavior, while agents can use the structured envelope. Use
`max_output_tokens` for large results to bound both representations; truncated
results are replaced with a safe preview and `truncated: true`.

Host runtime failures are converted to MCP `ErrorData` safely:

- Invalid routing and state errors (`INVALID_ARGUMENTS`, `SERVER_NOT_FOUND`,
  `SERVER_DISABLED`, `SERVER_NOT_CONNECTED`, `TOOL_NOT_FOUND`, and
  `TOOLS_NOT_DISCOVERED`) become `invalid_params` errors.
- Other runtime failures become internal errors.
- The `ErrorData` payload uses the same `dynamic-mcp/v1` outer fields with
  `ok: false`; `data.error` contains stable code, operation, optional server ID,
  and retryability. It does not contain tool arguments, source text, or raw
  error messages.
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
