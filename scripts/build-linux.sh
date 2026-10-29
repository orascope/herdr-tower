#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)
# Builds the Linux release binaries inside manylinux_2_28 containers.
#
# manylinux_2_28 ships glibc 2.28, so what it links runs on RHEL / Rocky /
# Alma 8 (8.10 included) and on every newer distribution. A binary linked on
# a newer system asks for newer glibc symbols and refuses to start there
# ("GLIBC_2.29 not found"): the earlier cross-build did exactly that.
#
#   scripts/build-linux.sh <out-dir> [aarch64] [x86_64]
#
# Leaves <out-dir>/herdr-tower-linux-<arch>. The version stamp comes from
# HERDR_TOWER_VERSION / _COMMIT / _DATE when set (a release), as for every
# other build. The Rust toolchain and the cargo registry live in named docker
# volumes, so only the first run downloads them; the source is mounted
# read-only and built with --locked.
#
# Each container is named for its role, `herdr-tower-build-<arch>-<version>`
# (`dev` outside a release), and runs with --network host.
#
# An arch can be built on another machine's docker: set
# HERDR_TOWER_BUILD_HOST_X86_64 / HERDR_TOWER_BUILD_HOST_AARCH64 to an ssh
# host. The tracked files are streamed there, built in a temporary directory,
# and the binary is copied back; nothing is left behind but the named volumes.
set -euo pipefail

out=${1:?usage: scripts/build-linux.sh <out-dir> [aarch64] [x86_64]}
shift
arches=("$@")
[ ${#arches[@]} -eq 0 ] && arches=(aarch64 x86_64)

root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)

# The build, as a script run inside the container.
inner='
  if [ ! -x /root/.cargo/bin/cargo ]; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path >/dev/null
  fi
  export PATH=/root/.cargo/bin:$PATH
  rustup update stable --no-self-update >/dev/null 2>&1 || true
  cd /src/rust
  cargo build --release --locked -p tower_cli
  cp /target/release/herdr-tower "/out/herdr-tower-linux-$ARCH"
  # The newest glibc symbol it asks for must be one 2.28 has.
  need=$(objdump -T "/out/herdr-tower-linux-$ARCH" | grep -o "GLIBC_[0-9.]*" | sort -uV | tail -1)
  echo "    $ARCH: needs $need"
  case $need in
    GLIBC_2.2[0-8]|GLIBC_2.1[0-9]|GLIBC_2.[0-9]) ;;
    *) echo "    $ARCH asks for $need, newer than manylinux_2_28 allows" >&2; exit 1 ;;
  esac
'

for arch in "${arches[@]}"; do
  case $arch in
    aarch64) platform=linux/arm64; host=${HERDR_TOWER_BUILD_HOST_AARCH64-} ;;
    x86_64) platform=linux/amd64; host=${HERDR_TOWER_BUILD_HOST_X86_64-} ;;
    *) echo "unknown arch: $arch" >&2; exit 2 ;;
  esac
  name="herdr-tower-build-$arch-${HERDR_TOWER_VERSION:-dev}"
  where=${host:-this machine}
  echo "==> $arch in quay.io/pypa/manylinux_2_28_$arch on $where ($name)"
  started=$SECONDS

  src=$root
  dest=$out
  if [ -n "$host" ]; then
    src=$(ssh "$host" mktemp -d)
    dest=$(ssh "$host" mktemp -d)
    git -C "$root" ls-files -z | tar -C "$root" --null -T - -cf - | ssh "$host" tar -x -C "$src"
  fi

  run=(docker run --rm --name "$name" --network host --platform "$platform"
    -v "$src":/src:ro
    -v "$dest":/out
    -v "herdr-tower-rustup-$arch":/root/.rustup
    -v "herdr-tower-cargo-$arch":/root/.cargo
    -v "herdr-tower-target-$arch":/target
    -e CARGO_TARGET_DIR=/target
    -e "HERDR_TOWER_VERSION=${HERDR_TOWER_VERSION-}"
    -e "HERDR_TOWER_COMMIT=${HERDR_TOWER_COMMIT-}"
    -e "HERDR_TOWER_DATE=${HERDR_TOWER_DATE-}"
    -e ARCH="$arch"
    "quay.io/pypa/manylinux_2_28_$arch" bash -euo pipefail -c "$inner")

  if [ -n "$host" ]; then
    ssh "$host" "docker rm -f $name >/dev/null 2>&1 || true"
    ssh "$host" "$(printf '%q ' "${run[@]}")"
    scp -q "$host:$dest/herdr-tower-linux-$arch" "$out/"
    ssh "$host" rm -rf "$src" "$dest"
  else
    docker rm -f "$name" >/dev/null 2>&1 || true
    "${run[@]}"
  fi
  echo "    $arch built in $((SECONDS - started))s on $where"
done
ls -l "$out"
