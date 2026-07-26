# Daemon And Local IPC

`mcp-host daemon run` starts one foreground daemon. It loads manifest-defined
servers once, owns their runtime, and exposes two distinct local endpoints:

| Endpoint | Purpose | Wire format |
| --- | --- | --- |
| control | CLI lifecycle and runtime operations | One `u32` big-endian length prefix, then one JSON payload |
| MCP | MCP clients, usually through `mcp-host mcp` | Raw MCP bytes; no control framing |

The endpoints are never interchangeable. A control client cannot speak MCP on
the control endpoint, and an MCP client must not send control frames to the MCP
endpoint.

## Startup And Singleton Lock

The daemon runs in the foreground until it receives a shutdown event. Its
startup order is:

1. Create the runtime directory. On Unix it is set to mode `0700`.
2. Open `daemon.lock` (mode `0600` on Unix) and take an exclusive non-blocking
   `fs2` lock. Failure means another daemon owns the runtime directory.
3. Derive the deterministic control and MCP addresses, then remove stale Unix
   socket files while holding the lock.
4. Load manifests from `--config-dir`, build the registry, and create the
   runtime manager.
5. Bind the control endpoint, then the MCP endpoint. Unix socket mode is
   `0600` where the OS supports atomic socket-mode selection; otherwise the
   enclosing `0700` runtime directory provides the access boundary.
6. Write `daemon.json`, then mark both services ready and start their accept
   loops.

The daemon does not fork, daemonize, or start in the background. A caller that
needs a background daemon must manage the process itself.

`daemon.json` is written only after both listeners bind successfully. It is a
secret-free JSON record containing `pid`, `started_at_unix_ms`,
`control_protocol_version`, `control_endpoint`, `mcp_endpoint`, `config_dir`,
and `binary_version`. On Unix the file is created with mode `0600` and is
synced before the daemon becomes ready.

## Addresses And Stale State

Addresses are deterministic for a runtime directory:

| Platform | Control | MCP |
| --- | --- | --- |
| Unix | `<runtime-dir>/control.sock` | `<runtime-dir>/mcp.sock` |
| Windows | `mcp-host-<fnv1a64(runtime-dir)>-control` | `mcp-host-<fnv1a64(runtime-dir)>-mcp` |

On Unix, each socket path is limited to 100 bytes as a safety limit. The daemon
does not reclaim an existing listener name. After it owns `daemon.lock`, it
removes only existing filesystem entries that are sockets; a non-socket entry
at an endpoint path is left in place and binding fails. Cleanup of a previous
`daemon.json` happens when new metadata is written, and only if that path is a
regular file.

On Windows the endpoints use `interprocess` `GenericNamespaced` names (named
pipes). The implementation supplies no custom security descriptor, so the
listener uses the library/OS default descriptor. The source has deterministic
endpoint and framing tests, but no native Windows bind/connect runtime test;
do not treat the tests as verification of named-pipe permissions or runtime
behavior on Windows.

## Control Protocol

Control protocol version remains `1` in v0.1.2. Each connection carries exactly
one request and one response, then is closed. The payload is JSON framed as:

```text
4-byte unsigned big-endian payload length
UTF-8 JSON payload
```

The payload limit is 8 MiB, excluding the four-byte prefix. Oversized frames or
invalid JSON are rejected. A request envelope has this shape:

```json
{
  "protocol_version": 1,
  "request_id": "client-chosen-id",
  "request": { "type": "status" }
}
```

A response echoes the protocol version and request ID and contains exactly one
of `result` or `error`:

```json
{
  "protocol_version": 1,
  "request_id": "client-chosen-id",
  "result": {}
}
```

The daemon rejects an unsupported request version. Clients also reject a
response with a different version or request ID, or with neither/both `result`
and `error`.

Control request `type` values are `ping`, `status`, `list_servers`,
`inspect_server`, `connect_server`, `disconnect_server`, `list_tools`,
`call_tool`, `call_tools`, `refresh_server`, and `shutdown`. `call_tools` carries
`calls`, an array of 1 through 32 `{server_id, tool_name, arguments?,
timeout_ms?}` items. Its control deadline is the greater of the base control
timeout and the longest explicit or effective item timeout plus five seconds.
The request and response each remain subject to the 8 MiB frame limit.

Protocol v1 is not a forward-compatible feature negotiation mechanism: a
v0.1.0 daemon does not recognize `call_tools`. Upgrade both CLI and daemon to
v0.1.1 or later.

## MCP Endpoint

The MCP endpoint is deliberately not a control protocol. After accepting a
connection, the daemon gives its split stream to a new RMCP `HostMcpServer`
session. Bytes are read and written unchanged by the local transport; there is
no length prefix, JSON wrapper, or one-request limit added by this layer.

`mcp-host mcp` is a stdio bridge. It copies stdin to this endpoint and copies
endpoint bytes to stdout. It requires a running daemon, uses a fixed five-second
connect timeout, and does not start one. Its stdout is protocol-only, including
when an error occurs; diagnostics are written to stderr.

## Shutdown And Cleanup

Shutdown begins on Unix `Ctrl-C`/SIGINT, `SIGTERM`, or a version-valid control
`shutdown` request. For control shutdown, the daemon first returns
`{"accepted":true}` and then cancels. On non-Unix platforms, the implemented
signal path is `Ctrl-C`.

Once shutdown starts, the daemon marks control and MCP as unavailable, rejects
new non-status control work (only `ping`, `status`, and `shutdown` remain
allowed), cancels both accept loops and their active connection/session tasks,
and waits up to five seconds for each stage. It then waits up to five seconds
for downstream runtime shutdown. Finally it removes `daemon.json`, removes
stale Unix sockets, unlocks and removes `daemon.lock`.

If listener setup, manifest loading, or metadata writing fails, the same local
files and lock are cleaned up before the daemon returns an error. A downstream
shutdown failure or timeout is reported after cleanup.
