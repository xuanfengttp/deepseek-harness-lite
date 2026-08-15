# Release Packaging

Build and package dsh-lite for all supported platforms.

## Prerequisites

### Windows host (current setup)

```pwsh
# Rust targets (already installed)
rustup target add aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf armv7-unknown-linux-musleabi x86_64-unknown-linux-musl

# cargo-zigbuild (already installed)
cargo install cargo-zigbuild

# zig (already installed via winget)
winget install zig.zig
# Add to PATH: C:\Users\<you>\AppData\Local\Microsoft\WinGet\Packages\zig.zig_*\zig-x86_64-windows-*\
```

### Linux host

```sh
rustup target add aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf armv7-unknown-linux-musleabi x86_64-unknown-linux-musl
cargo install cargo-zigbuild
# Install zig: https://ziglang.org/download/
```

## Build all targets

```pwsh
# Windows (PowerShell) — build all 5 targets
$zig = Resolve-Path "C:\Users\$env:USERNAME\AppData\Local\Microsoft\WinGet\Packages\zig.zig_*\zig-x86_64-windows-*" | Select-Object -First 1
$env:PATH = "$($zig.Path);$env:PATH"

# 1. Windows x86_64 (native)
cargo build --release --target x86_64-pc-windows-msvc

# 2. Linux ARM64 (musl static)
cargo zigbuild --release --target aarch64-unknown-linux-musl

# 3. Linux ARMv7 hard-float (musl static)
cargo zigbuild --release --target armv7-unknown-linux-musleabihf

# 4. Linux ARMv7 soft-float (musl static)
cargo zigbuild --release --target armv7-unknown-linux-musleabi

# 5. Linux x86_64 (musl static)
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

## Package for release

Each release package contains:
- `dsh-lite` (or `dsh-lite.exe`) — the binary
- `config.yaml` — default config (user edits API key/endpoint)
- `skills/` — bundled skill files
- `README.md` — quick start guide

### Package script

```pwsh
# packages.ps1 — creates release archives for all platforms
$version = "0.1.0-rc.6"
$out = "release-packages"
New-Item -ItemType Directory -Force -Path $out

$targets = @(
    @{ name = "windows-x86_64";   bin = "target\x86_64-pc-windows-msvc\release\dsh-lite.exe";  ext = "zip" },
    @{ name = "linux-arm64";      bin = "target\aarch64-unknown-linux-musl\release\dsh-lite";   ext = "tar.gz" },
    @{ name = "linux-armv7hf";    bin = "target\armv7-unknown-linux-musleabihf\release\dsh-lite"; ext = "tar.gz" },
    @{ name = "linux-armv7sf";    bin = "target\armv7-unknown-linux-musleabi\release\dsh-lite";  ext = "tar.gz" },
    @{ name = "linux-x86_64";     bin = "target\x86_64-unknown-linux-musl\release\dsh-lite";    ext = "tar.gz" }
)

foreach ($t in $targets) {
    $pkg = "dsh-lite-$version-$($t.name)"
    New-Item -ItemType Directory -Force -Path $pkg
    Copy-Item $t.bin "$pkg\"
    Copy-Item "config\default.yaml" "$pkg\config.yaml"
    Copy-Item -Recurse "skills" "$pkg\skills"
    Copy-Item "README.md" "$pkg\"
    if ($t.ext -eq "zip") {
        Compress-Archive -Path "$pkg\*" -DestinationPath "$out\$pkg.zip" -Force
    } else {
        tar czf "$out\$pkg.tar.gz" $pkg
    }
    Remove-Item -Recurse -Force $pkg
    Write-Output "✓ $pkg.$($t.ext)"
}
```

## GitHub Actions (automated)

Push a version tag to trigger automated release builds:

```sh
git tag v0.1.0-rc.6
git push origin v0.1.0-rc.6
```

The `.github/workflows/release.yml` workflow will:
1. Build all 5 targets in parallel (Linux runners for musl, Windows runner for MSVC)
2. Package each with config + skills + README
3. Create a GitHub Release with all archives attached

You can also trigger manually via GitHub Actions UI → "Run workflow".

## Supported targets

| Target | Platform | Float | Binary size | Notes |
|--------|----------|-------|-------------|-------|
| `x86_64-pc-windows-msvc` | Windows | — | ~2.7 MB | Native Windows build |
| `aarch64-unknown-linux-musl` | Linux ARM64 | — | ~2.7 MB | Raspberry Pi 4, ARM servers |
| `armv7-unknown-linux-musleabihf` | Linux ARMv7 | hard | ~2.9 MB | Most ARM devices (Pi 2/3, routers) |
| `armv7-unknown-linux-musleabi` | Linux ARMv7 | soft | ~2.9 MB | Older ARM devices without FPU |
| `x86_64-unknown-linux-musl` | Linux x86_64 | — | ~3.2 MB | x86 servers, dev/test |

All Linux binaries are **statically linked** (musl) — no glibc, no runtime dependencies, drop-in deploy.

## Adding new targets

To add a new architecture:

1. `rustup target add <target>`
2. Add to `.cargo/config.toml` if linker config needed
3. Add to `release.yml` matrix
4. Add to the package script
5. Build and verify
