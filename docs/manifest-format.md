# Manifest Format

Dynamic MCP Host loads downstream server registrations from TOML files. A
manifest describes configuration only; loading it never starts a process or
opens an MCP connection.

## Discovery

`ManifestLoader::load_directory` scans one user-provided directory.

- Only direct, regular files with the lowercase `.toml` extension are loaded.
- Subdirectories are not traversed.
- Hidden files, symlinks, unsupported extensions, and editor files beginning
  with `.`, `~`, or `#` are ignored.
- `policy.toml` and `*.skill.toml` belong to separate loaders and are not parsed
  as server manifests.
- Files are loaded in a deterministic filename order.
- An empty directory produces an empty manifest list.
- A missing or unreadable directory returns a typed error.

JSON is not supported because the existing domain model did not previously
provide a JSON manifest contract.

## Common Fields

```toml
id = "filesystem"
name = "Filesystem"
description = "Local filesystem tools"
enabled = true
```

`enabled` defaults to `true`. Disabled manifests remain in the registry and go
through the same validation and environment resolution as enabled manifests.

IDs are trimmed, converted to lowercase ASCII, and then validated against:

```text
^[a-z][a-z0-9._-]*$
```

For example, `" GitHub.Tools "` normalizes to `github.tools`. Duplicate checks
use the normalized ID.

Unknown fields are rejected at both server and transport levels.

## Stdio Transport

```toml
id = "github"
name = "GitHub"
description = "GitHub MCP server"

[transport]
type = "stdio"
command = "github-mcp-server"
args = ["stdio", "--verbose"]
working_directory = "./workspace"

[transport.environment]
GITHUB_TOKEN = "${GITHUB_TOKEN}"
LOG_LEVEL = "info"
```

- `command` must not be empty.
- Argument order is preserved exactly.
- `working_directory` is optional and must not be empty. A relative value is
  resolved lexically against the manifest file's parent directory. The loader
  does not require the directory to exist.
- Environment keys must not be empty.

## HTTP Transport

```toml
id = "remote"
name = "Remote"
description = "Remote Streamable HTTP MCP server"

[transport]
type = "http"
url = "https://example.com/mcp"

[transport.headers]
Authorization = "${REMOTE_AUTHORIZATION}"
```

- Only `http` and `https` URLs with a host are accepted.
- Usernames and passwords embedded in URLs are rejected.
- Header names must be non-empty valid HTTP token names.
- Header values use the same secret resolution rules as stdio environment
  values.

V1 passes these resolved static headers to RMCP's Streamable HTTP client on
every request.

### OAuth Authorization

An HTTP server can instead use authorization-code PKCE:

```toml
id = "remote"
name = "Remote"
description = "OAuth-protected MCP server"

[auth]
client_id = "pre-registered-public-client"
scopes = ["read", "write"]

[transport]
type = "http"
url = "https://example.com/mcp"
```

`client_id` is optional. When omitted, RMCP uses Dynamic Client Registration if
the authorization server advertises a registration endpoint. `scopes` is also
optional; an empty list lets RMCP select scopes from protected-resource and
authorization-server metadata. Empty client IDs, empty or duplicate scopes,
OAuth on a stdio transport, and combining `[auth]` with an `Authorization`
static header are rejected.

Pre-registered clients must allow an RFC 8252 loopback redirect with a dynamic
port. Dynamic registration receives the exact redirect URI selected by the CLI.

Run `mcp-host auth login <SERVER_ID>` before connecting. The CLI binds an
ephemeral `127.0.0.1` callback and uses PKCE S256. Tokens are stored below the
platform's persistent local-data directory in an `auth/` subtree; on Unix the
directory is mode `0700`, files are mode `0600`, and updates use an atomic
same-directory replace. The file name includes a fingerprint of server ID,
resource URL, client ID, and scopes so independent registrations cannot collide.
PKCE verifier and callback state remain memory-only. RMCP refreshes an expiring
access token when a refresh token is available. Credential files are bound to
the exact resource URL and are ignored after that URL changes.
If the platform cannot resolve a persistent local-data directory, non-OAuth
servers remain available but OAuth login/connect fail with `AUTH_FAILED`.

## Environment References

A value is resolved only when the complete string has this form:

```text
${VARIABLE_NAME}
```

Variable names use `[A-Za-z_][A-Za-z0-9_]*`. A missing variable produces a
typed error containing the source path, field path, and variable name. The
value is never included.

An environment variable that exists with an empty value is valid. Embedded
expressions such as `prefix-${TOKEN}` remain literal. A complete but malformed
reference such as `${BAD-NAME}` is rejected.

The loader does not support defaults, required-value operators, shell
expansion, command substitution, multiple interpolations, or automatic `.env`
loading.

## Secret Handling

Raw and resolved manifests are separate types. Resolved environment and header
values use a `SecretValue` backed by `secrecy::SecretString` without Serde
support.

- `Debug` always renders resolved values as `<redacted>`.
- Resolved secrets do not implement `Display` or `Serialize`.
- Secret access requires an explicit `expose_secret()` call.
- Raw manifest debug output lists environment/header keys but not values.
- Public TOML parse errors retain only the source path and byte span, not parser
  text or source snippets that could contain a value.

Raw manifest values remain explicitly accessible to library callers. Raw
credentials should therefore not be stored in manifests; use environment
references instead.
