#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)
# Builds, packages and checks the release binaries on a Mac.
#
# GitHub Actions cannot run here (billing), so releases are built locally:
# macOS natively, Linux in manylinux_2_28 containers (scripts/build-linux.sh),
# Windows x86_64 with the MinGW-w64 cross linker (`brew install mingw-w64`,
# `rustup target add x86_64-pc-windows-gnu`). Nothing is tagged or published — the script ends
# by printing the commands that do that, to be run once the checks pass.
#
#   scripts/release.sh <vX.Y.Z> <out-dir>
#
# Leaves <out-dir>/herdr-tower_<X.Y.Z>_<os>_<arch>.tar.gz (three of them),
# herdr-tower_<X.Y.Z>_windows_amd64.zip and checksums.txt. Builds from the checkout it lives in, at HEAD, which must be
# clean: the version stamp names HEAD's commit, and a dirty tree would ship
# code that commit does not hold.
#
# Checks, each fatal:
#   - every archive matches checksums.txt;
#   - macOS arm64 runs and reports <vX.Y.Z> (this Mac is arm64);
#   - macOS deployment target is 11.0 (arm64);
#   - both Linux binaries run and report <vX.Y.Z> on RHEL 8.10 (UBI), the
#     oldest glibc the release promises;
#   - the Windows binary needs no DLL beyond what Windows ships, and — with
#     HERDR_TOWER_CHECK_HOST_WINDOWS=<ssh target of a Windows machine> —
#     runs there and reports <vX.Y.Z>. Without that host the run is skipped
#     and the script says so.
set -euo pipefail

usage="usage: scripts/release.sh <vX.Y.Z> <out-dir>"
version=${1:?$usage}
out=${2:?$usage}
[[ $version =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a version: $version ($usage)" >&2; exit 2; }
n=${version#v}

root=$(cd "$(dirname "$0")/.." && pwd)
if [ -n "$(git -C "$root" status --porcelain)" ]; then
  echo "the checkout at $root has uncommitted changes; commit or set them aside first" >&2
  exit 1
fi
if git -C "$root" rev-parse -q --verify "refs/tags/$version" >/dev/null; then
  echo "tag $version already exists" >&2
  exit 1
fi

HERDR_TOWER_VERSION=$version
HERDR_TOWER_COMMIT=$(git -C "$root" rev-parse HEAD)
HERDR_TOWER_DATE=$(git -C "$root" log -1 --format=%cI)
export HERDR_TOWER_VERSION HERDR_TOWER_COMMIT HERDR_TOWER_DATE

# Wall-clock time of the whole build and of each step, kept in
# <out-dir>/build-info.txt beside the archives.
release_started=$SECONDS
steps=()
step() { steps+=("$1: $((SECONDS - step_started))s"); }

# <out-dir> is emptied first, so it must hold nothing but an earlier run's
# output: a slip such as "~" must not delete a tree.
if [ -d "$out" ]; then
  for f in "$out"/* "$out"/.[!.]*; do
    [ -e "$f" ] || continue
    case ${f##*/} in
      herdr-tower_*.tar.gz | herdr-tower_*.zip | checksums.txt | build-info.txt) ;;
      *) echo "$out holds $f, which is not release output; pick an empty or new directory" >&2; exit 1 ;;
    esac
  done
fi
rm -rf "$out"
mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "==> macOS arm64"
step_started=$SECONDS
(cd "$root/rust" \
  && MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --locked --release -p tower_cli --target aarch64-apple-darwin)

step "macOS arm64 (this machine)"

echo "==> Windows x86_64"
step_started=$SECONDS
# Stripped: MinGW's import-library symbols otherwise carry this checkout's
# absolute path (the builder's home folder) into the .exe.
(cd "$root/rust" \
  && CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
     CARGO_PROFILE_RELEASE_STRIP=symbols \
     cargo build --locked --release -p tower_cli --target x86_64-pc-windows-gnu)
step "Windows x86_64 (this machine, MinGW-w64)"

echo "==> Linux aarch64 and x86_64"
step_started=$SECONDS
"$root/scripts/build-linux.sh" "$work/linux" aarch64 x86_64
step "Linux aarch64 and x86_64 (aarch64: ${HERDR_TOWER_BUILD_HOST_AARCH64:-this machine}, x86_64: ${HERDR_TOWER_BUILD_HOST_X86_64:-this machine})"

# One binary named herdr-tower per archive, without macOS metadata files, with
# LICENSE and NOTICE beside it (Apache-2.0 §4: they travel with every copy).
# install.sh extracts the binary alone.
pack() {
  local bin=$1 target=$2 d
  d=$(mktemp -d)
  cp "$bin" "$d/herdr-tower"
  chmod 755 "$d/herdr-tower"
  cp "$root/LICENSE" "$root/NOTICE" "$d/"
  (cd "$d" && COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -czf "$out/herdr-tower_${n}_${target}.tar.gz" herdr-tower LICENSE NOTICE)
  rm -rf "$d"
}
pack "$root/rust/target/aarch64-apple-darwin/release/herdr-tower" darwin_arm64
pack "$work/linux/herdr-tower-linux-x86_64" linux_amd64
pack "$work/linux/herdr-tower-linux-aarch64" linux_arm64
# Windows gets a zip, the archive Explorer and PowerShell's Expand-Archive open.
pack_zip() {
  local bin=$1 target=$2 d
  d=$(mktemp -d)
  cp "$bin" "$d/herdr-tower.exe"
  cp "$root/LICENSE" "$root/NOTICE" "$d/"
  (cd "$d" && COPYFILE_DISABLE=1 zip -q -X "$out/herdr-tower_${n}_${target}.zip" herdr-tower.exe LICENSE NOTICE)
  rm -rf "$d"
}
pack_zip "$root/rust/target/x86_64-pc-windows-gnu/release/herdr-tower.exe" windows_amd64
(cd "$out" && shasum -a 256 ./*.tar.gz ./*.zip | sed 's|  \./|  |' | sort -k2 > checksums.txt)

echo "==> checking the archives"
step_started=$SECONDS
(cd "$out" && shasum -a 256 -c checksums.txt)
for target in darwin_arm64 linux_amd64 linux_arm64; do
  mkdir -p "$work/check/$target"
  tar -xzf "$out/herdr-tower_${n}_${target}.tar.gz" -C "$work/check/$target"
done
# The first line of --version is "herdr-tower <version> (commit …)".
reports() {
  local got
  # The whole output, then its first line. `| head -1` closed the pipe after
  # one line, the second write failed with "broken pipe", and under pipefail
  # a good binary failed the release (v0.36.0, docker's `--version`).
  got=$("$@" --version)
  got=${got%%$'\n'*}
  case $got in
    "herdr-tower $version "*) echo "  ok  $got" ;;
    *) echo "  expected $version, got: $got" >&2; exit 1 ;;
  esac
}
reports "$work/check/darwin_arm64/herdr-tower"
checks_macos_minimum() {
  local bin=$1 expected=$2 minimum
  minimum=$(otool -l "$bin" | awk '
    $1 == "cmd" { cmd = $2 }
    (cmd == "LC_BUILD_VERSION" && $1 == "minos") ||
    (cmd == "LC_VERSION_MIN_MACOSX" && $1 == "version") { print $2 }
  ')
  case $minimum in
    "$expected"|"$expected.0") echo "  ok  ${bin%/herdr-tower} requires macOS $minimum" ;;
    *) echo "  expected macOS $expected deployment target, got: $minimum" >&2; exit 1 ;;
  esac
}
checks_macos_minimum "$work/check/darwin_arm64/herdr-tower" 11.0
# A DLL Windows does not ship would make the binary fail to start on a clean
# machine, with a dialog rather than an error.
mkdir -p "$work/check/windows_amd64"
(cd "$work/check/windows_amd64" && unzip -q "$out/herdr-tower_${n}_windows_amd64.zip")
win_exe="$work/check/windows_amd64/herdr-tower.exe"
dlls=$(x86_64-w64-mingw32-objdump -p "$win_exe" | awk '/DLL Name:/ { print tolower($3) }' | sort -u)
for dll in $dlls; do
  case $dll in
    kernel32.dll | ntdll.dll | advapi32.dll | ws2_32.dll | userenv.dll | bcrypt.dll | \
    bcryptprimitives.dll | crypt32.dll | secur32.dll | ole32.dll | oleaut32.dll | shell32.dll | \
    user32.dll | msvcrt.dll | api-ms-win-*) ;;
    *) echo "  the Windows binary needs $dll, which Windows does not ship" >&2; exit 1 ;;
  esac
done
echo "  ok  windows_amd64 needs only system DLLs ($(echo $dlls | tr '\n' ' '))"
# No binary may carry the builder's home folder — in either spelling: a path,
# or MinGW's mangled symbol name (`_Users_<name>_…`).
home_name=${HOME##*/}
for bin in "$work"/check/*/herdr-tower "$win_exe"; do
  if strings -a "$bin" | grep -q -e "$HOME" -e "_Users_${home_name}_" -e "_home_${home_name}_"; then
    echo "  $bin carries the builder's home folder ($HOME)" >&2; exit 1
  fi
done
echo "  ok  no binary names the builder's home folder"
if [ -n "${HERDR_TOWER_CHECK_HOST_WINDOWS:-}" ]; then
  remote_dir="herdr-tower-verify-$version"
  ssh "$HERDR_TOWER_CHECK_HOST_WINDOWS" "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force \$env:TEMP\\$remote_dir | Out-Null\""
  scp -q "$win_exe" "$HERDR_TOWER_CHECK_HOST_WINDOWS:AppData/Local/Temp/$remote_dir/herdr-tower.exe"
  reports ssh "$HERDR_TOWER_CHECK_HOST_WINDOWS" "AppData\\Local\\Temp\\$remote_dir\\herdr-tower.exe"
  ssh "$HERDR_TOWER_CHECK_HOST_WINDOWS" "powershell -NoProfile -Command \"Remove-Item -Recurse -Force \$env:TEMP\\$remote_dir\"" || true
else
  echo "  --  windows_amd64 not run: set HERDR_TOWER_CHECK_HOST_WINDOWS=<ssh target> to run it on Windows"
fi
for arch in amd64 arm64; do
  reports docker run --rm --name "herdr-tower-verify-ubi8-linux-$arch-$version" \
    --network host --platform "linux/$arch" \
    -v "$work/check/linux_$arch":/b:ro redhat/ubi8:8.10 /b/herdr-tower
done

step "checking the archives (UBI 8.10 containers on this machine)"
{
  echo "$version $HERDR_TOWER_COMMIT"
  echo "total build time: $((SECONDS - release_started))s"
  printf '%s\n' "${steps[@]}"
} | tee "$out/build-info.txt"

# Every release note ends with a link to history.md, the record of all
# releases; the publish command appends it, so a note never ships without it.
history_link="[Release history](https://github.com/orascope/herdr-tower/blob/main/history.md)"

cat <<EOF

$version built from $HERDR_TOWER_COMMIT and checked. To publish:

  git -C "$root" tag $version $HERDR_TOWER_COMMIT
  git -C "$root" push origin $version
  { cat <notes.md>; printf '\\n%s\\n' '$history_link'; } |
    gh release create $version --verify-tag --title $version --latest \\
      --notes-file - "$out"/*.tar.gz "$out"/*.zip "$out/checksums.txt"
EOF
