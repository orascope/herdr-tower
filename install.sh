#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)
# Installs herdr-tower — fetches the latest release tar.gz (or HERDR_TOWER_VERSION)
# and puts the binary in ~/.local/bin/herdr-tower.
# The default path on Linux and WSL2, and it works on a Mac without brew too.
#   curl -fsSL https://raw.githubusercontent.com/orascope/herdr-tower/main/install.sh | bash
set -euo pipefail
REPO="orascope/herdr-tower"
BIN_DIR="${HERDR_TOWER_BIN_DIR:-$HOME/.local/bin}"

os=$(uname -s | tr '[:upper:]' '[:lower:]')   # darwin | linux
arch=$(uname -m)
case "$arch" in
  x86_64|amd64) arch=amd64 ;;
  arm64|aarch64) arch=arm64 ;;
  *) echo "unsupported architecture: $arch" >&2; exit 1 ;;
esac
case "$os" in darwin|linux) ;; *) echo "unsupported OS: $os (on Windows, use install.ps1 in PowerShell)" >&2; exit 1 ;; esac
[ "$os/$arch" != darwin/amd64 ] || { echo "unsupported: macOS on Intel (releases are macOS on Apple silicon and Linux)" >&2; exit 1; }

ver="${HERDR_TOWER_VERSION:-}"
if [ -z "$ver" ]; then
  # The tag comes from the releases/latest redirect rather than the API, which
  # rate-limits unauthenticated callers.
  ver=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed -E 's#.*/tag/##')
fi
ver="${ver#v}"
base="https://github.com/$REPO/releases/download/v$ver"
archive="herdr-tower_${ver}_${os}_${arch}.tar.gz"
url="$base/$archive"
echo "herdr-tower v$ver ($os/$arch) → $BIN_DIR/herdr-tower"
if [ "${HERDR_TOWER_DRY_RUN:-}" = "1" ]; then echo "dry-run: $url"; echo "dry-run: $base/checksums.txt"; exit 0; fi

# The archive is checked against the checksums.txt the same release publishes
# (scripts/release.sh writes it) before anything is extracted, installed or run.
# A download that was cut short, swapped on the way, or replaced on a mirror
# stops here; so does a release with no checksum for this archive. Nothing is
# installed on any failure below — the script exits before `install`.
refuse() { echo "refusing to install: $*" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  sha256_check() { sha256sum -c "$1"; }        # Linux (coreutils)
elif command -v shasum >/dev/null 2>&1; then
  sha256_check() { shasum -a 256 -c "$1"; }    # macOS
else
  refuse "neither sha256sum nor shasum is available to verify the download"
fi

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$url" -o "$tmp/$archive"
curl -fsSL "$base/checksums.txt" -o "$tmp/checksums.txt" ||
  refuse "could not download checksums.txt for v$ver"
# Exactly the one line naming this archive, compared by field rather than by
# pattern, so a file name is never read as a regular expression.
awk -v f="$archive" '$2 == f' "$tmp/checksums.txt" > "$tmp/want.txt"
[ "$(wc -l < "$tmp/want.txt")" -eq 1 ] || refuse "checksums.txt has no single entry for $archive"
(cd "$tmp" && sha256_check want.txt >/dev/null) || refuse "$archive does not match its checksum"
tar -xzf "$tmp/$archive" -C "$tmp" herdr-tower
mkdir -p "$BIN_DIR"
install -m 0755 "$tmp/herdr-tower" "$BIN_DIR/herdr-tower"
"$BIN_DIR/herdr-tower" daemon install
echo "installed: $("$BIN_DIR/herdr-tower" version 2>/dev/null | head -1 || echo "$BIN_DIR/herdr-tower")"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "add $BIN_DIR to your PATH: export PATH=\"$BIN_DIR:\$PATH\"" ;; esac
echo "next: herdr-tower init"
