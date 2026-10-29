# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

# Installs herdr-tower on Windows — fetches the latest release zip (or
# $env:HERDR_TOWER_VERSION) and puts the binary in ~\.local\bin\herdr-tower.exe.
#   powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/orascope/herdr-tower/main/install.ps1 | iex"
# The Windows counterpart of install.sh, with the same rule: nothing is
# installed unless the archive matches the checksums.txt its release publishes.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Repo = 'orascope/herdr-tower'
$BinDir = if ($env:HERDR_TOWER_BIN_DIR) { $env:HERDR_TOWER_BIN_DIR } else { Join-Path $env:USERPROFILE '.local\bin' }

function Refuse([string]$why) { throw "refusing to install: $why" }

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
    throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE (releases are Windows x86_64)"
}

$ver = $env:HERDR_TOWER_VERSION
if (-not $ver) {
    # The tag comes from the releases/latest redirect rather than the API,
    # which rate-limits unauthenticated callers.
    $ver = (& curl.exe -fsSLI -o NUL -w '%{url_effective}' "https://github.com/$Repo/releases/latest") -replace '.*/tag/', ''
    if ($LASTEXITCODE -ne 0 -or -not $ver) { throw 'could not find the latest release' }
}
$ver = $ver.TrimStart('v')
$base = "https://github.com/$Repo/releases/download/v$ver"
$archive = "herdr-tower_${ver}_windows_amd64.zip"
$target = Join-Path $BinDir 'herdr-tower.exe'
Write-Host "herdr-tower v$ver (windows/amd64) -> $target"
if ($env:HERDR_TOWER_DRY_RUN -eq '1') { Write-Host "dry-run: $base/$archive"; Write-Host "dry-run: $base/checksums.txt"; return }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("herdr-tower-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    & curl.exe -fsSL "$base/$archive" -o (Join-Path $tmp $archive)
    if ($LASTEXITCODE -ne 0) { throw "could not download $archive" }
    & curl.exe -fsSL "$base/checksums.txt" -o (Join-Path $tmp 'checksums.txt')
    if ($LASTEXITCODE -ne 0) { Refuse "could not download checksums.txt for v$ver" }

    # Exactly the one line naming this archive, compared by field.
    $want = @(Get-Content (Join-Path $tmp 'checksums.txt') | ForEach-Object {
        $f = $_ -split '\s+', 2
        if ($f.Count -eq 2 -and $f[1].Trim() -eq $archive) { $f[0].ToLowerInvariant() }
    })
    if ($want.Count -ne 1) { Refuse "checksums.txt has no single entry for $archive" }
    $got = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $archive)).Hash.ToLowerInvariant()
    if ($got -ne $want[0]) { Refuse "$archive does not match its checksum" }

    Expand-Archive -Path (Join-Path $tmp $archive) -DestinationPath (Join-Path $tmp 'x') -Force
    New-Item -ItemType Directory -Force $BinDir | Out-Null
    # A running .exe cannot be overwritten, but it can be renamed out of the
    # way; the old copy is removed on the next install, once nothing runs it.
    Get-ChildItem -Path $BinDir -Filter 'herdr-tower.exe.old-*' -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue
    if (Test-Path $target) { Rename-Item $target ("herdr-tower.exe.old-" + [Guid]::NewGuid().ToString('N')) }
    Copy-Item (Join-Path $tmp 'x\herdr-tower.exe') $target
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

& $target daemon install
Write-Host ("installed: " + ((& $target version 2>$null | Select-Object -First 1) -join ''))

# On the user's PATH, as herdr's own installer does, so a new terminal finds it.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($userPath -split ';') -contains $BinDir)) {
    [Environment]::SetEnvironmentVariable('Path', ($(if ($userPath) { "$userPath;" } else { '' }) + $BinDir), 'User')
    Write-Host "added $BinDir to your PATH (new terminals pick it up)"
}
Write-Host 'next: herdr-tower init'
