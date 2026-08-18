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

Install zig 0.13.0 (must be this version for cargo-zigbuild compatibility):
- **Windows**: download from <https://ziglang.org/download/0.13.0/zig-windows-x86_64-0.13.0.zip>
- **Linux**: download from <https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz>
- **macOS**: `brew install zig@0.13`

### Zig path (this machine)

> **重要：每次交叉编译前必须把 zig 加入 PATH，否则 cargo-zigbuild 找不到 linker。**

本机 zig 已下载解压在固定位置，编译前执行：

```pwsh
# PowerShell — 临时加入 PATH（每次新开终端都要执行）
$env:Path = "C:\Users\xuanfengttp\AppData\Local\Temp\zig-clean\zig-windows-x86_64-0.13.0;$env:Path"

# 验证
zig version  # 应输出 0.13.0
```

如果上述路径不存在（临时目录被清理），重新下载：

```pwsh
curl.exe -L --proxy http://127.0.0.1:7890 -o "$env:TEMP\zig.zip" "https://ziglang.org/download/0.13.0/zig-windows-x86_64-0.13.0.zip"
Expand-Archive "$env:TEMP\zig.zip" "$env:TEMP\zig-clean" -Force
$env:Path = "$env:TEMP\zig-clean\zig-windows-x86_64-0.13.0;$env:Path"
```

## Build

### Windows (native)

```sh
cargo build --release --target x86_64-pc-windows-msvc
```

### Linux targets (cross-compile, zig required in PATH)

```pwsh
# 1. 把 zig 加入 PATH（每次新终端必须执行）
$env:Path = "C:\Users\xuanfengttp\AppData\Local\Temp\zig-clean\zig-windows-x86_64-0.13.0;$env:Path"

# 2. 编译 4 个 Linux 目标
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target armv7-unknown-linux-musleabihf
cargo zigbuild --release --target armv7-unknown-linux-musleabi
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

### 一次性编译全部 5 个目标

```pwsh
$env:Path = "C:\Users\xuanfengttp\AppData\Local\Temp\zig-clean\zig-windows-x86_64-0.13.0;$env:Path"

# Windows
cargo build --release --target x86_64-pc-windows-msvc
# Linux x86_64
cargo zigbuild --release --target x86_64-unknown-linux-musl
# Linux aarch64
cargo zigbuild --release --target aarch64-unknown-linux-musl
# Linux armv7hf
cargo zigbuild --release --target armv7-unknown-linux-musleabihf
# Linux armv7sf
cargo zigbuild --release --target armv7-unknown-linux-musleabi
```

All dependencies are pure-Rust (no C bindings), so no C cross-toolchain is needed beyond zig.

## Package for release

编译完成后，打包 5 个目标到 `release/` 目录：

```pwsh
cd release

# Windows — 直接复制 exe
Copy-Item "..\target\x86_64-pc-windows-msvc\release\dsh-lite.exe" "dsh-lite-windows-x86_64.exe" -Force

# Linux — tar.gz 打包
tar -czf dsh-lite-linux-x86_64.tar.gz  -C ..\target\x86_64-unknown-linux-musl\release     dsh-lite
tar -czf dsh-lite-linux-aarch64.tar.gz -C ..\target\aarch64-unknown-linux-musl\release     dsh-lite
tar -czf dsh-lite-linux-armv7hf.tar.gz -C ..\target\armv7-unknown-linux-musleabihf\release dsh-lite
tar -czf dsh-lite-linux-armv7sf.tar.gz -C ..\target\armv7-unknown-linux-musleabi\release   dsh-lite
```

### 上传到 GitHub Release

```pwsh
$env:HTTPS_PROXY = "http://127.0.0.1:7890"
$ghToken = gh auth token
$releaseId = "<从 GitHub API 获取>"
$headers = @{ "Authorization" = "Bearer $ghToken"; "Accept" = "application/vnd.github+json" }

$assets = @(
  @{file="dsh-lite-windows-x86_64.exe";  mime="application/octet-stream"},
  @{file="dsh-lite-linux-x86_64.tar.gz";  mime="application/gzip"},
  @{file="dsh-lite-linux-aarch64.tar.gz"; mime="application/gzip"},
  @{file="dsh-lite-linux-armv7hf.tar.gz"; mime="application/gzip"},
  @{file="dsh-lite-linux-armv7sf.tar.gz"; mime="application/gzip"}
)
foreach ($a in $assets) {
  $url = "https://uploads.github.com/repos/xuanfengttp/deepseek-harness-lite/releases/$releaseId/assets?name=$($a.file)"
  $bytes = [System.IO.File]::ReadAllBytes((Join-Path (Get-Location) $a.file))
  Invoke-RestMethod -Uri $url -Method POST -Headers $headers -Body $bytes -ContentType $a.mime
  Write-Output "Uploaded: $($a.file)"
}
```
