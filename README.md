# winfsp-ufs

<img src="assets/logo.svg" width="96" alt="ufsmount logo">

Mount a FreeBSD UFS2 disk on Windows as a read-only drive.
`ufsmount.exe` reads the raw disk and serves the files through WinFsp.
Explorer, copy and robocopy work as on any drive.

## Requirements

- Windows 10 or 11, x64.
- [WinFsp](https://github.com/winfsp/winfsp/releases) 2.2.26286 (2026 Beta5) or later.
  Older versions have kernel vulnerabilities (CVE-2026-3006 and others).
- Administrator rights to read a physical disk.

## Install

Manual for now:

1. Install WinFsp.
2. Get `ufsmount.exe` from the [releases](../../releases), or build it (see Build).
3. Copy it to `C:\Program Files\ufsmount\` and add that folder to your PATH.

## Start and stop

Find the disk number with `Get-Disk` (here: 1).

Start, from an elevated prompt:

```
ufsmount mount PhysicalDrive1 U:
```

- U: is there while the command runs. Explorer and all programs see it.
- Stop: press Ctrl+C in that window, or close it. U: goes away.
- Unplugging the disk also stops it.

## Other commands

Also from an elevated prompt:

```
ufsmount probe PhysicalDrive1
ufsmount ls PhysicalDrive1 /home
ufsmount cp PhysicalDrive1 /home/me D:\backup
```

- `probe` lists partitions and UFS file systems.
- `--part s1a` picks a partition when there is more than one.
- Image files work too: `ufsmount mount disk.img U:`.

With the WinFsp launcher (`net use`), no window stays open:

```
ufsmount register
net use U: \\ufs\PhysicalDrive1 /persistent:no
net use U: /delete
```

- `register` once; `net use` starts, `net use /delete` stops.
- Both need an elevated prompt.
- Windows keeps `net use` drives per logon.
  A drive mapped in an elevated prompt is not seen by your normal Explorer.
  Use `mount` for Explorer.

## Limits

- Read-only. UFS1 is not supported.
- A file system that was not unmounted cleanly is read as is.
- A bad sector fails only the files that use it.
- Names Windows cannot show (`:`, `?`, invalid UTF-8, ...) map to private-use characters.
  `--charset latin1` shows non-UTF-8 names as Latin-1.
- Symlinks are Windows symlinks. Use `robocopy /XJ` or `/SL` to avoid loops.
- Names are case-sensitive, as on FreeBSD. Opening a file in another case works if it is unique.
  Wildcards such as `dir *.TXT` match case-sensitively.
- Device nodes, fifos and sockets are hidden.

## Build

Needs Rust stable, Visual Studio Build Tools (C++) and LLVM (libclang).

```
cargo build --release
cargo test
```

The WinFsp mount test runs with `UFSMOUNT_MOUNT_TEST=1`.

## Credits

- [rufs / fuse-ufs](https://github.com/realchonk/fuse-ufs) by Benjamin Stuerz and contributors (BSD-2-Clause).
  It is vendored in `vendor/fuse-ufs`. Our fixes are separate commits, meant for upstream.
- WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos.
  https://github.com/winfsp/winfsp
- [winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs) by SnowflakePowered and contributors (GPL-3.0).
- The daemon in the logo is a nod to the BSD Daemon by Marshall Kirk McKusick.

## License

AGPL-3.0-or-later, see `LICENSE`.
`vendor/fuse-ufs` keeps its BSD-2-Clause license.
