# Cross-compilation

DeepSeek Harness Lite targets musl static linking for embedded deployment.

## Targets

| Target | Architecture | Float |
|---|---|---|
| `aarch64-unknown-linux-musl` | ARM 64-bit | — |
| `armv7-unknown-linux-musleabihf` | ARMv7 | hard-float |
| `armv7-unknown-linux-musleabi` | ARMv7 | soft-float |
| `x86_64-unknown-linux-musl` | x86_64 (dev) | — |

## Install targets

```sh
rustup target add aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf armv7-unknown-linux-musleabi x86_64-unknown-linux-musl
```

## Build

On a Linux host with musl:

```sh
cargo build --release --target aarch64-unknown-linux-musl
cargo build --release --target armv7-unknown-linux-musleabihf
cargo build --release --target armv7-unknown-linux-musleabi
```

## Cross-compilation from Windows / without musl

Use `cargo-zigbuild` (recommended, no extra toolchain):

```sh
cargo install cargo-zigbuild
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

Or `cross` (Docker-based):

```sh
cargo install cross
cross build --release --target aarch64-unknown-linux-musl
```

All dependencies are pure-Rust, so no C cross-toolchain is needed beyond the linker.
