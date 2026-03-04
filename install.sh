#!/bin/sh
# archive-assistant installer
# Installs archive-repack and archive-assistant binaries.
#
# Usage: curl -fsSL https://raw.githubusercontent.com/jamietre/archive-assistant/master/install.sh | sh

set -e

REPO="jamietre/archive-assistant"

# ── Detect platform ────────────────────────────────────────────────────────────

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux) OS_NAME="linux" ;;
  *)
    echo "Unsupported OS: $OS (only Linux is supported at this time)" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64) ARCH_NAME="x86_64" ;;
  *)
    echo "Unsupported architecture: $ARCH (only x86_64 is supported at this time)" >&2
    exit 1
    ;;
esac

PLATFORM="${ARCH_NAME}-${OS_NAME}"

# ── Resolve version ────────────────────────────────────────────────────────────

LATEST_VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
  | grep '"tag_name"' \
  | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"

if [ -z "$LATEST_VERSION" ] || [ "$LATEST_VERSION" = "null" ]; then
  echo "Could not determine latest version. Set VERSION explicitly and retry." >&2
  exit 1
fi

if [ -n "$VERSION" ]; then
  echo "Latest version: ${LATEST_VERSION}"
  echo "Using VERSION override: ${VERSION}"
else
  VERSION="$LATEST_VERSION"
  echo "Latest version: ${VERSION}"
fi

INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# ── Download and extract ───────────────────────────────────────────────────────

TARBALL="archive-assistant-${VERSION#v}-${PLATFORM}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading ${URL}..."
curl -fsSL "$URL" -o "${TMPDIR}/${TARBALL}"

echo "Extracting..."
tar -xzf "${TMPDIR}/${TARBALL}" -C "${TMPDIR}"

# ── Install binaries ───────────────────────────────────────────────────────────

mkdir -p "$INSTALL_DIR"

for bin in archive-repack archive-assistant; do
  if [ -f "${TMPDIR}/${bin}" ]; then
    install -m 755 "${TMPDIR}/${bin}" "${INSTALL_DIR}/${bin}"
  else
    echo "WARNING: ${bin} not found in archive" >&2
  fi
done

echo ""
echo "Installed to ${INSTALL_DIR}:"
echo "  archive-repack      — repack any archive format to ZIP"
echo "  archive-assistant   — walk a directory tree and preprocess archives"
echo ""

# ── PATH check ────────────────────────────────────────────────────────────────

case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo "NOTE: ${INSTALL_DIR} is not in your PATH."
    echo "Add this to your shell profile:"
    echo ""
    echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
    echo ""
    ;;
esac
