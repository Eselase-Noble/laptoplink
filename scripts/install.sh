#!/usr/bin/env bash
# LaptopLink installer.
#
#   curl -fsSL https://raw.githubusercontent.com/Eselase-Noble/laptoplink/production/scripts/install.sh | bash
#
# Downloads the latest prebuilt release for your OS/arch and installs the
# `laplink` (and `laptoplink` alias) binaries. No Rust toolchain required.
set -euo pipefail

REPO="Eselase-Noble/laptoplink"
# Install into the first of these that we can write to.
BIN_DIR="${LAPLINK_BIN_DIR:-}"

say() { printf '>> %s\n' "$*"; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- detect platform ------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  case "$arch" in
            x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
            *) err "unsupported Linux arch '$arch'. Build from source: https://github.com/$REPO" ;;
          esac ;;
  Darwin) case "$arch" in
            arm64|aarch64) target="aarch64-apple-darwin" ;;
            x86_64)        target="x86_64-apple-darwin" ;;
            *) err "unsupported macOS arch '$arch'" ;;
          esac ;;
  *) err "unsupported OS '$os'. On Windows, download the .zip from the Releases page." ;;
esac

# --- resolve the latest release tag --------------------------------------
say "finding the latest release"
tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
[ -n "${tag:-}" ] || err "no published release found yet. Build from source: https://github.com/$REPO"

asset="laplink-${tag}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/${tag}/${asset}"

# --- download + extract ---------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
say "downloading $asset ($tag)"
curl -fSL "$url" -o "$tmp/pkg.tar.gz" || err "download failed: $url"
tar -xzf "$tmp/pkg.tar.gz" -C "$tmp"
src="$tmp/laplink-${tag}-${target}"

# --- choose an install directory -----------------------------------------
if [ -z "$BIN_DIR" ]; then
  if [ -w "/usr/local/bin" ]; then BIN_DIR="/usr/local/bin"
  else BIN_DIR="$HOME/.local/bin"; fi
fi
mkdir -p "$BIN_DIR"

install -m 0755 "$src/laplink" "$BIN_DIR/laplink"
install -m 0755 "$src/laptoplink" "$BIN_DIR/laptoplink"

say "installed 'laplink' and 'laptoplink' to $BIN_DIR"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "note: $BIN_DIR is not on your PATH. Add it, e.g.:"
     say "      echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.profile" ;;
esac
say "try it:  laplink doctor"
