#!/bin/sh
# Installs the dashr binary into ./bin for the Herdr plugin (DASHR-HERDR-006).
#
# Herdr runs this as the plugin's build step, from the plugin root. It
# downloads the release matching herdr-plugin.toml's version, verifies its
# SHA-256, and installs it. No Rust toolchain is needed. Only when no release
# exists for this version and platform does it fall back to building with
# cargo, and it says so.
set -eu

REPO="czinegeroland/herdr-dashr"
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' herdr-plugin.toml | head -n 1)"
[ -n "$VERSION" ] || { echo "install: no version in herdr-plugin.toml" >&2; exit 1; }

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64 | Linux-arm64) TARGET="aarch64-unknown-linux-gnu" ;;
  Darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
  Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
  *) TARGET="" ;;
esac

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

mkdir -p bin
ARCHIVE="dashr-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ARCHIVE}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ -n "$TARGET" ] && [ "${DASHR_INSTALL_FROM_SOURCE:-}" != "1" ] \
  && curl -fsSL --retry 3 -o "$TMP/$ARCHIVE" "$URL" 2>/dev/null \
  && curl -fsSL --retry 3 -o "$TMP/$ARCHIVE.sha256" "$URL.sha256" 2>/dev/null; then
  EXPECTED="$(awk '{print $1}' "$TMP/$ARCHIVE.sha256")"
  ACTUAL="$(sha256 "$TMP/$ARCHIVE")"
  if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "install: checksum mismatch for $ARCHIVE (expected $EXPECTED, got $ACTUAL)" >&2
    exit 1
  fi
  tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
  install -m 0755 "$TMP/dashr" bin/dashr
  echo "install: dashr $VERSION ($TARGET) installed, checksum verified"
elif command -v cargo >/dev/null 2>&1; then
  echo "install: no release for v$VERSION on ${TARGET:-this platform}; building with cargo"
  cargo build --release --locked --bin dashr
  install -m 0755 target/release/dashr bin/dashr
else
  echo "install: no release for v$VERSION on ${TARGET:-this platform}, and cargo is not installed" >&2
  exit 1
fi

bin/dashr --version
