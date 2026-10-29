#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)
# Installs a built release on other machines over ssh: the archive that fits
# each machine's OS and architecture, its checksum, the binary, the resident
# supervisor, and a restart of the dock tower so it runs the new binary.
#
#   scripts/remote-up.sh <vX.Y.Z> <release-dir> [ssh-target ...]
#
# <release-dir> is what scripts/release.sh left (the archives and
# checksums.txt). Without targets, every enabled machine `herdr machine list`
# knows is upgraded, by the address herdr itself connects to — a label is not
# always resolvable (a tailnet machine's short name is not).
#
# Per machine, in order, stopping that machine at the first failure:
#   1. the archive and checksums.txt go to ~/.cache/herdr-tower-upgrade/;
#   2. the archive is checked against checksums.txt there;
#   3. the binary is installed to ~/.local/bin/herdr-tower;
#   4. `herdr-tower daemon install` (launchd or systemd --user);
#   5. a dock tower of the ssh account — the binary run with no arguments,
#      never another user's on a shared machine — is sent TERM, and the
#      supervisor starts it again from the new binary;
#   6. the cache folder is removed.
# Boards, `daemon run` and the other herdr-tower processes are left alone.
#
# A Windows machine (OpenSSH server, PowerShell) gets the same steps from a
# PowerShell script: the windows_amd64 zip, its checksum, the binary at
# ~\.local\bin\herdr-tower.exe (the running one renamed out of the way, as
# Windows will not overwrite it), `daemon install`, and the dock tower ended
# so the supervisor starts the new one.
set -uo pipefail

usage="usage: scripts/remote-up.sh <vX.Y.Z> <release-dir> [ssh-target ...]"
version=${1:?$usage}
dir=${2:?$usage}
shift 2
[[ $version =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a version: $version ($usage)" >&2; exit 2; }
n=${version#v}
dir=$(cd "$dir" && pwd)
[ -f "$dir/checksums.txt" ] || { echo "$dir has no checksums.txt" >&2; exit 2; }

targets=("$@")
if [ ${#targets[@]} -eq 0 ]; then
  # id, label, address, profile, state
  while read -r _ _ addr _ state; do
    [ "$state" = enabled ] && targets+=("$addr")
  done < <(herdr machine list)
fi
[ ${#targets[@]} -gt 0 ] || { echo "no machines to upgrade" >&2; exit 2; }

ssh_opts=(-o BatchMode=yes -o ConnectTimeout=8)

# Runs the PowerShell script on stdin on Windows host $1, encoded so no shell
# on the way reads it.
ps_remote() {
  local enc
  enc=$( { echo '$ProgressPreference = "SilentlyContinue"'; cat; } | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')
  ssh "${ssh_opts[@]}" "$1" "powershell -NoProfile -NonInteractive -EncodedCommand $enc"
}
cache=.cache/herdr-tower-upgrade
failed=0

for h in "${targets[@]}"; do
  echo "===== $h"
  info=$(ssh "${ssh_opts[@]}" "$h" 'echo "$(uname -s) $(uname -m)"' 2>/dev/null)
  case $info in
    Darwin* | Linux*) ;;
    *)
      info=$(echo 'Write-Output ("Windows " + $env:PROCESSOR_ARCHITECTURE)' | ps_remote "$h" 2>/dev/null | tr -d '\r') \
        || { echo "  ssh failed"; failed=1; continue; }
      ;;
  esac
  if [ "$info" = "Windows AMD64" ]; then
    a=herdr-tower_${n}_windows_amd64.zip
    [ -f "$dir/$a" ] || { echo "  $dir/$a is missing"; failed=1; continue; }
    echo "New-Item -ItemType Directory -Force (Join-Path \$env:USERPROFILE '$cache') | Out-Null" | ps_remote "$h" \
      && scp -q "${ssh_opts[@]}" "$dir/$a" "$dir/checksums.txt" "$h:$cache/" \
      || { echo "  copy failed"; failed=1; continue; }
    {
      echo "\$a = '$a'; \$d = Join-Path \$env:USERPROFILE '$cache'"
      cat <<'REMOTE'
$ErrorActionPreference = 'Stop'
Set-Location $d
$want = @(Get-Content checksums.txt | ForEach-Object { $f = $_ -split '\s+', 2; if ($f.Count -eq 2 -and $f[1].Trim() -eq $a) { $f[0].ToLowerInvariant() } })
$got = (Get-FileHash -Algorithm SHA256 $a).Hash.ToLowerInvariant()
if ($want.Count -ne 1 -or $want[0] -ne $got) { throw "checksum mismatch for $a" }
Expand-Archive -Path $a -DestinationPath x -Force
$bin = Join-Path $env:USERPROFILE '.local\bin'
New-Item -ItemType Directory -Force $bin | Out-Null
$exe = Join-Path $bin 'herdr-tower.exe'
Get-ChildItem $bin -Filter 'herdr-tower.exe.old-*' -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
if (Test-Path $exe) { Rename-Item $exe ('herdr-tower.exe.old-' + [Guid]::NewGuid().ToString('N')) }
Copy-Item x\herdr-tower.exe $exe
Set-Location $env:USERPROFILE
& $exe version | Select-Object -First 1
& $exe daemon install 2>&1 | Select-Object -Last 2
# A dock is the bare binary: its command line names nothing after the path.
# Only this account's: another user's dock on the same machine is not ours.
Get-CimInstance Win32_Process -Filter "Name='herdr-tower.exe'" | Where-Object { $_.CommandLine -and $_.CommandLine.Trim() -match 'herdr-tower\.exe"?$' -and ($_ | Invoke-CimMethod -MethodName GetOwner).User -eq $env:USERNAME } | ForEach-Object { "dock pid $($_.ProcessId) -> end"; Stop-Process -Id $_.ProcessId -Force }
Remove-Item -Recurse -Force $d
REMOTE
    } | ps_remote "$h" || { echo "  FAILED"; failed=1; }
    continue
  fi
  case $info in
    "Darwin arm64") t=darwin_arm64 ;;
    "Linux x86_64") t=linux_amd64 ;;
    "Linux aarch64" | "Linux arm64") t=linux_arm64 ;;
    *) echo "  no release archive for: $info"; failed=1; continue ;;
  esac
  a=herdr-tower_${n}_${t}.tar.gz
  [ -f "$dir/$a" ] || { echo "  $dir/$a is missing"; failed=1; continue; }
  ssh "${ssh_opts[@]}" "$h" "mkdir -p \"\$HOME/$cache\"" \
    && scp -q "${ssh_opts[@]}" "$dir/$a" "$dir/checksums.txt" "$h:$cache/" \
    || { echo "  copy failed"; failed=1; continue; }
  ssh "${ssh_opts[@]}" "$h" bash -s "$a" "$cache" <<'REMOTE' || { echo "  FAILED"; failed=1; }
set -uo pipefail
a=$1; d=$HOME/$2; cd "$d" || exit 1
want=$(awk -v f="$a" '$2 == f { print $1 }' checksums.txt)
if command -v sha256sum >/dev/null; then got=$(sha256sum "$a" | awk '{ print $1 }'); else got=$(shasum -a 256 "$a" | awk '{ print $1 }'); fi
[ -n "$want" ] && [ "$want" = "$got" ] || { echo "  checksum mismatch for $a"; exit 1; }
tar -xzf "$a" herdr-tower 2>/dev/null || tar -xzf "$a" herdr-tower || exit 1
mkdir -p "$HOME/.local/bin"
install -m 0755 herdr-tower "$HOME/.local/bin/herdr-tower.new" \
  && mv "$HOME/.local/bin/herdr-tower.new" "$HOME/.local/bin/herdr-tower" || exit 1
"$HOME/.local/bin/herdr-tower" version | head -1
"$HOME/.local/bin/herdr-tower" daemon install 2>&1 | tail -2
# A dock is the bare binary, whatever the home directory is spelled as —
# this account's only: another user's dock on the same machine is not ours.
for p in $(pgrep -u "$(id -u)" -f '/\.local/bin/herdr-tower$'); do
  echo "dock pid $p -> TERM"
  kill -TERM "$p"
done
cd "$HOME" && rm -rf "$d"
REMOTE
done
exit $failed
