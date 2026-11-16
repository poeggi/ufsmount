# ufsmount: repo, build, CI

ufsmount mounts FreeBSD UFS2 file systems read-only on Windows through WinFsp.
Write support is not a feature. UFS1 is not supported (rufs has no UFS1).

## Layout
A Cargo workspace. Shared code names no product; each product changes only its own folder.
Other products use win-mountkit as a git dependency on a release tag, and install side by side with ufsmount:
every system name comes from the product id, and only WinFsp is shared. A per-volume detail
(such as the file system name in `fs::Info`) comes from the backend, not from `Product`.
A git dependency carries no link setup. Each product supplies its own: `+crt-static` (`.cargo/config.toml`),
the link args of `products/ufsmount/build.rs`, the tray manifest (Common Controls 6), and `Product.winfsp_rs`
from its own Cargo.lock. New fields in `Product`, `Found` or `Info` break a product's build until it sets them.
- `crates/win-mountkit/` shared: disk access, partition tables, the volume layer, WinFsp, launcher, `cp`,
  CLI (`cli.rs`), tray (`tray/`). `product.rs` is the `Product` struct, `fs.rs` the `Backend`/`Fs` traits.
- `products/ufsmount/` ufsmount: `src/lib.rs` (`PRODUCT`: names, URLs, texts), `src/ufs.rs` (UFS through rufs:
  BSD labels, superblock), the two thin exes, `build.rs` (icon, manifest, link flags), `installer.iss` (defines),
  `logos/`, `THIRD-PARTY-NOTICES.txt`, `tests/`.
- `installer/` shared setup: `common.iss` (included by each product's `installer.iss`), `winfsp.iss`.
- `docs/` images for the README.
- `vendor/fuse-ufs/` upstream fuse-ufs with rufs, a git subtree. See rufs.md.
- All build output and other temporary files go to `temp/` (git-ignored; set in `.cargo/config.toml`).
  The setup lands in `temp/installer`. Nothing built is committed: releases carry the exes and the setup.

## Build and test
- Rust stable, MSVC target, VS Build Tools (C++), LLVM (libclang for bindgen in winfsp-sys).
- Before a commit: `cargo fmt --check`, `cargo clippy --all-targets --locked --no-deps -- -D warnings`, `cargo test --locked`.
- Mount tests need WinFsp and run only with `UFSMOUNT_MOUNT_TEST=1`.
  The tests in `tests/mount.rs` mount one at a time (a shared lock): two WinFsp mounts starting
  at the same moment can fail.
- rufs tests: `cargo test --manifest-path vendor/fuse-ufs/Cargo.toml -p rufs --locked --ignore-rust-version`,
  with `CARGO_TARGET_DIR=temp/vendor`.
- WinFsp minimum version: `installer/winfsp.iss` is the one source. A Rust test checks `MIN_WINFSP` against it.
- `winfsp-x64.dll` is delay-loaded (every target, unit tests included) by full path from the WinFsp
  InstallDir, never from the exe folder. Link flags do not carry over between packages: the shared crate's
  `build.rs` sets them for its tests, each product's `build.rs` for its exes and tests (exes also get
  `/DEPENDENTLOADFLAG:0x800`). `/IGNORE:4199` silences targets without imports from a delay-loaded DLL.
  It also hides a `/DELAYLOAD` name that matches nothing: the smoke job catches a static WinFsp import.
- Every exe and test calls `set_product` (ufsmount: `ufsmount::init()`) first. The shared crate's own
  tests use a demo product.

## CI (`.github/workflows/ci.yml`)
- build: fmt, a check that `crates/` never says "ufs", clippy, tests with a WinFsp mount, release build,
  the setup, and a 0.0.1 setup for the downgrade test.
- smoke: a fresh runner without WinFsp. Installs (downloads WinFsp), mounts a test image through the tray,
  downgrades and upgrades, uninstalls, and checks that nothing is left and PATH is unchanged.
- release: tags `v*` only. The tag must equal the version in the root `Cargo.toml` (`[workspace.package]`).
  See the release skill.
- Actions are pinned by commit SHA with the version as a comment. Tag builds run without the cache.
  Every job has a timeout.

## Conventions
- Releases only on the user's explicit request.
- ASCII only in every file. Each source file has the SPDX line `AGPL-3.0-or-later`.
- Dependencies: the latest stable release, checked upstream.
- `products/ufsmount/THIRD-PARTY-NOTICES.txt` is maintained by hand. When dependencies change, compare it with
  `cargo tree -p ufsmount -e normal --target x86_64-pc-windows-msvc --prefix none --format "{p}|{l}"`.
- Licenses: ours AGPL-3.0-or-later. rufs BSD-2-Clause, winfsp-rs GPL-3.0, WinFsp GPL-3.0 with FLOSS exception.
  Dual-licensed crates are used under Apache-2.0.
