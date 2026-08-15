# packages.ps1 — Build and package dsh-lite for all platforms
# Usage: pwsh -File packages.ps1 [-Version 0.1.0-rc.6]
param(
    [string]$Version = "0.1.0-rc.6"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSCommandPath
$out = Join-Path $Root "release-packages"
New-Item -ItemType Directory -Force -Path $out | Out-Null

# Locate zig for cargo-zigbuild
$zigBase = Get-ChildItem -Path "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Directory -Filter "zig.zig_*" -ErrorAction SilentlyContinue | Select-Object -First 1
if ($zigBase) {
    $zigExe = Get-ChildItem -Path $zigBase.FullName -Recurse -Filter "zig.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($zigExe) {
        $env:PATH = "$($zigExe.DirectoryName);$env:PATH"
        Write-Host "✓ zig found: $(zig version)"
    }
}

$targets = @(
    @{ name = "windows-x86_64";   bin = "target\x86_64-pc-windows-msvc\release\dsh-lite.exe";    ext = "zip";    cmd = "cargo build --release --target x86_64-pc-windows-msvc" },
    @{ name = "linux-arm64";      bin = "target\aarch64-unknown-linux-musl\release\dsh-lite";     ext = "tar.gz"; cmd = "cargo zigbuild --release --target aarch64-unknown-linux-musl" },
    @{ name = "linux-armv7hf";    bin = "target\armv7-unknown-linux-musleabihf\release\dsh-lite"; ext = "tar.gz"; cmd = "cargo zigbuild --release --target armv7-unknown-linux-musleabihf" },
    @{ name = "linux-armv7sf";    bin = "target\armv7-unknown-linux-musleabi\release\dsh-lite";   ext = "tar.gz"; cmd = "cargo zigbuild --release --target armv7-unknown-linux-musleabi" },
    @{ name = "linux-x86_64";     bin = "target\x86_64-unknown-linux-musl\release\dsh-lite";      ext = "tar.gz"; cmd = "cargo zigbuild --release --target x86_64-unknown-linux-musl" }
)

foreach ($t in $targets) {
    $pkgName = "dsh-lite-$Version-$($t.name)"
    $pkgDir = Join-Path $Root $pkgName

    Write-Host ""
    Write-Host "── Building $($t.name) ──" -ForegroundColor Cyan

    # Build
    Push-Location $Root
    Invoke-Expression $t.cmd
    if ($LASTEXITCODE -ne 0) {
        Write-Host "✗ Build failed for $($t.name)" -ForegroundColor Red
        Pop-Location
        continue
    }
    Pop-Location

    $binPath = Join-Path $Root $t.bin
    if (-not (Test-Path $binPath)) {
        Write-Host "✗ Binary not found: $($t.bin)" -ForegroundColor Red
        continue
    }

    # Package
    New-Item -ItemType Directory -Force -Path $pkgDir | Out-Null
    Copy-Item $binPath $pkgDir
    Copy-Item (Join-Path $Root "config\default.yaml") (Join-Path $pkgDir "config.yaml")
    Copy-Item -Recurse (Join-Path $Root "skills") $pkgDir
    Copy-Item (Join-Path $Root "README.md") $pkgDir

    if ($t.ext -eq "zip") {
        $archive = Join-Path $out "$pkgName.zip"
        Compress-Archive -Path "$pkgDir\*" -DestinationPath $archive -Force
    } else {
        $archive = Join-Path $out "$pkgName.tar.gz"
        Push-Location $Root
        tar czf $archive $pkgName
        Pop-Location
    }

    Remove-Item -Recurse -Force $pkgDir
    $sizeKB = [math]::Round((Get-Item $archive).Length / 1024, 1)
    Write-Host "✓ $pkgName.$($t.ext)  ($sizeKB KB)" -ForegroundColor Green
}

Write-Host ""
Write-Host "All packages in: $out" -ForegroundColor Yellow
Get-ChildItem $out | Format-Table Name, @{N='SizeKB';E={[math]::Round($_.Length/1024,1)}}
