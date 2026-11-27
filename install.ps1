# Tekvra installer (Windows PowerShell): downloads the release .zip for the
# host architecture, verifies its SHA-256 digest against SHA256SUMS.txt, and
# installs the tekvra.exe binary into the install prefix bin directory.
#
# Configuration (environment variables):
#   TEKVRA_VERSION         release tag to install (default: v0.1.0)
#   TEKVRA_INSTALL_PREFIX  install prefix (default: $env:LOCALAPPDATA\Programs\tekvra)
#   TEKVRA_DOWNLOAD_PREFIX base URL for release assets (default: the GitHub release download URL)
#
# The installer aborts with a non-zero exit status BEFORE placing any binary in
# the target directory when the computed digest does not match SHA256SUMS.txt.
$ErrorActionPreference = "Stop"

$TekvraVersion = if ($env:TEKVRA_VERSION) { $env:TEKVRA_VERSION } else { "v0.1.0" }
$TekvraInstallPrefix = if ($env:TEKVRA_INSTALL_PREFIX) {
    $env:TEKVRA_INSTALL_PREFIX
} else {
    Join-Path $env:LOCALAPPDATA "Programs\tekvra"
}
$TekvraDownloadPrefix = if ($env:TEKVRA_DOWNLOAD_PREFIX) {
    $env:TEKVRA_DOWNLOAD_PREFIX
} else {
    "https://github.com/lumincc/tekvra/releases/download"
}

switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { $arch = "x86_64" }
    "ARM64" { $arch = "aarch64" }
    default {
        Write-Error "install.ps1: unsupported architecture '$($env:PROCESSOR_ARCHITECTURE)'"
        exit 1
    }
}
$target = "$arch-pc-windows-msvc"
$archive = "tekvra-$TekvraVersion-$target.zip"
$baseUrl = "$TekvraDownloadPrefix/$TekvraVersion"

$tmpDir = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ("tekvra-install-" + [guid]::NewGuid().ToString("N")))
try {
    Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile (Join-Path $tmpDir $archive)
    Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS.txt" -OutFile (Join-Path $tmpDir "SHA256SUMS.txt")

    $entry = Select-String -Path (Join-Path $tmpDir "SHA256SUMS.txt") -Pattern ([regex]::Escape($archive)) | Select-Object -First 1
    if (-not $entry) {
        Write-Error "install.ps1: no checksum entry for $archive in SHA256SUMS.txt"
        exit 1
    }
    $expected = ($entry.Line -split "\s+")[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmpDir $archive)).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Write-Error "install.ps1: checksum mismatch for $archive (expected $expected, got $actual)"
        exit 1
    }

    Expand-Archive -Path (Join-Path $tmpDir $archive) -DestinationPath $tmpDir -Force
    $binDir = Join-Path $TekvraInstallPrefix "bin"
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    Copy-Item (Join-Path $tmpDir "tekvra.exe") (Join-Path $binDir "tekvra.exe") -Force
    Write-Host "installed tekvra $TekvraVersion to $(Join-Path $binDir 'tekvra.exe')"
    if (($env:PATH -split ";") -notcontains $binDir) {
        Write-Host "note: add $binDir to your PATH to use 'tekvra' from any shell"
    }
} finally {
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
}
