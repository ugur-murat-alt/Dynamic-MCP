# Installation

## Quick Install

The POSIX installer supports Linux x86-64, macOS x86-64 and Apple Silicon, and
Windows x86-64 from Git Bash, MSYS2, or Cygwin:

```bash
curl -fsSL https://raw.githubusercontent.com/ugur-murat-alt/Dynamic-MCP/main/install.sh | sh
```

It resolves the latest GitHub Release, downloads the matching `.tar.gz` archive
and `.sha256` file, verifies the checksum, and atomically installs `mcp-host` to
`~/.local/bin`. The destination is never replaced when download, checksum, or
archive validation fails.

Add `~/.local/bin` to `PATH` if the installer reports that it is missing.

## Installer Options

Install a pinned release:

```bash
curl -fsSL https://raw.githubusercontent.com/ugur-murat-alt/Dynamic-MCP/main/install.sh \
  | sh -s -- --version v0.2.0
```

Pin `v0.2.0` for the RMCP 3 beta SDK migration, stable Host result envelope,
and MCP-first harness skill. Earlier v0.1.x releases retain their documented
behavior. Upgrade the CLI and running daemon together; control protocol v1 is
unchanged, but the v0.2.0 Host MCP result contract is versioned separately.

Choose another destination:

```bash
./install.sh --install-dir "$HOME/bin"
```

Use another GitHub fork:

```bash
./install.sh --repo OWNER/REPOSITORY
```

Environment equivalents are `MCP_HOST_REPO` and `MCP_HOST_INSTALL_DIR`.
`./install.sh --help` prints the complete interface.

## Harness Setup

After installation, register the short-lived stdio bridge:

```bash
mcp-host harness install opencode
mcp-host harness install claude-code --scope user
# Or both:
mcp-host harness install all
```

The command stores the canonical absolute binary path so the harness does not
depend on its process `PATH`. A non-default daemon runtime directory must be
registered explicitly:

```bash
mcp-host --runtime-dir /absolute/runtime/path harness install all
```

OpenCode configuration is global. Claude Code's default in Dynamic MCP Host is
`user`, unlike Claude's own `local` default. Select `--scope local` for a private
current-project entry or `--scope project` for a shareable `.mcp.json` entry.

Registration is verified and repeatable. An exact semantic match is left
unchanged. A missing or mismatched entry is repaired through the official
harness CLI and read back from OpenCode's merged `config.json`, `opencode.json`,
and `opencode.jsonc` view or the selected Claude scope; success is returned only
when name, transport, command, and argv match.

Harness setup also installs the embedded `dynamic-mcp` skill and manages one
marked instruction block without replacing user-authored content:

| Harness | Skill | Managed instructions |
| --- | --- | --- |
| OpenCode | `$XDG_CONFIG_HOME/opencode/skills/dynamic-mcp/SKILL.md`, falling back to `~/.config/opencode/...` | `$XDG_CONFIG_HOME/opencode/AGENTS.md`, falling back to `~/.config/opencode/AGENTS.md` |
| Claude Code | `~/.claude/skills/dynamic-mcp/SKILL.md` | `~/.claude/CLAUDE.md` |

Files are written only when content changes and are atomically replaced. A
duplicate or unbalanced managed marker is rejected instead of guessing which
section to overwrite. Use `--name` when an existing registration must remain
untouched.

If a harness must launch a daemon-bootstrap or supervisor wrapper, register its
complete argv explicitly. No implicit `mcp` argument is appended in this mode:

```bash
mcp-host harness install opencode \
  --bridge-command ~/.local/libexec/mcp-host/serve-bridge
# Repeat --bridge-arg VALUE for wrapper arguments when needed.
```

`--bridge-command` cannot be combined with `--runtime-dir`; pass wrapper-specific
options with repeated `--bridge-arg` values.

The harness CLI (`opencode` or `claude`) must be installed and on `PATH` when a
registration needs repair. This step does not install either harness and does
not start Dynamic MCP Host's daemon.

## Start The Host

Create one or more manifests, then either run the daemon in the foreground:

```bash
mcp-host daemon run --config-dir /absolute/path/to/manifests
```

The registered harness starts `mcp-host mcp`, which connects to that daemon.
See [Manifest format](manifest-format.md) and [Daemon and IPC](daemon-and-ipc.md).

For a managed background service, install the native service instead:

```bash
# systemd user service on Linux, launchd user agent on macOS
mcp-host daemon service install --config-dir /absolute/path/to/manifests

# Write and enable without starting immediately
mcp-host daemon service install --config-dir /absolute/path/to/manifests --no-start

mcp-host daemon service status --config-dir /absolute/path/to/manifests
mcp-host daemon service uninstall --config-dir /absolute/path/to/manifests
```

`install` writes or repairs only Dynamic MCP-managed artifacts, enables/loads the
service, and starts it unless `--no-start` is present. Reinstalling a running
service restarts it so updated executable/config/runtime argv takes effect.
`uninstall` stops and disables/unloads before removing the managed artifact.
Foreign files or SCM records are never overwritten, stopped, or removed.

Linux/macOS default to user scope; `--scope system` requires platform
administrator privileges. Windows uses a native SCM LocalSystem service,
defaults to system scope, requires an elevated terminal, and stores the default
runtime below `%ProgramData%\Dynamic MCP\runtime`. Scheduled Tasks are not used.
For systemd user services, Dynamic MCP does not enable login lingering; use the
operating system's normal `loginctl enable-linger` administration if the service
must survive logout.

## Upgrade

Run the installer again. It verifies and atomically replaces the executable at
the same path, so existing harness registrations continue to work:

```bash
curl -fsSL https://raw.githubusercontent.com/ugur-murat-alt/Dynamic-MCP/main/install.sh | sh
```

## Release Assets

Each `v*` tag runs the release workflow and publishes one archive plus one
checksum file for each supported target:

```text
mcp-host-<TAG>-<TARGET>.tar.gz
mcp-host-<TAG>-<TARGET>.tar.gz.sha256
```

The release job runs formatting, Clippy, and the full workspace test suite
before building or publishing assets. The tag must equal `v` plus the Cargo
package version.
