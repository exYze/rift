# Rift installer for Windows — https://github.com/exYze/rift
#
#   irm https://raw.githubusercontent.com/exYze/rift/master/install.ps1 | iex
#
# Downloads the latest release binary, verifies its SHA-256 checksum, installs
# it as rift.exe, and adds the install dir to your user PATH. Override the
# install directory with $env:RIFT_INSTALL.

$ErrorActionPreference = 'Stop'
$repo = 'exYze/rift'

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64') {
    throw "rift ships an x64 Windows binary only (yours: $env:PROCESSOR_ARCHITECTURE). Build from source: cargo install --git https://github.com/$repo rift-tui"
}
$target = 'x86_64-pc-windows-msvc'

$installDir = if ($env:RIFT_INSTALL) { $env:RIFT_INSTALL } else { Join-Path $env:LOCALAPPDATA 'Programs\rift' }
New-Item -ItemType Directory -Force -Path $installDir | Out-Null

$base = "https://github.com/$repo/releases/latest/download"
$tmp = Join-Path $env:TEMP "rift-install-$PID"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$exeTmp = Join-Path $tmp 'rift.exe'
$sums = Join-Path $tmp 'checksums.txt'

Write-Host "downloading rift-$target.exe ..."
Invoke-WebRequest -Uri "$base/rift-$target.exe" -OutFile $exeTmp
Invoke-WebRequest -Uri "$base/checksums.txt" -OutFile $sums

# Verify SHA-256 against the published checksums before installing anything.
$hash = (Get-FileHash $exeTmp -Algorithm SHA256).Hash.ToLower()
$line = Select-String -Path $sums -Pattern "rift-$target.exe" | Select-Object -First 1
if (-not $line -or -not $line.Line.ToLower().StartsWith($hash)) {
    throw "checksum verification failed for rift-$target.exe"
}
Write-Host "checksum OK"

$dest = Join-Path $installDir 'rift.exe'
# Windows can't overwrite a running rift.exe, but it can rename it aside first.
try {
    Move-Item $exeTmp $dest -Force
} catch {
    $old = Join-Path $installDir 'rift.old.exe'
    Remove-Item $old -Force -ErrorAction SilentlyContinue
    Rename-Item $dest $old
    Move-Item $exeTmp $dest -Force
}
Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "installed $(& $dest --version) to $dest"

# Add the install dir to the user PATH if it isn't already there.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $installDir) {
    $newPath = if ([string]::IsNullOrEmpty($userPath)) { $installDir } else { "$userPath;$installDir" }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host ""
    Write-Host "added $installDir to your user PATH — open a new terminal, then run:  rift"
} else {
    Write-Host "run it with:  rift"
}
