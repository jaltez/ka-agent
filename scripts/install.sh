#!/bin/sh
# Ka Agent installer: downloads the release binary for this machine,
# verifies its checksum, installs it, and starts `ka-agent setup`.
#
#   curl -fsSL https://raw.githubusercontent.com/jaltez/ka-agent/main/scripts/install.sh | sh
#
# Environment:
#   KA_AGENT_INSTALL_DIR   where the binary goes (default: ~/.local/bin)
#   KA_AGENT_NO_SETUP=1    install only; do not start the setup wizard
#   KA_AGENT_INSTALL_FROM  install from this local tarball (no download)
set -eu

REPO="jaltez/ka-agent"
DIR="${KA_AGENT_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'ka-agent install: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *) os="" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) arch="" ;;
esac
if [ -z "$os" ] || [ -z "$arch" ]; then
  die "unsupported platform $(uname -s)/$(uname -m); build from source: cargo install --git https://github.com/$REPO ka-agent"
fi
T="$arch-$os"

fetch() {
  # fetch URL OUT
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    die "need curl or wget to download"
  fi
}

verify() {
  # verify TARBALL SUMFILE (the sum file names the tarball's basename)
  ( cd "$(dirname "$1")" &&
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum -c "$(basename "$2")" >/dev/null
    elif command -v shasum >/dev/null 2>&1; then
      shasum -a 256 -c "$(basename "$2")" >/dev/null
    else
      die "need sha256sum or shasum to verify the download"
    fi ) || die "checksum mismatch for $(basename "$1")"
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

if [ -n "${KA_AGENT_INSTALL_FROM:-}" ]; then
  [ -f "$KA_AGENT_INSTALL_FROM" ] || die "no such file: $KA_AGENT_INSTALL_FROM"
  cp "$KA_AGENT_INSTALL_FROM" "$TMP/ka-agent.tar.gz"
else
  NAME="ka-agent-$T.tar.gz"
  URL="https://github.com/$REPO/releases/latest/download/$NAME"
  say "downloading $NAME…"
  fetch "$URL" "$TMP/$NAME" || die "download failed: $URL"
  fetch "$URL.sha256" "$TMP/$NAME.sha256" || die "download failed: $URL.sha256"
  verify "$TMP/$NAME" "$TMP/$NAME.sha256"
  mv "$TMP/$NAME" "$TMP/ka-agent.tar.gz"
fi

tar xzf "$TMP/ka-agent.tar.gz" -C "$TMP" ka-agent || die "the archive has no ka-agent binary"
mkdir -p "$DIR"
# Replace atomically: a running daemon keeps its old inode.
cp "$TMP/ka-agent" "$DIR/.ka-agent.new"
chmod 755 "$DIR/.ka-agent.new"
mv "$DIR/.ka-agent.new" "$DIR/ka-agent"
say "installed $("$DIR/ka-agent" --version) to $DIR/ka-agent"

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say "add $DIR to your PATH (e.g. in ~/.profile: export PATH=\"$DIR:\$PATH\")" ;;
esac

if [ "${KA_AGENT_NO_SETUP:-0}" != "1" ]; then
  if [ -r /dev/tty ] && (exec </dev/tty) 2>/dev/null; then
    "$DIR/ka-agent" setup </dev/tty
  else
    say "next: run \`ka-agent setup\`"
  fi
fi
