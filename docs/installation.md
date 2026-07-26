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
  | sh -s -- --version v0.1.0
```

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

Registration is repeatable. OpenCode updates the same named global entry.
Claude Code removes and recreates the same name only in the selected scope.
Use `--name` when an existing registration must remain untouched.

The harness CLI (`opencode` or `claude`) must be installed and on `PATH`. This
step does not install either harness and does not start Dynamic MCP Host's
daemon.

## Start The Host

Create one or more manifests, then run the daemon in the foreground:

```bash
mcp-host daemon run --config-dir /absolute/path/to/manifests
```

The registered harness starts `mcp-host mcp`, which connects to that daemon.
See [Manifest format](manifest-format.md) and [Daemon and IPC](daemon-and-ipc.md).

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
