# Dynamic MCP Host

Dynamic MCP Host V1 is a long-running, registry-backed MCP runtime. It exposes
one fixed MCP server to AI clients while acting as an MCP client to manifest-
defined stdio and Streamable HTTP servers.

The v0.2.0 release upgrades the Rust SDK to RMCP 3.0.0-beta.2, adds a stable
`dynamic-mcp/v1` Host response envelope, registry/policy hot reload, bounded
reconnect, explicit package installation, HTTP OAuth PKCE, process isolation,
linear runtime skills, and managed harness skills/instructions. It retains the
fixed host tool surface, stable MCP `2025-11-25`, and control protocol v1.

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
`rmcp = "=3.0.0-beta.2"`. RMCP OAuth support is enabled for authorization-code
PKCE; draft protocol tasks, lifecycle extensions, and elicitation remain
disabled.

## Quick Install

Install the latest GitHub Release to `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/ugur-murat-alt/MCP-Host/main/install.sh | sh
```

Register the transparent bridge with an AI coding harness:

```bash
mcp-host harness install opencode
mcp-host harness install claude-code --scope user
# Or configure both:
mcp-host harness install all
```

The installer verifies the release archive's SHA-256 checksum before replacing
the binary. Harness setup stores the installed binary's canonical absolute
path, verifies the resulting config semantically, installs the `dynamic-mcp`
skill, and updates only a marked block in global `AGENTS.md` or `CLAUDE.md`.
It does not start the daemon; start `mcp-host daemon run` with your manifest
directory before an AI client launches the bridge.

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

To run it as a managed background service instead:

```bash
mcp-host daemon service install --config-dir config/demo
mcp-host daemon service status --config-dir config/demo
```

Linux uses systemd, macOS uses launchd, and Windows uses native SCM LocalSystem;
`--no-start` installs/enables without starting immediately. See
[Installation](docs/installation.md) and [CLI](docs/cli.md) for scope, lifecycle,
status, and privilege details.

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
For an OAuth-enabled HTTP manifest, run `mcp-host auth login <SERVER_ID>` before
connecting; see [Manifest Format](docs/manifest-format.md) and [CLI](docs/cli.md).

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
harness install skips an already exact registration, repairs a mismatch with
the official harness CLI, and then reads the config back before reporting
success. Skill and instruction files are content-idempotent and atomically
replaced. Use `--name <NAME>` to choose a name other than `dynamic-mcp`, and use
`--runtime-dir <DIR>` when the daemon uses a non-default runtime directory.
Use `--bridge-command <PATH>` with repeated `--bridge-arg <ARG>` values when an
existing harness must launch a daemon-bootstrap or supervisor wrapper instead
of invoking `mcp-host mcp` directly.

Once connected through a harness, agents should use the Dynamic MCP tools for
runtime work: `list_servers`, `inspect_server`, `connect_server`, `list_tools`,
`find_tool`, `call_tool`/`call_tools`, `refresh_server`, `disconnect_server`,
`status`, and — for servers that advertise them — `list_resources` and
`read_resource`. The surface is
agent-friendly: `call_tool` auto-connects registered servers, recovers from
stale tool caches with a single refresh-retry pass, attaches close-name
`suggestions` to `TOOL_NOT_FOUND` errors, and honors an optional
`max_output_tokens` budget. `list_servers` and `status --stats` expose durable
usage memory (call counts, failures, last use, project associations). The
terminal CLI is for installation, daemon bootstrap, and diagnostics when that
MCP surface is unavailable.

### OpenCode serve integration (native per-server tools)

When opencode runs in server mode (`opencode serve`), the daemon can register
each connected downstream server as its own MCP server at runtime, so agents
see native tool names (`echo`, `add`, ...) instead of a routing layer:

```
mcp-host daemon run --config-dir config --opencode-serve-url http://127.0.0.1:4096
```

On `connect_server` the daemon posts `mcp-host-<server>` to the running
`opencode serve` `/mcp` API, pointing at a per-server proxy socket
(`<runtime-dir>/mcp-<server>.sock`). On `disconnect_server` it posts the
matching disconnect and removes the socket. Registration is runtime-only:
restarting `opencode serve` drops the entry, and there is no remove endpoint in
this opencode version, so disconnect marks the server `disabled`.

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

## Runtime Skills

Root-level `*.skill.toml` files define 1-16 sequential, fail-fast tool calls.
Typed run inputs and outputs from earlier steps can be referenced without adding
dynamic Host tools:

```bash
mcp-host skill list
mcp-host skill run issue-notify --input '{"title":"Login fails"}'
```

Each run is synchronous and stateless, passes a skill-level policy gate, and
rechecks normal call policy for every step. See [Runtime Skills](docs/runtime-skills.md).

## 9. Shutdown

```bash
mcp-host disconnect fixture --runtime-dir target/mcp-host-runtime
mcp-host daemon stop --runtime-dir target/mcp-host-runtime
```

Daemon shutdown closes inbound MCP sessions, disconnects upstream sessions,
reaps fixture children, removes IPC endpoints and metadata, and releases the
singleton lock.

## Terminal Convenience

The CLI favors short, discoverable commands. Most commands have aliases
(`ls`, `c`, `dc`, `t`, `rf`, `ca`, `b`, `st`, `sk`, `pkg`, `d`, `a`, `h`),
and display commands render human-readable tables unless `--json` is given.

Three commands make everyday use easier:

```bash
# Interactive session with Tab completion for servers, tools, and flags
mcp-host shell

# Health check for the installation, daemon, PATH, and harness skills
mcp-host doctor

# Scaffold a starter config directory (example.toml + policy.toml + README)
mcp-host init --dir config

# Shell completions for bash, zsh, fish, powershell, or elvish
mcp-host completions zsh > /usr/local/share/zsh/site-functions/_mcp-host
```

See [CLI](docs/cli.md) for the full command reference, aliases, exit codes,
and the interactive shell contract.

## Documentation

- [Architecture](docs/architecture.md)
- [Installation](docs/installation.md)
- [Runtime](docs/runtime.md)
- [Runtime Skills](docs/runtime-skills.md)
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
cargo check --workspace --all-features --target aarch64-apple-darwin
```

The workspace uses Rust 1.97.1 and edition 2024. Windows and macOS
cross-compilation are compile checks, not claims that native named-pipe, Unix
socket, or process tests ran on those operating systems.

## V1 Scope

V1 intentionally excludes implicit package installation, semantic search,
automatic tool selection, databases, web UI, clustering, and
prompts/resources/sampling proxying. Hot reload, bounded reconnect, policy
enforcement, explicit package installation, OAuth, and linear runtime skills
remain daemon control-plane features and do not expand the fixed Host MCP tool
list.
