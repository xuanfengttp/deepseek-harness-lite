# Cross-compilation

DeepSeek Harness Lite targets musl static linking for embedded deployment.
All Linux binaries are statically linked — no glibc, no runtime dependencies.

## Targets

| Target | Platform | Float | Binary size | Use case |
|--------|----------|-------|-------------|----------|
| `x86_64-pc-windows-msvc` | Windows | — | ~2.7 MB | Windows servers, dev machines |
| `aarch64-unknown-linux-musl` | Linux ARM64 | — | ~2.7 MB | Raspberry Pi 4, ARM servers, modern network devices |
| `armv7-unknown-linux-musleabihf` | Linux ARMv7 | hard | ~2.9 MB | Pi 2/3, most ARM routers/switches |
| `armv7-unknown-linux-musleabi` | Linux ARMv7 | soft | ~2.9 MB | Older ARM devices without FPU |
| `x86_64-unknown-linux-musl` | Linux x86_64 | — | ~3.2 MB | x86 servers, dev/test |

## Toolchain

### Install targets

```sh
rustup target add aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf armv7-unknown-linux-musleabi x86_64-unknown-linux-musl
```

### Install cargo-zigbuild + zig

`cargo-zigbuild` uses zig as the linker — no musl cross-toolchain needed.

```sh
cargo install cargo-zigbuild
```

Install zig:
- **Windows**: `winget install zig.zig`
- **Linux**: download from <https://ziglang.org/download/> or `snap install zig --classic`
- **macOS**: `brew install zig`

## Build

```sh
# Windows (native)
cargo build --release --target x86_64-pc-windows-msvc

# Linux ARM64 (cross-compile from any host)
cargo zigbuild --release --target aarch64-unknown-linux-musl

# Linux ARMv7 hard-float
cargo zigbuild --release --target armv7-unknown-linux-musleabihf

# Linux ARMv7 soft-float
cargo zigbuild --release --target armv7-unknown-linux-musleabi

# Linux x86_64
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

All dependencies are pure-Rust (no C bindings), so no C cross-toolchain is needed beyond zig.

## Package for release

Use the `packages.ps1` script to build all targets and create archives:

```pwsh
pwsh -File packages.ps1 -Version 0.1.0-rc.6
# → release-packages/dsh-lite-0.1.0-rc.6-windows-x86_64.zip
# → release-packages/dsh-lite-0.1.0-rc.6-linux-arm64.tar.gz
# → release-packages/dsh-lite-0.1.0-rc.6-linux-armv7hf.tar.gz
# → release-packages/dsh-lite-0.1.0-rc.6-linux-armv7sf.tar.gz
# → release-packages/dsh-lite-0.1.0-rc.6-linux-x86_64.tar.gz
```

Each archive contains: binary + `config.yaml` + `skills/` + `README.md`.

See [RELEASE.md](../RELEASE.md) for the full release workflow and GitHub Actions setup.
