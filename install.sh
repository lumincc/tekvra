#!/usr/bin/env bash
# Tekvra installer (Linux/macOS): downloads the release archive for the host
# triple, verifies its SHA-256 digest against SHA256SUMS.txt, and installs the
# `tekvra` binary into $TEKVRA_INSTALL_PREFIX/bin.
#
# Configuration (environment variables):
#   TEKVRA_VERSION         release tag to install (default: v0.1.0)
#   TEKVRA_INSTALL_PREFIX  install prefix (default: ~/.local; binary lands in PREFIX/bin)
#   TEKVRA_DOWNLOAD_PREFIX base URL for release assets; supports file:// for local
#                          smoke tests (default: the GitHub release download URL)
#
# The installer aborts with a non-zero exit status BEFORE placing any binary in
# the target directory when the computed digest does not match SHA256SUMS.txt.
set -euo pipefail

TEKVRA_VERSION="${TEKVRA_VERSION:-v0.1.0}"
TEKVRA_INSTALL_PREFIX="${TEKVRA_INSTALL_PREFIX:-$HOME/.local}"
TEKVRA_DOWNLOAD_PREFIX="${TEKVRA_DOWNLOAD_PREFIX:-https://github.com/lumincc/tekvra/releases/download}"

case "$(uname -s)" in
  Linux) os="linux" ;;
  Darwin) os="darwin" ;;
  *)
    echo "install.sh: unsupported OS '$(uname -s)'" >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *)
    echo "install.sh: unsupported architecture '$(uname -m)'" >&2
    exit 1
    ;;
esac

case "$os" in
  linux) target="${arch}-unknown-linux-gnu" ;;
  darwin) target="${arch}-apple-darwin" ;;
esac

archive="tekvra-${TEKVRA_VERSION}-${target}.tar.gz"
base_url="${TEKVRA_DOWNLOAD_PREFIX}/${TEKVRA_VERSION}"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

fetch() {
  # $1 = URL, $2 = destination file
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$1" -O "$2"
  else
    echo "install.sh: need curl or wget to download release assets" >&2
    exit 1
  fi
}

fetch "${base_url}/${archive}" "${tmp_dir}/${archive}"
fetch "${base_url}/SHA256SUMS.txt" "${tmp_dir}/SHA256SUMS.txt"

digest_cmd="sha256sum"
command -v sha256sum >/dev/null 2>&1 || digest_cmd="shasum -a 256"

expected="$(grep -E "[[:space:]]${archive}\$" "${tmp_dir}/SHA256SUMS.txt" | awk '{print $1}')"
if [ -z "$expected" ]; then
  echo "install.sh: no checksum entry for ${archive} in SHA256SUMS.txt" >&2
  exit 1
fi
actual="$($digest_cmd "${tmp_dir}/${archive}" | awk '{print $1}')"
if [ "$actual" != "$expected" ]; then
  echo "install.sh: checksum mismatch for ${archive}" >&2
  echo "  expected: $expected" >&2
  echo "  actual:   $actual" >&2
  exit 1
fi

tar -xzf "${tmp_dir}/${archive}" -C "$tmp_dir"
# Release archives produced by `scripts/ci_verify_packaging.py --package-release`
# nest the payload under tekvra-<version>-<target>/ alongside LICENSE, NOTICE.md,
# THIRD_PARTY_LICENSES/, and docs/. Fall back to a flat archive root for
# hand-built fixtures that predate the nested layout.
bundle_dir="${tmp_dir}/tekvra-${TEKVRA_VERSION}-${target}"
if [ ! -f "${bundle_dir}/tekvra" ]; then
  bundle_dir="${tmp_dir}"
fi
mkdir -p "${TEKVRA_INSTALL_PREFIX}/bin"
install -m 0755 "${bundle_dir}/tekvra" "${TEKVRA_INSTALL_PREFIX}/bin/tekvra"
echo "installed tekvra ${TEKVRA_VERSION} to ${TEKVRA_INSTALL_PREFIX}/bin/tekvra"
