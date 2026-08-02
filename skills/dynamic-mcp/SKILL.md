---
name: dynamic-mcp
description: Use Dynamic MCP tools to discover, connect, inspect, and invoke downstream MCP servers. Use whenever a task involves a server managed by Dynamic MCP; prefer these MCP tools over the mcp-host terminal CLI.
---

# Dynamic MCP

Use the Dynamic MCP Host tool surface for runtime work. Do not shell out to
`mcp-host` when the corresponding MCP tool is available.

## Required Workflow

1. Call `status` when host availability is uncertain.
2. Call `list_servers` to discover registered downstream servers.
3. Call `inspect_server` before connecting when transport or state matters.
4. Call `connect_server` explicitly when a server is disconnected.
5. Call `list_tools` to discover exact downstream tool names and schemas.
6. Call `call_tool` for one invocation or `call_tools` for independent parallel work.
7. Call `refresh_server` when a connected server's tools may have changed.
8. Call `disconnect_server` only when the server is no longer needed.

Never invent a downstream tool name or argument shape. `list_tools` is the
source of truth. Dynamic MCP does not connect a server implicitly.

## Tool Results

Successful Dynamic MCP control tools return this stable machine envelope in
`structuredContent`:

```json
{
  "schema_version": "dynamic-mcp/v1",
  "operation": "list_servers",
  "ok": true,
  "data": {}
}
```

Read `data` for machine decisions. The top-level MCP text content is a concise
human summary.

`call_tool` keeps the downstream tool's top-level `content`, `isError`, and
`_meta` for normal MCP/LLM behavior. Its `structuredContent.data.result`
contains the complete raw downstream result, including the original
`structuredContent`. An envelope with `ok: true` means routing and transport
completed; still check the downstream `isError` value.

Runtime failures are MCP errors with safe structured data containing the stable
error code, retryability, and optional server ID. Do not retry non-retryable
errors without changing the request or state.

## Parallel Calls

Use `call_tools` only for independent calls. It accepts 1 to 32 items, starts
them concurrently, and returns results in input order. One item-level runtime
error does not cancel siblings. Inspect every item status and every successful
downstream result's `isError` field.

## Recovery

- `SERVER_NOT_CONNECTED`: call `connect_server`, then retry.
- `TOOLS_NOT_DISCOVERED` or `TOOL_NOT_FOUND`: call `refresh_server` or
  `list_tools` with refresh, then use the returned schema.
- Stale tool snapshot: refresh before invoking a tool whose schema may have changed.
- Retryable transport/runtime error: inspect status, reconnect if needed, and
  retry only when the operation is safe.
- Non-retryable argument error: correct arguments from the discovered schema.

## CLI Boundary

The terminal CLI is reserved for installation, daemon bootstrap, harness
registration, and diagnostics when the MCP control surface is unavailable.
Runtime discovery, connection management, tool listing, and tool invocation
must use Dynamic MCP tools whenever they are exposed by the harness.
