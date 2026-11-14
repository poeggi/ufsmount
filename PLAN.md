# winfsp-ufs plan

## Goal

Mount a FreeBSD UFS/UFS2 disk on Windows as a drive letter.
Read-only.
Files show up in Explorer and work with any program (copy, robocopy).

## Approach

- WinFsp provides the signed kernel driver and a user-mode library.
- We build one user-mode program: `ufsmount.exe`.
- It reads the raw disk, parses UFS and answers WinFsp's file requests.
- No own kernel code, no driver signing.
- Language: Rust.
- UFS parsing: reuse `rufs` (the library behind realchonk's fuse-ufs).
  - Vendored as a git subtree: `vendor/fuse-ufs`, the full upstream repo, squashed, pinned to one upstream commit.
  - Our changes to it are separate commits that touch only `vendor/fuse-ufs`.
    Each one is written so it can go upstream as is.
  - Upstream them with `git format-patch --relative=vendor/fuse-ufs` and `git am` in a fork.
  - Take upstream updates with `git subtree pull --squash`.
- WinFsp binding: the `winfsp` crate (winfsp-rs). Maintained, has a service wrapper, builds without WinFsp installed.
  - Build machines and CI need libclang (bindgen in `winfsp-sys`).
  - Delay-load the DLL. Load `winfsp-x64.dll` by full path from the WinFsp InstallDir (registry), never from the exe folder.
- WinFsp version: require 2.2.26286 (2026 Beta5) or later. Raise it to the 2026 final once released.
  2.1 (the latest stable) has CVE-2026-3006; later betas fix more kernel bugs.
- License: AGPL-3.0 (`LICENSE`).
  rufs is BSD-2-Clause, winfsp-rs is GPL-3.0, WinFsp is GPL-3.0 with a FLOSS exception.
  All three combine with AGPL-3.0.

## How it runs

- Manual: `ufsmount.exe -d \\.\PhysicalDriveN -m U:` (needs admin for raw disk access).
- The drive exists while the program runs. Ctrl+C unmounts.
- Later via the WinFsp launcher: `net use U: \\ufs\PhysicalDriveN`.
  The launcher runs it as SYSTEM with `-u %1 -m %2`.
  By default only SYSTEM and elevated admins may start it. Never grant Everyone: that exposes every raw disk.
  Accept only instance names matching `PhysicalDrive[0-9]+`.

## Disk layout to support

- MBR with a FreeBSD slice (type 0xA5), then a BSD disklabel, then UFS.
- GPT with a freebsd-ufs partition.
- A bare UFS image file (for tests).
- UFS2 first. UFS1 is not in rufs (upstream PR #110 is open).
- Find UFS by its superblock magic, not by the disklabel fstype.
  A UFS partition can be labeled fstype 0 (unused).

## Milestones

Status: 1 to 8 done and tested on the real disk.

1. Check the base. Done.
2. Vendor and patch rufs (each patch a separate commit, fit for upstream).
   The patch list is in `IMPLEMENTATION.md`, Step 2.
3. Disk access.
   - Open `\\.\PhysicalDriveN` read-only (sector-aligned reads).
   - Parse MBR, GPT and BSD disklabel. List the UFS partitions found.
   - Feed rufs with a block reader on top of this.
4. CLI tool without WinFsp.
   - `ufsmount ls <path>` and `ufsmount cp <src> <dst>`.
   - Proves the parsing on the real disk before any mount code exists.
5. WinFsp mount.
   - Open, read, directory listing, file info, timestamps.
   - Map UFS mode bits to Windows attributes (read-only).
   - Symlinks: show as Windows symlinks (reparse points). Device files and sockets: hide.
   - Catch panics in every callback; a panic across the C boundary kills the process.
   - File names: UFS bytes to UTF-16; handle invalid UTF-8.
6. Tests.
   - Build small UFS2 images (on FreeBSD or with makefs) as test fixtures.
   - Compare file lists and checksums against the source tree.
7. Launcher registration.
   - Register under the WinFsp launcher (like `fsreg.bat`).
8. Installer and tray app (see `INSTALLER-PLAN.md`).
   - Checks for WinFsp and its version. Points to the official MSI; no bundling while only betas are safe.
   - Copies `ufsmount.exe` and registers it with the launcher.
   - Uses the logo: `assets/ufsmount.ico` (also the exe icon) and `assets/logo.svg`.

## Out of scope

- Write support.
- Soft-updates journal replay. A dirty file system is read as-is.

## Open points

- Symlinks over a `net use` mount: Windows may block remote symlink evaluation. Test it.
- Behaviour on bad sectors of old disks: skip the file and log it, never abort the whole copy.
