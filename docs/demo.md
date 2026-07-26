# V1 Demo

This demo is offline. It uses the repository's real RMCP fixture server and
exercises the daemon, control IPC, Runtime Manager, upstream MCP client, and CLI.

## Build

Run from the repository root:

```bash
cargo build --workspace
export PATH="$PWD/target/debug:$PATH"
rm -rf target/mcp-host-runtime
```

PowerShell users can set:

```powershell
cargo build --workspace
$env:PATH = "$PWD\target\debug;$env:PATH"
Remove-Item -Recurse -Force target\mcp-host-runtime -ErrorAction SilentlyContinue
```

## Terminal 1: Daemon

```bash
mcp-host daemon run \
  --config-dir config/demo \
  --runtime-dir target/mcp-host-runtime
```

Leave this process running.

## Terminal 2: CLI Flow

```bash
mcp-host daemon status --runtime-dir target/mcp-host-runtime --json

mcp-host list --runtime-dir target/mcp-host-runtime --json

mcp-host connect fixture --runtime-dir target/mcp-host-runtime --json

mcp-host tools fixture --runtime-dir target/mcp-host-runtime --json

mcp-host call fixture echo \
  --arguments '{"message":"MCP Host works"}' \
  --runtime-dir target/mcp-host-runtime \
  --json

mcp-host call fixture add \
  --arguments '{"a":2,"b":3}' \
  --runtime-dir target/mcp-host-runtime \
  --json

mcp-host refresh fixture --runtime-dir target/mcp-host-runtime --json

mcp-host inspect fixture --runtime-dir target/mcp-host-runtime --json

mcp-host status --runtime-dir target/mcp-host-runtime --json

mcp-host disconnect fixture --runtime-dir target/mcp-host-runtime --json

mcp-host daemon stop --runtime-dir target/mcp-host-runtime --json
```

Expected evidence includes:

- `connect` reports `state: "connected"` and five tools.
- `tools` includes `echo`, `add`, `sleep`, `fail`, and `crash`.
- `echo` returns `structuredContent.message` unchanged.
- `add` returns `structuredContent.sum` equal to `5`.
- `inspect` exposes no environment or header values.
- `daemon stop` returns `{"accepted":true}` and Terminal 1 exits.
- `control.sock`, `mcp.sock`, `daemon.json`, and `daemon.lock` are removed on
  Unix after clean shutdown.

## MCP Client Flow

Start the daemon again, then configure an MCP client to execute:

```bash
/absolute/path/to/target/debug/mcp-host mcp \
  --runtime-dir /absolute/path/to/target/mcp-host-runtime
```

The Host MCP Server advertises exactly eight tools. A client should call:

```text
list_servers
inspect_server
connect_server
list_tools
call_tool
refresh_server (when needed)
disconnect_server
```

The automated test `real_rmcp_client_reaches_upstream_through_bridge_and_daemon`
executes this exact real-process chain with RMCP:

```text
RMCP client -> mcp-host mcp -> MCP IPC -> daemon -> RuntimeManager
            -> fixture MCP process -> echo result -> same path back
```

See [`mcp-host-server.md`](mcp-host-server.md) for tool schemas and
[`testing.md`](testing.md) for the executable evidence suite.
