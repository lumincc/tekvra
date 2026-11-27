# Tekvra installer (Windows PowerShell): downloads the release .zip for the
# host architecture, verifies its SHA-256 digest against SHA256SUMS.txt, and
# installs the tekvra.exe binary into the install prefix bin directory.
#
# Configuration (environment variables):
#   TEKVRA_VERSION         release tag to install (default: v0.1.0)
#   TEKVRA_INSTALL_PREFIX  install prefix (default: $env:LOCALAPPDATA\Programs\tekvra)
#   TEKVRA_DOWNLOAD_PREFIX base URL for release assets (default: the GitHub release download URL)
#   TEKVRA_FORCE_TARGET    override the detected Rust target triple (e.g. force
#                          aarch64-pc-windows-msvc, which release.yml publishes)
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
$baseUrl = "$TekvraDownloadPrefix/$TekvraVersion"

function Get-ReleaseFile {
    # Downloads a release asset, resolving file:// URLs locally so smoke tests
    # can serve fixtures from disk without a network stack.
    param([string]$Url, [string]$DestPath)
    if ($Url -like "file://*") {
        $localPath = $Url -replace "^file://", ""
        # Windows file URLs carry a leading slash before the drive letter
        # (file:///C:/...); normalize before copying.
        if ($localPath -match "^/[A-Za-z]:") { $localPath = $localPath.Substring(1) }
        Copy-Item -LiteralPath $localPath -Destination $DestPath -Force
    } else {
        Invoke-WebRequest -Uri $Url -OutFile $DestPath -UseBasicParsing
    }
}

if ($env:TEKVRA_FORCE_TARGET) {
    $target = $env:TEKVRA_FORCE_TARGET
} else {
    switch ($env:PROCESSOR_ARCHITECTURE) {
        "AMD64" { $target = "x86_64-pc-windows-msvc" }
        "ARM64" {
            # Prefer the native ARM64 archive when release.yml has published one;
            # fall back cleanly to x86_64 (ARM64 Windows 11 runs x64 binaries
            # natively via the Prism emulator) while the ARM64 job is fresh.
            $target = "x86_64-pc-windows-msvc"
            try {
                $probeUri = "$baseUrl/tekvra-$TekvraVersion-aarch64-pc-windows-msvc.zip"
                $probe = Invoke-WebRequest -Uri $probeUri -Method Head -UseBasicParsing -ErrorAction Stop
                if ($probe.StatusCode -eq 200) { $target = "aarch64-pc-windows-msvc" }
            } catch {
                # Native ARM64 archive absent or unreachable: keep the x86_64 fallback.
            }
        }
        default {
            Write-Error "install.ps1: unsupported architecture '$($env:PROCESSOR_ARCHITECTURE)'"
            exit 1
        }
    }
}
$archive = "tekvra-$TekvraVersion-$target.zip"

$tmpDir = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ("tekvra-install-" + [guid]::NewGuid().ToString("N")))
try {
    Get-ReleaseFile -Url "$baseUrl/$archive" -DestPath (Join-Path $tmpDir $archive)
    Get-ReleaseFile -Url "$baseUrl/SHA256SUMS.txt" -DestPath (Join-Path $tmpDir "SHA256SUMS.txt")

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
    # Release archives produced by `ci_verify_packaging.py --package-release`
    # nest the payload under tekvra-<version>-<target>/ alongside LICENSE,
    # NOTICE.md, THIRD_PARTY_LICENSES/, and docs/. Fall back to a flat archive
    # root for hand-built fixtures that predate the nested layout.
    $bundleDir = Join-Path $tmpDir "tekvra-$TekvraVersion-$target"
    $srcExe = Join-Path $bundleDir "tekvra.exe"
    if (-not (Test-Path $srcExe)) {
        $srcExe = Join-Path $tmpDir "tekvra.exe"
    }
    $binDir = Join-Path $TekvraInstallPrefix "bin"
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    Copy-Item $srcExe (Join-Path $binDir "tekvra.exe") -Force
    Write-Host "installed tekvra $TekvraVersion to $(Join-Path $binDir 'tekvra.exe')"
    if (($env:PATH -split ";") -notcontains $binDir) {
        Write-Host "note: add $binDir to your PATH to use 'tekvra' from any shell"
    }
} finally {
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
}
