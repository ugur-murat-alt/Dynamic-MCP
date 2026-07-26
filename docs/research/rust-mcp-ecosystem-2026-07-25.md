# Rust MCP Ecosystem Evaluation

Date: 2026-07-25

## Decision Summary

- Target the stable MCP specification revision `2025-11-25`.
- Use the official Rust SDK, `rmcp = 2.2.0`, with an exact version pin when it
  enters the workspace.
- Do not build on `rmcp 3.0.0-beta` or the draft `2026-07-28` protocol.
- Start downstream integration with stdio child processes. Keep Streamable HTTP
  behind the same service boundary and add it without a custom MCP protocol
  implementation.
- Use Tokio, clap, serde, serde_json, TOML, tracing, thiserror, anyhow, and
  directories where their owning milestones need them.
- Use `interprocess 2.4.2` for cross-platform local daemon IPC, with explicit
  length-delimited framing.

## MCP And RMCP

MCP `2025-11-25` defines initialization as `initialize`, the initialize result,
then `notifications/initialized`. Requests should not begin before negotiation
finishes. It defines stdio and Streamable HTTP transports and recommends a
timeout for every request followed by cancellation when a timeout expires.

Tool discovery uses paginated `tools/list`. A server advertising
`tools.listChanged` may send `notifications/tools/list_changed`; clients must
list tools again because the notification contains no delta. Tool invocation
uses `tools/call`, with protocol errors distinct from tool execution errors.

The official `rmcp 2.2.0` release was published on 2026-07-08 under Apache-2.0.
It supports client and server roles, a `TokioChildProcess` stdio client
transport, stdio server transport, and Streamable HTTP transports. Its 2.2.0
release fixed cancellation behavior and findings from the 2025-11-25
conformance audit. The relevant features can be enabled separately so the core
client and inbound server adapter need not compile every transport.

`rust-mcp-sdk` was considered as an alternative. It provides broad client and
server functionality, but its smaller ecosystem and explicit development-stage
warning make the official SDK the lower-risk V1 foundation.

## General Rust Stack

- Tokio is the required async runtime. Enable only the process, runtime, signal,
  synchronization, time, and I/O features used by each milestone.
- clap derive keeps the command model typed without a CLI framework layer.
- serde, serde_json, and TOML cover manifests, IPC DTOs, and CLI JSON output.
- tracing and tracing-subscriber provide structured diagnostics; secrets are
  excluded from fields.
- thiserror owns library/domain errors. anyhow is limited to binary
  orchestration boundaries where contextual reporting is more useful than a
  public error type.
- directories supplies per-user config, state, and runtime locations.

`tokio::process::Child` continues running when dropped unless configured
otherwise. `kill_on_drop` is only a safety net and does not guarantee timely
reaping. Normal shutdown must close MCP input, wait, escalate termination after
a timeout, and await process exit. Killing one PID does not guarantee descendant
cleanup on Unix or Windows; process groups and Windows Job Objects are separate
platform work and are not promised by V1.

## Local IPC

`interprocess 2.4.2` offers Tokio local sockets backed by Unix domain sockets on
Unix and named pipes on Windows. It does not add message framing, so the host
will use bounded length-delimited frames. Filesystem socket paths must live in a
private per-user runtime directory. Windows named-pipe access must reject remote
clients and receive platform-specific access-control tests before release.

The crate has a small maintainer surface compared with Tokio. Keeping the IPC
wire protocol independent from the crate and running Linux, macOS, and Windows
tests limits replacement cost.

## Sources

- [MCP 2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)
- [MCP 2025-11-25 tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)
- [MCP 2025-11-25 transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
- [Official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)
- [rmcp 2.2.0 crate metadata](https://crates.io/api/v1/crates/rmcp/2.2.0)
- [rmcp 2.2.0 API documentation](https://docs.rs/rmcp/2.2.0/rmcp/)
- [rmcp 2.2.0 release](https://github.com/modelcontextprotocol/rust-sdk/releases/tag/rmcp-v2.2.0)
- [Alternative rust-mcp-sdk](https://github.com/rust-mcp-stack/rust-mcp-sdk)
- [Tokio process documentation](https://docs.rs/tokio/latest/tokio/process/)
- [interprocess 2.4.2 crate metadata](https://crates.io/api/v1/crates/interprocess/2.4.2)
- [interprocess local sockets](https://docs.rs/interprocess/2.4.2/interprocess/local_socket/)
- [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)
