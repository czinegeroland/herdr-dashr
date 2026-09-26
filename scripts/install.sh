#!/bin/sh
# Installs the dashr binary into ./bin for the Herdr plugin (DASHR-HERDR-006).
#
# Herdr runs this as the plugin's build step, from the plugin root. No Rust
# toolchain is needed. It installs the version herdr-plugin.toml declares,
# trying in order:
#
#   1. npm: `herdr-dashr@<version>` and its platform package, which npm
#      checks against its own integrity hash, and whose executable was
#      verified against the release's SHA-256 before it was packed
#      (scripts/build-npm-packages.mjs, DASHR-TECH-005);
#   2. the GitHub release archive, verified against its published SHA-256;
#   3. a cargo build, only when neither exists for this version and platform.
#
# DASHR_INSTALL_SOURCE=npm|github|source forces one of them.
set -eu

REPO="czinegeroland/herdr-dashr"
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' herdr-plugin.toml | head -n 1)"
[ -n "$VERSION" ] || { echo "install: no version in herdr-plugin.toml" >&2; exit 1; }

# Keep in step with npm/dashr/bin.js, scripts/build-npm-packages.mjs and the
# release matrix; crates/dashr-cli/tests/distribution.rs holds them together.
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) TARGET="x86_64-unknown-linux-gnu" SUFFIX="linux-x64" ;;
  Linux-aarch64 | Linux-arm64) TARGET="aarch64-unknown-linux-gnu" SUFFIX="linux-arm64" ;;
  Darwin-x86_64) TARGET="x86_64-apple-darwin" SUFFIX="darwin-x64" ;;
  Darwin-arm64) TARGET="aarch64-apple-darwin" SUFFIX="darwin-arm64" ;;
  *) TARGET="" SUFFIX="" ;;
esac

SOURCE="${DASHR_INSTALL_SOURCE:-}"
[ "${DASHR_INSTALL_FROM_SOURCE:-}" = "1" ] && SOURCE="source"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

mkdir -p bin
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Installs $1 as bin/dashr if it runs and reports the expected version.
take() {
  if [ -x "$1" ] && [ "$("$1" --version 2>/dev/null)" = "dashr $VERSION" ]; then
    install -m 0755 "$1" bin/dashr
    return 0
  fi
  return 1
}

from_npm() {
  [ -n "$SUFFIX" ] && command -v npm >/dev/null 2>&1 || return 1
  # Into a scratch prefix, so the plugin directory gets no node_modules.
  npm install --prefix "$TMP/npm" --no-save --no-package-lock --no-audit --no-fund \
    --loglevel=error "herdr-dashr@$VERSION" >/dev/null 2>&1 || return 1
  take "$TMP/npm/node_modules/herdr-dashr-$SUFFIX/dashr" || return 1
  echo "install: dashr $VERSION ($SUFFIX) installed from npm"
}

from_github() {
  [ -n "$TARGET" ] || return 1
  archive="dashr-${VERSION}-${TARGET}.tar.gz"
  url="https://github.com/${REPO}/releases/download/v${VERSION}/${archive}"
  curl -fsSL --retry 3 -o "$TMP/$archive" "$url" 2>/dev/null || return 1
  curl -fsSL --retry 3 -o "$TMP/$archive.sha256" "$url.sha256" 2>/dev/null || return 1
  expected="$(awk '{print $1}' "$TMP/$archive.sha256")"
  actual="$(sha256 "$TMP/$archive")"
  if [ "$expected" != "$actual" ]; then
    echo "install: checksum mismatch for $archive (expected $expected, got $actual)" >&2
    exit 1
  fi
  mkdir -p "$TMP/unpack"
  tar -xzf "$TMP/$archive" -C "$TMP/unpack"
  take "$TMP/unpack/dashr" || return 1
  echo "install: dashr $VERSION ($TARGET) installed from the GitHub release, checksum verified"
}

from_source() {
  command -v cargo >/dev/null 2>&1 || return 1
  echo "install: building dashr $VERSION with cargo"
  cargo build --release --locked --bin dashr
  install -m 0755 target/release/dashr bin/dashr
}

case "$SOURCE" in
  npm) from_npm || { echo "install: dashr $VERSION is not installable from npm here" >&2; exit 1; } ;;
  github) from_github || { echo "install: no GitHub release of dashr $VERSION for ${TARGET:-this platform}" >&2; exit 1; } ;;
  source) from_source || { echo "install: cargo is not installed" >&2; exit 1; } ;;
  "") from_npm || from_github || from_source || {
    echo "install: dashr $VERSION is not on npm or GitHub for ${TARGET:-this platform}, and cargo is not installed" >&2
    exit 1
  } ;;
  *) echo "install: DASHR_INSTALL_SOURCE must be npm, github or source" >&2; exit 1 ;;
esac

bin/dashr --version
