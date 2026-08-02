# Manifest Registry

Milestone 2 separates filesystem loading from registry construction.

## Loading

```rust
use mcp_host_core::{ManifestLoader, ProcessEnvironment};
use std::path::Path;

let loader = ManifestLoader::new(ProcessEnvironment);
let manifests = loader.load_directory(Path::new("config/servers"))?;
# Ok::<(), mcp_host_core::ManifestLoadError>(())
```

The loader performs, in order:

1. non-recursive TOML discovery;
2. Serde parsing with unknown-field rejection;
3. semantic validation;
4. environment reference resolution;
5. construction of `LoadedManifest` values containing source, raw, and
   resolved configuration.

Startup configuration I/O is synchronous. It is bounded local filesystem work
performed before runtime operation, so making it async would add API and Tokio
coupling without enabling meaningful concurrency.

## Building A Snapshot

```rust
use mcp_host_core::RegistryBuilder;

# let manifests = Vec::new();
let registry = RegistryBuilder::build(manifests)?;

for server in registry.iter() {
    println!("{}: {}", server.id(), server.source_path().display());
}
# Ok::<(), mcp_host_core::RegistryBuildError>(())
```

`McpServerRegistry` stores entries in a `BTreeMap` keyed by normalized
`ServerId`. It exposes only immutable lookup and iteration APIs:

- `get`
- `contains`
- `iter`
- `len`
- `is_empty`

Iteration order is normalized server ID order and is independent of filesystem
enumeration order. Disabled servers remain present.

## Duplicate IDs

`RegistryBuilder` sorts loaded manifests by source filename before insertion.
If two files define the same normalized ID, construction fails at the first
duplicate with:

- the normalized ID;
- the first source path;
- the second source path.

This attribution remains deterministic even if callers provide loaded
manifests in a different vector order.

## Error Boundaries

`ManifestLoadError` distinguishes directory discovery, file access, format,
parse, validation, and environment resolution failures. Validation and
environment errors remain typed sources with field paths.

`RegistryBuildError` currently represents duplicate normalized IDs. Errors are
fail-fast; collecting all directory errors or all duplicates is not part of V1.

## Runtime Separation

The registry is an immutable configuration snapshot. It contains no lifecycle,
process handle, MCP peer, tool cache, retry state, or mutable connection state.
At daemon startup, `RuntimeManager` consumes each
`RegisteredServer::resolved_manifest()` and creates a separate per-server
runtime entry. All CLI and inbound MCP sessions share that manager. The daemon
watches the configuration directory and atomically reconciles added, changed,
and removed entries after a 500 ms debounce; invalid reloads preserve the prior
snapshot. Root-level `*.skill.toml` files are excluded from server discovery and
loaded into a separate immutable skill catalog. Registry, policy, and skill
catalog validation succeeds before any of the three is published.

Remote registries and implicit package installation remain absent. Policy,
explicit package installation, and OAuth configuration are loaded locally; none
is stored as mutable registry runtime state.
