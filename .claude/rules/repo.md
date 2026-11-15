# winfsp-ufs: repo, build, CI

ufsmount mounts FreeBSD UFS2 file systems read-only on Windows through WinFsp.
Write support is not a feature. UFS1 is not supported (rufs has no UFS1).

## Layout
- `src/` the crate: CLI (`main.rs`), library (`lib.rs`), tray (`tray/`, `bin/ufsmount-tray.rs`).
- `installer/` Inno Setup scripts. `logos/` logo, icon, wizard images.
- `vendor/fuse-ufs/` upstream fuse-ufs with rufs, a git subtree. See rufs.md.
- All build output and other temporary files go to `temp/` (git-ignored; set in `.cargo/config.toml`).
  The setup lands in `temp/installer`. Nothing built is committed: releases carry the exes and the setup.

## Build and test
- Rust stable, MSVC target, VS Build Tools (C++), LLVM (libclang for bindgen in winfsp-sys).
- Before a commit: `cargo fmt --check`, `cargo clippy --all-targets --locked --no-deps -- -D warnings`, `cargo test --locked`.
- Mount tests need WinFsp and run only with `UFSMOUNT_MOUNT_TEST=1`.
  Tests in `tests/mount.rs` run in parallel; each test uses its own drive letter range.
- rufs tests: `cargo test --manifest-path vendor/fuse-ufs/Cargo.toml -p rufs --locked --ignore-rust-version`,
  with `CARGO_TARGET_DIR=temp/vendor`.
- WinFsp minimum version: `installer/winfsp.iss` is the one source. A Rust test checks `MIN_WINFSP` against it.
- `winfsp-x64.dll` is delay-loaded by full path from the WinFsp InstallDir, never from the exe folder.

## CI (`.github/workflows/ci.yml`)
- build: fmt, clippy, tests with a WinFsp mount, release build, the setup, and a 0.0.1 setup for the downgrade test.
- smoke: a fresh runner without WinFsp. Installs (downloads WinFsp), mounts a test image through the tray,
  downgrades and upgrades, uninstalls, and checks that nothing is left and PATH is unchanged.
- release: tags `v*` only. The tag must equal the Cargo version. See the release skill.

## Conventions
- Releases only on the user's explicit request.
- ASCII only in every file. Each source file has the SPDX line `AGPL-3.0-or-later`.
- Dependencies: the latest stable release, checked upstream.
- `THIRD-PARTY-NOTICES.txt` is maintained by hand. When dependencies change, compare it with
  `cargo tree -e normal --target x86_64-pc-windows-msvc --prefix none --format "{p}|{l}"`.
- Licenses: ours AGPL-3.0-or-later. rufs BSD-2-Clause, winfsp-rs GPL-3.0, WinFsp GPL-3.0 with FLOSS exception.
  Dual-licensed crates are used under Apache-2.0.
