#!/bin/sh
# install.sh — one-line installer for agent-notify (Linux / WSL):
#   curl -fsSL https://raw.githubusercontent.com/jaltez/agent-notify/main/scripts/install.sh | sh
# Downloads the latest release from GitHub, verifies its checksum and
# installs the binary into ~/.local/bin.
set -eu

REPO="jaltez/agent-notify"
PREFIX="${PREFIX:-$HOME/.local/bin}"

fail() { echo "install: $*" >&2; exit 1; }

OS="$(uname -s)"
ARCH="$(uname -m)"
[ "$OS" = "Linux" ] || fail "this installer supports Linux/WSL; on Windows download the release zip from https://github.com/$REPO/releases"
[ "$ARCH" = "x86_64" ] || fail "no release assets for $ARCH; build from source: go install github.com/$REPO@latest"

command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 || fail "need curl or wget"
command -v tar >/dev/null 2>&1 || fail "need tar"
command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 || fail "need sha256sum"

fetch() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$1" -o "$2"
	else
		wget -qO "$2" "$1"
	fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "install: finding latest release"
fetch "https://api.github.com/repos/$REPO/releases/latest" "$TMP/rel.json"
VERSION="$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$TMP/rel.json" | head -1)"
[ -n "$VERSION" ] || fail "could not read latest version"

BASE="https://github.com/$REPO/releases/download/$VERSION"

# match the linux amd64 tarball whatever the naming convention
# (goreleaser uses underscores, early releases used dashes)
URLS="$(grep -o '"browser_download_url": *"[^"]*"' "$TMP/rel.json" | sed 's/.*"\([^"]*\)"$/\1/')"
ASSET_URL="$(echo "$URLS" | grep 'linux.*amd64.*tar.gz$' | head -1)"
[ -n "$ASSET_URL" ] || fail "no linux amd64 tarball in release $VERSION"
ASSET="$(basename "$ASSET_URL")"

SUM_URL="$(echo "$URLS" | grep -E '/(checksums.txt|SHA256SUMS)$' | head -1)"
[ -n "$SUM_URL" ] || fail "no checksum file in release $VERSION"
SUM_NAME="$(basename "$SUM_URL")"

echo "install: downloading $VERSION ($ASSET)"
fetch "$ASSET_URL" "$TMP/$ASSET"
fetch "$SUM_URL" "$TMP/$SUM_NAME"

EXPECTED="$(grep " $ASSET\$" "$TMP/$SUM_NAME" | awk '{print $1}')"
[ -n "$EXPECTED" ] || fail "no checksum entry for $ASSET"
if command -v sha256sum >/dev/null 2>&1; then
	ACTUAL="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
else
	ACTUAL="$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')"
fi
[ "$ACTUAL" = "$EXPECTED" ] || fail "checksum mismatch (expected $EXPECTED, got $ACTUAL)"

mkdir -p "$PREFIX"
tar -xzf "$TMP/$ASSET" -C "$TMP" agent-notify
mv "$TMP/agent-notify" "$PREFIX/agent-notify"
chmod +x "$PREFIX/agent-notify"

echo "install: installed $PREFIX/agent-notify ($VERSION)"
case ":$PATH:" in
	*":$PREFIX:"*) ;;
	*) echo "install: note: $PREFIX is not in your PATH" ;;
esac
echo "install: run 'agent-notify' (tray) or 'agent-notify probe' to start"
