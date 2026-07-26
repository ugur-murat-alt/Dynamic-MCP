# CLI

The binary is `mcp-host`.

```text
mcp-host [--runtime-dir <DIR>] [--json] [--timeout <MS>] <command>
```

Global flags are accepted before or after subcommands. Examples below place
them before the command for consistency.

| Flag | Meaning |
| --- | --- |
| `--runtime-dir <DIR>` | Directory containing the daemon lock, metadata, and endpoint state. Defaults on Unix to `$XDG_RUNTIME_DIR/mcp-host` when `XDG_RUNTIME_DIR` is non-empty; otherwise it uses the platform local-data directory plus `runtime`. |
| `--json` | Write successful command output as compact JSON. Without it, successful values are pretty-printed JSON. Runtime errors are JSON on stdout only with this flag, except for `mcp`. |
| `--timeout <MS>` | Control request deadline in milliseconds, inclusive range `1..=300000`; default is 65 seconds. For `call`, the same optional value is also sent to the daemon as the downstream tool timeout. Batch control deadlines are derived from their items. |
| `-h`, `--help` | Show Clap-generated help for the selected command. `--help` includes detailed examples where available. |
| `-V`, `--version` | Show the binary version. |

There is no implicit daemon startup or autostart. Every command other than
`daemon run` and `harness install` requires the selected runtime directory's
daemon endpoint to already be live. `mcp` also requires it, but uses its own
fixed five-second connect deadline rather than `--timeout`. Harness child CLIs
use a fixed 60-second deadline.

## Commands

| Command | Control operation | Example |
| --- | --- | --- |
| `daemon run --config-dir <DIR>` | Start the foreground daemon and load manifests from `<DIR>`. | `mcp-host --runtime-dir /run/user/1000/mcp-host daemon run --config-dir ./config` |
| `daemon status` | Read daemon status. | `mcp-host --json daemon status` |
| `daemon stop` | Request orderly daemon shutdown. | `mcp-host daemon stop` |
| `list` | List configured servers. | `mcp-host list` |
| `inspect <SERVER_ID>` | Inspect one server. | `mcp-host inspect filesystem` |
| `connect <SERVER_ID>` | Connect one server. | `mcp-host connect filesystem` |
| `disconnect <SERVER_ID>` | Disconnect one server. | `mcp-host disconnect filesystem` |
| `tools <SERVER_ID> [--refresh]` | List a server's tools; `--refresh` refreshes first. | `mcp-host tools filesystem --refresh` |
| `refresh <SERVER_ID>` | Refresh one server. | `mcp-host refresh filesystem` |
| `call <SERVER_ID> <TOOL_NAME> [--arguments <JSON> | --arguments-file <PATH>]` | Invoke a tool. | `mcp-host call filesystem read_file --arguments '{"path":"README.md"}'` |
| `batch --calls <JSON_ARRAY> | --calls-file <PATH\|->` | Invoke 1 through 32 already-connected tools in parallel. | `mcp-host batch --calls '[{"server_id":"fixture","tool_name":"echo"}]'` |
| `status` | Read daemon status; equivalent control operation to `daemon status`. | `mcp-host status` |
| `harness install <TARGET>` | Register the stdio bridge with `opencode`, `claude-code`, or `all`. | `mcp-host harness install all` |
| `mcp` | Bridge stdin/stdout to the daemon's MCP endpoint. | `mcp-host mcp` |

`--config-dir` belongs to `daemon run`; it is required and has no default.
`--refresh` belongs only to `tools`. `call` requires the positional server ID
and tool name. `--arguments` and `--arguments-file` are mutually exclusive.

## Batch Calls

`batch` requires exactly one of `--calls <JSON_ARRAY>` and
`--calls-file <PATH|->`; `-` reads the JSON array from stdin. The array has 1
through 32 items. Every item has this shape:

```json
{
  "server_id": "fixture",
  "tool_name": "echo",
  "arguments": { "message": "hello" },
  "timeout_ms": 1000
}
```

`arguments` defaults to `{}` and `timeout_ms` is optional. Every target must
already be connected: batch never connects a server implicitly. The daemon runs
all items concurrently and returns items in input order. An item runtime failure
has `"status":"error"` with a safe `RuntimeError`; it does not cancel the
other items. A completed upstream invocation has `"status":"success"` and
preserves its original `content`, `structuredContent`, `isError`, and `_meta`.
An upstream `isError: true` is therefore a successful transport result, not an
item runtime error.

The batch control deadline is the greater of the base control timeout and the
longest explicit or effective item timeout plus five seconds. The complete
control request and response must each fit the 8 MiB IPC frame limit.

## Harness Installation

`harness install` invokes each harness's official CLI without a shell. The
current `mcp-host` executable and every bridge argument remain separate argv
values, so spaces in paths are safe.

```bash
# OpenCode global MCP configuration
mcp-host harness install opencode

# Claude Code for all projects owned by the current user
mcp-host harness install claude-code

# Claude Code shared through the current project's .mcp.json
mcp-host harness install claude-code --scope project

# Configure both harnesses
mcp-host harness install all --name dynamic-mcp
```

| Option | Meaning |
| --- | --- |
| `<TARGET>` | `opencode`, `claude-code`, or `all`. |
| `--name <NAME>` | Stored MCP server name. Default: `dynamic-mcp`. Names accept 1-64 ASCII letters, digits, hyphens, underscores, or dots. |
| `--scope <SCOPE>` | Claude Code scope: `local`, `project`, or `user`. Default: `user`. Ignored for OpenCode. |
| `--runtime-dir <DIR>` | Store an absolute runtime override in the registered bridge command. Relative input is resolved from the installation command's working directory. Omit it to use the platform default. |

OpenCode 1.18.4 writes MCP registrations globally and replaces an existing
entry with the same name. Claude Code 2.1.218 rejects duplicate names, so
`mcp-host` removes and recreates only the selected Claude scope. This makes the
command repeatable, but an existing same-name registration in that scope is
intentionally replaced. Claude's `local` scope has higher precedence than
`project`, which has higher precedence than `user`; a same-name higher-scope
entry can therefore mask a newly installed user entry.

The selected harness CLI must already be on `PATH`. Harness configuration does
not start the daemon. Successful JSON output contains the installed harnesses,
the exact bridge argv, and `"daemonRequired": true`.

## Tool Arguments

`call` accepts arguments in one of three ways:

| Input | Behavior | Example |
| --- | --- | --- |
| Omitted | Sends `{}`. | `mcp-host call filesystem list_dir` |
| `--arguments <JSON>` | Parses the inline JSON. | `mcp-host call filesystem read_file --arguments '{"path":"README.md"}'` |
| `--arguments-file <PATH>` | Reads JSON from a file. Use `-` for stdin. | `mcp-host call filesystem read_file --arguments-file request.json` |

The parsed value must be a JSON object. Invalid JSON, a non-object value, an
unreadable file, or combining the two argument flags is a runtime failure.

When tool output has top-level `"isError": true`, the command still writes the
returned value but exits with the upstream-tool exit status.

## Output And Exit Status

Ordinary successful commands write their value to stdout. `--json` uses compact
JSON; the default is indented JSON. Diagnostics use stderr. With `--json`, a
runtime error is written as `{"error": ...}` to stdout, except that `mcp`
always preserves stdout exclusively for bridged MCP protocol bytes and writes
errors to stderr.

| Status | Meaning |
| --- | --- |
| `0` | Success. |
| `2` | CLI usage/parsing error, including invalid or missing command arguments. |
| `3` | Daemon IPC is unavailable or the daemon is not running. |
| `4` | Other runtime failure, including invalid call arguments, configuration, protocol, or daemon failures. |
| `5` | `call`, or a batch with no item runtime error, returned a tool result with top-level `isError: true`. |

For `batch`, exit status `4` takes precedence when any item has
`status: "error"`; otherwise status `5` indicates one or more successful item
results with `isError: true`, and status `0` indicates neither condition.

`mcp` never prints CLI JSON or human output to stdout. Start the daemon first,
then let the MCP client own the bridge's stdin and stdout:

```console
mcp-host --runtime-dir /run/user/1000/mcp-host daemon run --config-dir ./config
# In another process, configure the MCP client to execute:
mcp-host --runtime-dir /run/user/1000/mcp-host mcp
```
