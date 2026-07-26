#!/bin/sh
set -eu

REPO="${MCP_HOST_REPO:-ugur-murat-alt/Dynamic-MCP}"
RELEASES_URL="${MCP_HOST_RELEASES_URL:-https://github.com/$REPO/releases}"
VERSION="latest"
INSTALL_DIR="${MCP_HOST_INSTALL_DIR:-$HOME/.local/bin}"

usage() {
    cat <<'EOF'
Install Dynamic MCP Host from a GitHub Release.

Usage:
  install.sh [--version <TAG>] [--install-dir <DIR>] [--repo <OWNER/REPO>]

Options:
  --version <TAG>      Release tag to install, for example v0.1.0. Default: latest.
  --install-dir <DIR>  Destination directory. Default: $HOME/.local/bin.
  --repo <OWNER/REPO>  GitHub repository. Default: ugur-murat-alt/Dynamic-MCP.
  -h, --help           Show this help.

Environment:
  MCP_HOST_REPO          Override the GitHub owner/repository.
  MCP_HOST_INSTALL_DIR   Override the destination directory.

Examples:
  curl -fsSL https://raw.githubusercontent.com/ugur-murat-alt/Dynamic-MCP/main/install.sh | sh
  curl -fsSL https://raw.githubusercontent.com/ugur-murat-alt/Dynamic-MCP/main/install.sh | sh -s -- --version v0.1.0
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || { echo "error: --version requires a value" >&2; exit 2; }
            VERSION="$2"
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || { echo "error: --install-dir requires a value" >&2; exit 2; }
            INSTALL_DIR="$2"
            shift 2
            ;;
        --repo)
            [ "$#" -ge 2 ] || { echo "error: --repo requires a value" >&2; exit 2; }
            REPO="$2"
            RELEASES_URL="https://github.com/$REPO/releases"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

command -v curl >/dev/null 2>&1 || { echo "error: curl is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "error: tar is required" >&2; exit 1; }

if [ "$VERSION" = "latest" ]; then
    LATEST_URL=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$RELEASES_URL/latest")
    VERSION=${LATEST_URL##*/}
    if [ -z "$VERSION" ] || [ "$VERSION" = "latest" ]; then
        echo "error: could not resolve the latest release tag" >&2
        exit 1
    fi
else
    case "$VERSION" in
        v*) ;;
        *) VERSION="v$VERSION" ;;
    esac
fi

OS=$(uname -s)
ARCH=$(uname -m)
EXE="mcp-host"

case "$OS:$ARCH" in
    Linux:x86_64|Linux:amd64) TARGET="x86_64-unknown-linux-gnu" ;;
    Darwin:x86_64|Darwin:amd64) TARGET="x86_64-apple-darwin" ;;
    Darwin:arm64|Darwin:aarch64) TARGET="aarch64-apple-darwin" ;;
    MINGW*:x86_64|MSYS*:x86_64|CYGWIN*:x86_64)
        TARGET="x86_64-pc-windows-msvc"
        EXE="mcp-host.exe"
        ;;
    *)
        echo "error: unsupported platform: $OS $ARCH" >&2
        exit 1
        ;;
esac

ASSET="mcp-host-$VERSION-$TARGET.tar.gz"
BASE_URL="$RELEASES_URL/download/$VERSION"
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mcp-host-install.XXXXXX")
trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

echo "Downloading $ASSET"
curl -fsSL --retry 3 "$BASE_URL/$ASSET" -o "$TMP_DIR/$ASSET"
curl -fsSL --retry 3 "$BASE_URL/$ASSET.sha256" -o "$TMP_DIR/$ASSET.sha256"

(
    cd "$TMP_DIR"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$ASSET.sha256"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "$ASSET.sha256"
    elif command -v openssl >/dev/null 2>&1; then
        EXPECTED=$(cut -d ' ' -f 1 "$ASSET.sha256")
        ACTUAL=$(openssl dgst -sha256 "$ASSET" | sed 's/^.*= //')
        [ "$EXPECTED" = "$ACTUAL" ] || { echo "error: checksum mismatch" >&2; exit 1; }
    else
        echo "error: sha256sum, shasum, or openssl is required" >&2
        exit 1
    fi
)

tar -xzf "$TMP_DIR/$ASSET" -C "$TMP_DIR"
[ -f "$TMP_DIR/$EXE" ] || { echo "error: release archive does not contain $EXE" >&2; exit 1; }

mkdir -p "$INSTALL_DIR"
STAGED="$INSTALL_DIR/.mcp-host.install.$$"
cp "$TMP_DIR/$EXE" "$STAGED"
chmod 755 "$STAGED"
mv -f "$STAGED" "$INSTALL_DIR/$EXE"

echo "Installed $INSTALL_DIR/$EXE"
"$INSTALL_DIR/$EXE" --version

case ":$PATH:" in
    *:"$INSTALL_DIR":*) ;;
    *) echo "Add $INSTALL_DIR to PATH to run mcp-host from any shell." ;;
esac

echo "Next: mcp-host harness install opencode"
echo "  or: mcp-host harness install claude-code --scope user"
