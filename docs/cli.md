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
| `--json` | Write successful command output as compact JSON. Without it, successful values are pretty-printed JSON, except for the display commands listed below, which render human-readable tables or lines. Runtime errors are JSON on stdout only with this flag, except for `mcp`. |
| `--timeout <MS>` | Control request deadline in milliseconds, inclusive range `1..=300000`; default is 65 seconds, except OAuth login waits up to 300 seconds. For `call`, the same optional value is also sent to the daemon as the downstream tool timeout. Batch control deadlines are derived from their items. |
| `-h`, `--help` | Show Clap-generated help for the selected command. `--help` includes detailed examples where available. |
| `-V`, `--version` | Show the binary version. |

There is no implicit daemon startup by normal control commands. Every command
other than `daemon run`, `daemon service`, and `harness install` requires the
selected runtime directory's daemon endpoint to already be live. `mcp` also
requires it, but uses its own fixed five-second connect deadline rather than
`--timeout`. Harness child CLIs use a fixed 60-second deadline.

## Human-Readable Output

Without `--json`, these commands render friendly output instead of JSON:

| Command | Human format |
| --- | --- |
| `list` / `ls` | Table of ID, name, state, tool count, and transport. |
| `tools` / `t` | Sorted table of tool names and descriptions; appends a stale note when the snapshot is stale. |
| `status` / `st` | Key-value lines for version, protocol, uptime, server counts, sessions, and endpoint readiness. `--stats` appends a durable usage table (calls, errors, last use, projects). |
| `skill list` / `sk list` | Table of skill ID, name, and step count. |
| `connect` / `c` | One line, e.g. `fixture: connected (5 tools, MCP 2025-11-25)`. |
| `disconnect` / `dc` | One line, e.g. `fixture: disconnected`. |

Every other command keeps JSON output; add `--json` at any time for the
machine-readable form.

## Commands

| Command | Control operation | Example |
| --- | --- | --- |
| `daemon run --config-dir <DIR> [--opencode-serve-url <URL>]` | Start the foreground daemon and load manifests from `<DIR>`. `--opencode-serve-url` registers a per-server MCP proxy with a running `opencode serve` instance on every connect. | `mcp-host --runtime-dir /run/user/1000/mcp-host daemon run --config-dir ./config --opencode-serve-url http://127.0.0.1:4096` |
| `daemon status` | Read daemon status. | `mcp-host --json daemon status` |
| `daemon stop` | Request orderly daemon shutdown. | `mcp-host daemon stop` |
| `daemon service install --config-dir <DIR> [--manager <...>] [--scope <...>] [--no-start]` | Install/repair, enable/load, and normally start the native service. | `mcp-host daemon service install --config-dir ./config` |
| `daemon service uninstall --config-dir <DIR> [--manager <...>] [--scope <...>]` | Stop, disable/unload, and remove a managed native service. | `mcp-host daemon service uninstall --config-dir ./config` |
| `daemon service status --config-dir <DIR> [--manager <...>] [--scope <...>]` | Read artifact ownership/drift plus loaded, enabled, and active state. | `mcp-host --json daemon service status --config-dir ./config` |
| `list` (alias `ls`) | List configured servers. | `mcp-host list` |
| `inspect <SERVER_ID>` (alias `i`) | Inspect one server. | `mcp-host inspect filesystem` |
| `connect <SERVER_ID>` (alias `c`) | Connect one server; records the current working directory as a project label in durable usage memory. | `mcp-host connect filesystem` |
| `disconnect <SERVER_ID>` (alias `dc`) | Disconnect one server. | `mcp-host disconnect filesystem` |
| `tools <SERVER_ID> [--refresh]` (alias `t`) | List a server's tools; `--refresh` refreshes first. | `mcp-host tools filesystem --refresh` |
| `refresh <SERVER_ID>` (alias `rf`) | Refresh one server. | `mcp-host refresh filesystem` |
| `call <SERVER_ID> <TOOL_NAME> [--arguments <JSON> \| --arguments-file <PATH>] [--no-auto-connect] [--no-retry] [--max-output-tokens <N>]` (alias `ca`) | Invoke a tool. Implicitly connects a registered but disconnected server, recovers from stale caches with one refresh-retry pass, and reports close-name suggestions on `TOOL_NOT_FOUND`. `--max-output-tokens` caps the serialized result (4 bytes/token). | `mcp-host call filesystem read_file --arguments '{"path":"README.md"}'` |
| `batch --calls <JSON_ARRAY> \| --calls-file <PATH\|->` (alias `b`) | Invoke 1 through 32 already-connected tools in parallel. | `mcp-host batch --calls '[{"server_id":"fixture","tool_name":"echo"}]'` |
| `status [--stats]` (alias `st`) | Read daemon status; equivalent control operation to `daemon status`. `--stats` adds the per-server usage table. | `mcp-host status --stats` |
| `auth login <SERVER_ID>` | Start authorization-code PKCE and complete it through an ephemeral loopback callback. | `mcp-host auth login remote` |
| `auth status <SERVER_ID>` | Show whether local OAuth credentials exist and list granted scopes without exposing tokens. | `mcp-host auth status remote` |
| `auth logout <SERVER_ID>` | Disconnect the server and remove local OAuth credentials. | `mcp-host auth logout remote` |
| `skill list` | List runtime skills allowed by `skill_run` policy. | `mcp-host skill list` |
| `skill run <SKILL_ID> [--input <JSON> \| --input-file <PATH\|->]` | Run 1-16 tool steps sequentially and fail fast. | `mcp-host skill run issue-notify --input '{"title":"Bug"}'` |
| `package install <SERVER_ID>` | Explicitly install the exact package declared by the manifest. | `mcp-host package install remote` |
| `harness install <TARGET>` | Register the stdio bridge with `opencode`, `claude-code`, or `all`. | `mcp-host harness install all` |
| `mcp [--endpoint <PATH>]` | Bridge stdin/stdout to the daemon's MCP endpoint; `--endpoint` selects a per-server proxy socket. | `mcp-host mcp --endpoint /run/user/1000/mcp-host/mcp-fixture.sock` |
| `shell` | Start an interactive terminal session. | `mcp-host shell` |
| `completions <SHELL>` | Print a completion script for `bash`, `zsh`, `fish`, `powershell`, or `elvish`. | `mcp-host completions fish > ~/.config/fish/completions/mcp-host.fish` |
| `doctor` | Run installation, daemon, and harness health checks. | `mcp-host doctor` |
| `init [--dir <DIR>] [--force]` | Scaffold a starter configuration directory (default `./config`). | `mcp-host init --dir config` |

`--config-dir` belongs to `daemon run` and each `daemon service` subcommand; it
is required and has no default.
`--refresh` belongs only to `tools`. `call` requires the positional server ID
and tool name. `--arguments` and `--arguments-file` are mutually exclusive.

## Interactive Shell

`mcp-host shell` starts a persistent terminal session. It reuses the same
commands as the one-shot CLI, including aliases, with these extras:

- Tab completion for command names, aliases, registered server IDs, discovered
  tool names (after `call <SERVER>`), and common flags.
- A `help` command listing the available commands.
- `exit`, `quit`, or Ctrl-D to leave the session.
- Shell history stored under the platform state directory
  (`~/.local/state/mcp-host/history.txt` on Linux).

When stdin is not a terminal, the shell reads one command per line, so it can
be scripted:

```bash
printf 'c fixture\ncall fixture echo --arguments '"'"'{"message":"hi"}'"'"'\nexit\n' \
  | mcp-host shell --runtime-dir /path/to/runtime
```

Stdin-based input flags (`--arguments-file -`, `--calls-file -`,
`--input-file -`) are rejected inside the shell because stdin is the shell
itself.

## Doctor

`mcp-host doctor` checks, without modifying anything:

- the binary and its version,
- whether `mcp-host` on `PATH` resolves to the running binary,
- the runtime directory,
- daemon reachability and version agreement,
- the OpenCode and Claude Code `dynamic-mcp` skill installation.

Each check is reported as `[ok]`, `[warn]`, or `[error]`; the exit status is
`0` when no check is an error and `4` otherwise. `--json` prints the complete
report as one JSON object.

## Init

`mcp-host init [--dir <DIR>] [--force]` writes a starter configuration
directory containing `example.toml` (a documented stdio manifest),
`policy.toml`, and a short `README.md`. Existing files are never overwritten
unless `--force` is passed.

## Native Service Management

`--manager auto` selects systemd on Linux, launchd on macOS, and native Windows
SCM on Windows. Scope defaults to `user` for systemd/launchd and `system` for
Windows SCM. Windows explicitly rejects user scope. All manager calls use exact
argv without a shell.

`install` enables and starts by default. `--no-start` still writes/enables a
systemd or Windows service but does not start it; for launchd it writes the
LaunchAgent/LaunchDaemon without bootstrapping it. A normal reinstall restarts a
loaded service to guarantee current descriptor argv is active. `uninstall`
checks ownership before any manager mutation, then stops/unloads and removes.

Successful status JSON has this stable shape:

```json
{
  "artifact": "installed",
  "loaded": true,
  "enabled": true,
  "active": "running",
  "descriptor": "/path/or/service-name"
}
```

`artifact` is `not_installed`, `installed`, `drifted`, or `foreign`; `active` is
`running`, `stopped`, `failed`, or `unknown`. These states return exit status 0
when the query itself succeeds. Manager absence, permission denial, and query
failure return `SERVICE_MANAGER_UNAVAILABLE`, `SERVICE_PERMISSION_DENIED`, or
`SERVICE_OPERATION_FAILED`; attempted mutation of a foreign service returns
`SERVICE_FOREIGN`.

Systemd user scope runs `systemctl --user`; Dynamic MCP does not modify linger
settings. Launchd uses `gui/<uid>` for user agents and `system` for daemons.
Windows uses an auto-start LocalSystem SCM service with a hidden native service
dispatcher; STOP/SHUTDOWN controls trigger the daemon's normal bounded cleanup.

## OAuth Login

`auth login` opens a listener only on `127.0.0.1` with an OS-assigned port,
asks the daemon to build the authorization URL, and prints that URL to stderr.
Open it in a browser. The CLI accepts one matching `/callback` request, sends
the complete callback URL to the daemon for PKCE/state/issuer validation and
token exchange, then returns secret-free status JSON. Authorization and callback
URLs are never included in runtime errors or daemon logs.

Each accepted loopback connection has a five-second request-header deadline;
silent or malformed local connections receive an error response and do not
consume the complete login attempt.

The daemon must remain running for the whole login. A login expires after five
minutes, a second simultaneous login is rejected, and login is rejected while
that server is connected. `auth logout` disconnects first so an in-flight token
refresh cannot recreate a deleted credential file.

## Runtime Skills

`skill list` and `skill run` are control-plane commands; they do not expand the
fixed Host MCP tool list. Input defaults to `{}` and must be a JSON object.
`--input-file -` reads stdin. Successful and failed step runs both print the
structured result so completed-step outputs remain available. CLI status is `5`
when a downstream step returns `isError: true`, `4` for another failed step, and
`0` when every step succeeds. See [Runtime Skills](runtime-skills.md) for the
TOML, template, policy, reload, and output contracts.

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

# Register a daemon-bootstrap or supervisor wrapper as the complete command
mcp-host harness install opencode \
  --bridge-command ~/.local/libexec/mcp-host/serve-bridge
```

| Option | Meaning |
| --- | --- |
| `<TARGET>` | `opencode`, `claude-code`, or `all`. |
| `--name <NAME>` | Stored MCP server name. Default: `dynamic-mcp`. Names accept 1-64 ASCII letters, digits, hyphens, underscores, or dots. |
| `--scope <SCOPE>` | Claude Code scope: `local`, `project`, or `user`. Default: `user`. Ignored for OpenCode. |
| `--runtime-dir <DIR>` | Store an absolute runtime override in the registered bridge command. Relative input is resolved from the installation command's working directory. Omit it to use the platform default. |
| `--bridge-command <PATH>` | Register this canonicalized executable as the complete bridge command instead of the current `mcp-host ... mcp` argv. It cannot be combined with `--runtime-dir`. |
| `--bridge-arg <ARG>` | Append one argument to `--bridge-command`; repeat for multiple arguments. No implicit `mcp` argument is added. |

OpenCode writes MCP registrations globally. Claude Code rejects duplicate
names, so a mismatched Claude registration is removed and recreated only in the
selected scope. Before invoking either CLI, `mcp-host` semantically checks the
resolved config; an exact name, transport, command, and argv match is not
rewritten. OpenCode verification follows the runtime's deep merge order:
`config.json`, `opencode.json`, then `opencode.jsonc`. After an update it reads
the effective OpenCode or Claude scope back and rejects a false-success CLI exit.
Claude's `local` scope has higher
precedence than `project`, which has higher precedence than `user`; a same-name
higher-scope entry can therefore mask a newly installed user entry.

Each concrete harness also receives the embedded `dynamic-mcp` skill and a
marked instruction block. OpenCode uses `$XDG_CONFIG_HOME/opencode` on every
platform, falling back to `~/.config/opencode`; Claude Code uses
`~/.claude/skills/dynamic-mcp/SKILL.md` and `~/.claude/CLAUDE.md`. Existing text
outside `<!-- dynamic-mcp:start -->` and `<!-- dynamic-mcp:end -->` is preserved.
Skill and instruction files are content-idempotent and atomically replaced.

The selected harness CLI must be on `PATH` only when a registration needs to be
created or repaired. Harness configuration does not start the daemon. Successful
JSON output contains the installed harnesses, exact bridge argv, verification
and update flags, config/skill/instruction paths, and `"daemonRequired": true`.

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
