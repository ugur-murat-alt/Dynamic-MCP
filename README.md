# Dynamic MCP Host

Dynamic MCP Host V1 is a long-running, registry-backed MCP runtime. It exposes
one fixed MCP server to AI clients while acting as an MCP client to manifest-
defined stdio and Streamable HTTP servers.

The v0.1.1 release adds parallel batch tool invocation while retaining the
fixed host tool surface and control protocol v1.

The host manages registration, connection lifecycle, tool discovery, caching,
refresh, invocation, timeout, and shutdown. It does not perform semantic
routing, choose tools, load skills, or inject upstream tools into its own tool
list.

## Workspace

- `mcp-host-core`: manifests, validation, immutable registry, lifecycle, runtime
  DTOs, and stable error codes.
- `mcp-host-mcp`: Runtime Manager, upstream RMCP clients, fixture server, and the
  fixed inbound Host MCP Server.
- `mcp-host`: foreground daemon, local IPC, CLI, and transparent stdio bridge.

V1 targets MCP `2025-11-25` and pins the official Rust SDK as
`rmcp = "=2.2.0"`.

## Quick Install

Install the latest GitHub Release to `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/ugur-murat-alt/Dynamic-MCP/main/install.sh | sh
```

Register the transparent bridge with an AI coding harness:

```bash
mcp-host harness install opencode
mcp-host harness install claude-code --scope user
# Or configure both:
mcp-host harness install all
```

The installer verifies the release archive's SHA-256 checksum before replacing
the binary. Harness registration stores the installed binary's canonical
absolute path. It does not start the daemon; start `mcp-host daemon run` with
your manifest directory before an AI client launches the bridge.

See [Installation](docs/installation.md) for pinned versions, custom install
directories, harness scopes, upgrades, and platform support.

## 1. Build From Source

From the repository root:

```bash
cargo build --workspace
export PATH="$PWD/target/debug:$PATH"
```

PowerShell equivalent:

```powershell
cargo build --workspace
$env:PATH = "$PWD\target\debug;$env:PATH"
```

Adding `target/debug` to `PATH` lets the demo manifest use the portable command
name `mcp-host-fixture-server` instead of a machine-specific absolute path.

## 2. Create A Manifest

The offline demo manifest is [`config/demo/fixture.toml`](config/demo/fixture.toml):

```toml
id = "fixture"
name = "Demo Fixture"
description = "Offline RMCP fixture server for the Dynamic MCP Host V1 demo"

[transport]
type = "stdio"
command = "mcp-host-fixture-server"
```

See [`docs/manifest-format.md`](docs/manifest-format.md) for stdio, HTTP,
working-directory, environment-reference, and secret-handling rules.

## 3. Start The Daemon

Terminal 1:

```bash
mcp-host daemon run \
  --config-dir config/demo \
  --runtime-dir target/mcp-host-runtime
```

The daemon runs in the foreground and owns all persistent runtime state. CLI
commands and MCP bridge processes do not create their own Runtime Manager.

## 4. List Servers

Terminal 2:

```bash
mcp-host list --runtime-dir target/mcp-host-runtime
```

## 5. Connect

```bash
mcp-host connect fixture --runtime-dir target/mcp-host-runtime
```

Connections are explicit. `tools`, `call`, and `batch` never connect silently.

## 6. List Tools

```bash
mcp-host tools fixture --runtime-dir target/mcp-host-runtime
```

## 7. Call A Tool

```bash
mcp-host call fixture echo \
  --arguments '{"message":"MCP Host works"}' \
  --runtime-dir target/mcp-host-runtime
```

Add `--json` to any ordinary CLI command for compact machine-readable JSON.

## 8. Add The Host To An AI Client

The daemon must already be running. Configure the AI client to execute only the
short-lived bridge. OpenCode and Claude Code can be configured directly:

```bash
mcp-host harness install opencode
mcp-host harness install claude-code --scope user
```

OpenCode writes its global MCP configuration. Claude Code supports `local`,
`project`, and `user` scopes; Dynamic MCP Host defaults to `user`. Re-running a
harness install replaces only the same named registration in the selected
scope. Use `--name <NAME>` to choose a name other than `dynamic-mcp`, and use
`--runtime-dir <DIR>` when the daemon uses a non-default runtime directory.

For VS Code, the officially documented workspace file is `.vscode/mcp.json`:

```json
{
  "servers": {
    "mcp-host": {
      "type": "stdio",
      "command": "/absolute/path/to/target/debug/mcp-host",
      "args": [
        "mcp",
        "--runtime-dir",
        "/absolute/path/to/target/mcp-host-runtime"
      ]
    }
  }
}
```

Source: [VS Code MCP configuration reference](https://code.visualstudio.com/docs/agents/reference/mcp-configuration).

`mcp-host mcp` is a transparent byte bridge. It does not parse MCP or run
business logic, and its stdout is reserved exclusively for protocol traffic.

## Batch Calls

After explicitly connecting every referenced server, invoke 1 through 32 tools
in one request:

```bash
mcp-host batch --calls '[
  {"server_id":"fixture","tool_name":"echo","arguments":{"message":"first"}},
  {"server_id":"fixture","tool_name":"echo","arguments":{"message":"second"}}
]'
```

Use `--calls-file <PATH>` (or `-` for stdin) instead of `--calls`; exactly one
input is required. Calls run in parallel, including calls to the same server,
but results retain input order. See [CLI](docs/cli.md) and
[Runtime](docs/runtime.md) for the input, timeout, result, and exit-status
contracts.

## 9. Shutdown

```bash
mcp-host disconnect fixture --runtime-dir target/mcp-host-runtime
mcp-host daemon stop --runtime-dir target/mcp-host-runtime
```

Daemon shutdown closes inbound MCP sessions, disconnects upstream sessions,
reaps fixture children, removes IPC endpoints and metadata, and releases the
singleton lock.

## Documentation

- [Architecture](docs/architecture.md)
- [Installation](docs/installation.md)
- [Runtime](docs/runtime.md)
- [Daemon and IPC](docs/daemon-and-ipc.md)
- [CLI](docs/cli.md)
- [Host MCP Server](docs/mcp-host-server.md)
- [Testing](docs/testing.md)
- [Full demo](docs/demo.md)
- [Registry](docs/registry.md)

## Development Gates

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release
cargo check --workspace --all-features --target x86_64-pc-windows-gnu
cargo check --workspace --all-features --target x86_64-apple-darwin
```

The workspace uses Rust 1.97.1 and edition 2024. Windows and macOS
cross-compilation are compile checks, not claims that native named-pipe, Unix
socket, or process tests ran on those operating systems.

## V1 Scope

V1 intentionally excludes hot reload, automatic reconnect/backoff, OAuth,
automatic installation of upstream server packages, permissions/RBAC,
semantic search, automatic tool selection, skills, databases, web UI,
clustering, and prompts/resources/sampling proxying. Manifest changes require
a daemon restart.
