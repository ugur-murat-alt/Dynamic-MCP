# Testing

## Required Commands

Run these commands before accepting a change:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
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
cargo clippy --workspace --all-targets --all-features \
  --target x86_64-pc-windows-gnu -- -D warnings
cargo check --workspace --all-features --target x86_64-apple-darwin
cargo check --workspace --all-features --target aarch64-apple-darwin
```

These cross-compilation checks are not native Windows or macOS testing. They
prove that the targets compile, not that named pipes, Unix sockets, processes,
service managers, or runtime tests execute correctly. Run the suite and native
service lifecycle checks on each OS for native validation.

## Current Suite

The v0.2.0 full suite contains **221 tests** under
`cargo test --workspace --all-targets --all-features -- --list`: 186 unit tests
and 35 integration or end-to-end tests, with 0 ignored. There are currently no
doctests.

| Area | Count | Coverage |
| --- | ---: | --- |
| `mcp-host-core` unit | 93 | Lifecycle transitions and dispositions, manifest/OAuth/package/reconnect parsing and validation, server/skill loading, templates, policy, deterministic registry construction, protocol DTOs, batch/OAuth/skill DTOs, and secret redaction. |
| `mcp-host-core` integration | 3 | Real manifest directories, duplicate IDs, example manifests, resolved values, and secret-free debug output. |
| `mcp-host-mcp` unit | 33 | Fixed Host Server surface and metadata, stable envelope schemas, safe errors, downstream result preservation, session accounting, OAuth credential paths/permissions, skill input/template resolution and atomic catalog reload, package behavior, runtime preconditions, and streaming stderr redaction. |
| Local Streamable HTTP end-to-end | 3 | HTTP initialization, resolved header delivery, secret redaction, paginated discovery, stale cache preservation after refresh failure, and an offline HTTPS TLS ClientHello probe. |
| OAuth Streamable HTTP end-to-end | 1 | RFC 9728 metadata, dynamic and pre-registered clients, PKCE S256, callback validation, token exchange, automatic refresh, Bearer MCP calls, secret-free status, and logout cleanup. |
| CLI, harness, IPC, bridge, daemon, service, and batch unit | 60 | Detailed help, skill exit semantics, OAuth loopback parsing and stalled-connection recovery, cross-platform XDG path resolution, three-layer deep OpenCode config verification, managed files, shell-free argv generation, service descriptor ownership/drift, systemd/launchd command ordering and launchd state parsing, external service cancellation, batch deadlines and exit precedence, IPC framing, endpoint validation, bridge byte forwarding, daemon metadata, and protocol mismatch. |
| Real stdio runtime end-to-end | 12 | Downstream initialization, discovery, calls, batch concurrency and isolation, timeout reuse, lifecycle cancellation, crashes, bounded reconnects, typed skill chaining, fail-fast tool errors, per-step policy, immutable in-flight skill snapshots, and shared shutdown. |
| Daemon and CLI end-to-end | 10 | Persistent CLI state, complete RMCP bridge chain, shared runtime, daemon exclusivity, atomic server/policy/skill hot reload, invalid skill snapshot retention, package/auth dispatch errors, and active-session shutdown. |
| Harness CLI end-to-end | 5 | OpenCode and Claude argv separation, exact/mismatch/idempotent config paths, explicit wrappers, post-write readback rejection, skill and managed instruction installation, and isolated missing-CLI errors. |
| Service CLI integration | 1 | Missing native service manager maps to the stable `SERVICE_MANAGER_UNAVAILABLE` runtime code without mutating an artifact. |

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
- A batch with two 500 ms sleeps and an echo completes concurrently, keeps input
  order, and leaves sibling results available when one item has a runtime error.
- A two-step skill preserves typed input, passes the first raw structured output
  into the second step, and treats downstream `isError` as fail-fast.
- An allowed skill stops on a later call-policy denial, returns the first step's
  partial result, and never invokes remaining steps.
- A running two-step skill completes from its cloned definition while hot reload
  removes that skill from the published catalog.

`crates/mcp-host-mcp/tests/http_e2e.rs` runs real local Streamable HTTP RMCP
servers. It verifies an environment-resolved secret header is delivered to the
local service but absent from inspection debug output. It also verifies a
paginated tool listing is collected completely and that a failed refresh leaves
the prior snapshot available with `stale: true`. A separate local probe verifies
that an `https://` manifest opens a TLS connection rather than rejecting the
scheme before network I/O.

`crates/mcp-host-mcp/tests/auth_e2e.rs` runs a local OAuth authorization server
and Bearer-protected Streamable HTTP MCP service. It verifies discovery,
registration selection, PKCE callback validation, token exchange and refresh,
authorized tool calls, credential-file lifecycle, and secret-free status.

`crates/mcp-host-cli/tests/daemon_e2e.rs` runs a real daemon, CLI subprocesses,
RMCP client -> stdio bridge -> daemon -> downstream fixture chain, one runtime
shared by the CLI and two MCP clients, rejection of a second daemon without
damaging the first, and shutdown while both upstream and downstream sessions are
active.
It also verifies root skill files are excluded from server discovery, valid
skills hot reload through CLI `skill list/run`, invalid skill changes preserve
the prior catalog, and `skill_run` policy reloads atomically with that catalog.

`crates/mcp-host-cli/tests/harness_cli.rs` launches fake OpenCode and Claude
executables. It verifies separate argv values, runtime paths containing spaces,
Claude scope and remove/add ordering, compact JSON output, and actionable errors
when a harness CLI is missing. The test does not modify real harness
configuration.

Service unit tests use a fake shell-free command runner to verify systemd and
launchd install, reinstall, `--no-start`, status, and uninstall ordering without
changing the host machine. They also verify foreign artifacts are rejected
before any manager call and that external service cancellation reaches orderly
daemon shutdown. Windows SCM code is covered by descriptor/argv unit tests plus
Windows GNU Clippy and release cross-builds; create/start/STOP/SHUTDOWN/delete
still requires a native elevated Windows test. launchd bootstrap/bootout likewise
requires native macOS validation.

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
source secrets, runtime errors omit tool arguments and OAuth callback values,
OAuth status omits access/refresh tokens, daemon metadata contains no
configuration secrets, and HTTP inspection output does not reveal the supplied
header secret. Streaming stderr tests additionally split a sentinel secret
across read chunks and verify that both complete and end-of-stream partial
values are redacted.
