# Testing

## Required Commands

Run these commands before accepting a change:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Installer and detailed-help smoke checks:

```bash
sh -n install.sh
./install.sh --help
cargo run -p mcp-host --bin mcp-host -- --help
cargo run -p mcp-host --bin mcp-host -- harness install --help
```

The workspace test command is the default test gate. It has no `#[ignore]`
tests, does not require Internet access, and uses temporary directories and
local fixtures. HTTP tests bind `127.0.0.1:0`, so their ports are ephemeral.
They do not depend on a fixed port or a remote service.

For Windows GNU and macOS compile checks, also run:

```bash
cargo build --target x86_64-pc-windows-gnu --release
cargo check --workspace --all-features --target x86_64-apple-darwin
```

These cross-compilation checks are not native Windows or macOS testing. They
prove that the targets compile, not that named pipes, Unix sockets, processes,
or runtime tests execute correctly. Run the suite on each OS for native
validation.

## Current Suite

The current suite contains **105 tests** under
`cargo test --workspace --all-features -- --list`: 86 unit tests and 19
integration or end-to-end tests. There are currently no doctests. This count is
an inventory of the current working tree and may change as the final suite
changes.

| Area | Count | Coverage |
| --- | ---: | --- |
| `mcp-host-core` unit | 44 | Lifecycle transitions and dispositions, manifest parsing, validation, loading, deterministic registry construction, protocol DTOs, and secret redaction. |
| `mcp-host-core` integration | 3 | Real manifest directories, duplicate IDs, example manifests, resolved values, and secret-free debug output. |
| `mcp-host-mcp` unit | 15 | Fixed Host Server tool surface and metadata, schema requirements, runtime error mapping, upstream `isError` preservation, session accounting, fixture behavior, runtime preconditions, and streaming stderr redaction. |
| CLI, harness, IPC, bridge, and daemon unit | 27 | Detailed help and harness parsing, shell-free harness argv generation, absolute runtime paths, exit codes, length-prefixed IPC framing, endpoint validation, bidirectional bridge byte forwarding and EOF draining, daemon metadata, protocol mismatch, and empty-registry control requests. |
| Real stdio runtime end-to-end | 7 | Downstream process initialization, discovery, calls, timeout reuse, concurrent lifecycle operations, cancellation ordering, crashes, reconnects, and shared shutdown. |
| Local Streamable HTTP end-to-end | 2 | HTTP initialization, resolved header delivery, secret redaction, paginated discovery, and stale cache preservation after refresh failure. |
| Daemon and CLI end-to-end | 5 | Persistent CLI state, complete RMCP bridge chain, shared runtime, daemon exclusivity, and active-session shutdown. |
| Harness CLI end-to-end | 2 | OpenCode and Claude Code argv separation, repeatable Claude scope replacement, JSON output, and missing harness errors without touching real config. |

## Runtime End-To-End Coverage

`crates/mcp-host-cli/tests/runtime_e2e.rs` runs a real stdio fixture process.
It verifies:

- Initialize, discover exactly five fixture tools, call successful and
  tool-level-error paths, disconnect, and child-process reaping.
- A timed-out `sleep` call returns `TOOL_CALL_TIMEOUT`; a subsequent `echo`
  call proves that the session remains reusable.
- Ten concurrent `connect_server` calls start one process, and ten concurrent
  `disconnect_server` calls join one shutdown.
- Two different servers connect independently and `RuntimeManager::shutdown`
  closes both.
- Disconnect during delayed initialization cancels startup and reaps the child.
- Connect callers already joined to that startup observe a later disconnect;
  they do not silently begin a second connection.
- A fixture `crash` moves the runtime to `Failed`; a later connect starts a new
  process and successfully routes another call.

`crates/mcp-host-mcp/tests/http_e2e.rs` runs real local Streamable HTTP RMCP
servers. It verifies an environment-resolved secret header is delivered to the
local service but absent from inspection debug output. It also verifies a
paginated tool listing is collected completely and that a failed refresh leaves
the prior snapshot available with `stale: true`.

`crates/mcp-host-cli/tests/daemon_e2e.rs` runs a real daemon, CLI subprocesses,
RMCP client -> stdio bridge -> daemon -> downstream fixture chain, one runtime
shared by the CLI and two MCP clients, rejection of a second daemon without
damaging the first, and shutdown while both upstream and downstream sessions are
active.

`crates/mcp-host-cli/tests/harness_cli.rs` launches fake OpenCode and Claude
executables. It verifies separate argv values, runtime paths containing spaces,
Claude scope and remove/add ordering, compact JSON output, and actionable errors
when a harness CLI is missing. The test does not modify real harness
configuration.

## Fixture Contract

The stdio fixture is `mcp-host-fixture-server`. Its five fixed tools are:

| Tool | Input | Behavior |
| --- | --- | --- |
| `echo` | `{ "message": string }` | Returns the message as text and structured content. |
| `add` | `{ "a": integer, "b": integer }` | Returns the signed 64-bit sum, or a tool-level error on overflow. |
| `sleep` | `{ "milliseconds": integer }` | Sleeps for at most 5000 milliseconds. |
| `fail` | No arguments. | Returns a caller-visible tool-level error. |
| `crash` | No arguments. | Exits the fixture process with status 86. |

`FixtureOptions` supports these deterministic test controls:

- `startup_counter_file`: records each fixture start for single-flight and
  reconnect assertions.
- `pid_file`: records the child PID for process cleanup assertions.
- `initialize_delay_ms`: delays initialization, capped at 5000 milliseconds,
  for startup-cancellation coverage.

## Secret Safety Tests

Secret-focused tests use sentinel values only in local fixtures and assertions.
They verify that raw and resolved manifest debug output redacts environment and
HTTP-header secrets, URL query strings are redacted, parse errors do not retain
source secrets, runtime errors omit tool arguments, daemon metadata contains no
configuration secrets, and HTTP inspection output does not reveal the supplied
header secret. Streaming stderr tests additionally split a sentinel secret
across read chunks and verify that both complete and end-of-stream partial
values are redacted.
