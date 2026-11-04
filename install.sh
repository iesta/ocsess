#!/bin/sh
# Install ocsess from GitHub releases.
#
#   curl -sSfL https://raw.githubusercontent.com/iesta/ocsess/main/install.sh | sh
#
# Environment:
#   OCSESS_VERSION      release tag to install (default: latest)
#   OCSESS_INSTALL_DIR  install directory (default: $HOME/.local/bin)
set -eu

REPO="iesta/ocsess"
BIN="ocsess"
INSTALL_DIR="${OCSESS_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*" >&2; }
fail() { say "error: $*"; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

need uname
need tar
need mkdir
if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    fail "missing required command: curl or wget"
fi

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    else
        wget -qO "$2" "$1"
    fi
}

case "$(uname -s)" in
    Linux) os_part="unknown-linux-musl" ;;
    Darwin) os_part="apple-darwin" ;;
    *) fail "unsupported operating system: $(uname -s) · Linux and macOS only" ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) arch_part="x86_64" ;;
    arm64 | aarch64) arch_part="aarch64" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
esac

target="$arch_part-$os_part"

version="${OCSESS_VERSION:-}"
if [ -z "$version" ]; then
    # Follow the /releases/latest redirect instead of the API, which is rate limited.
    resolved=""
    if command -v curl >/dev/null 2>&1; then
        resolved=$(curl -fsSIL -o /dev/null -w '%{url_effective}' \
            "https://github.com/$REPO/releases/latest" 2>/dev/null || true)
    fi
    case "$resolved" in
        */releases/tag/*) version="${resolved##*/}" ;;
    esac
    [ -n "$version" ] || fail "could not detect the latest release · set OCSESS_VERSION=vX.Y"
fi

case "$version" in
    v*) ;;
    *) fail "unexpected release tag: $version" ;;
esac

asset="$BIN-$version-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$version"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "downloading $asset"
fetch "$base/$asset" "$tmp/$asset" || fail "download failed: $base/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "download failed: $base/SHA256SUMS"

expected=$(awk -v file="$asset" '$2 == file || $2 == "*" file { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "no checksum listed for $asset"

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$asset" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')
else
    fail "missing required command: sha256sum or shasum"
fi

[ "$expected" = "$actual" ] || fail "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
cp "$tmp/${asset%.tar.gz}/$BIN" "$INSTALL_DIR/$BIN"
chmod 755 "$INSTALL_DIR/$BIN"

say "installed $BIN $version to $INSTALL_DIR/$BIN"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) say "add it to your PATH: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

if ! command -v opencode >/dev/null 2>&1; then
    say "note: $BIN reads OpenCode data · install the opencode CLI to use it"
fi
