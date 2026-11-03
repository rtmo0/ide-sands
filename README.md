> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# Sands

[![Sands CI](https://github.com/rtmo0/ide-sands/actions/workflows/sands_ci.yml/badge.svg)](https://github.com/rtmo0/ide-sands/actions/workflows/sands_ci.yml)

Sands is a code editor where every project lives in its own [Docker Sandbox](https://docs.docker.com/ai/sandboxes/):
its repositories, coding agents (Claude Code, Codex and others), toolchains, debuggers and connected services such as
databases, Kubernetes or S3. The host only runs Sands and `sbx`.

Sands is a fork of [Zed](https://github.com/zed-industries/zed). The work plan and current status are in
[SANDS_TODO.md](./SANDS_TODO.md).

### Building

Requirements on macOS:

- Xcode with the Metal Toolchain (`xcodebuild -downloadComponent MetalToolchain`)
- [rustup](https://rustup.rs) (the toolchain version is pinned in `rust-toolchain.toml`)
- `zig` and `cargo-zigbuild`, used by development builds to cross-compile the remote server for sandboxes
- [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) (`sbx`) 0.50.x, signed in with `sbx login`

```sh
cargo run            # development build
script/bundle-mac -d # Sands Dev.app
```

Upstream build guides still apply for platform details:
[macOS](./docs/src/development/macos.md), [Linux](./docs/src/development/linux.md),
[Windows](./docs/src/development/windows.md).

### Licensing

Sands, like Zed, is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked (such as GPUI).
The bundled Sands themes are based on [JetBrains Themes](https://github.com/artemevsevev/zed-theme-jetbrains)
by Artem Evsevev (Apache-2.0), see `assets/themes/LICENSES`.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).
