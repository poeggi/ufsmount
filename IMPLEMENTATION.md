# Implementation plan

How `ufsmount.exe` gets built. Goals and milestones are in `PLAN.md`.

## Toolchain

- Rust stable, target `x86_64-pc-windows-msvc`. Nightly rustfmt only for the rufs patches.
- Visual Studio Build Tools (C++ workload, Windows SDK) and LLVM (libclang for bindgen in `winfsp-sys`).
- WinFsp 2.2.26286 (2026 Beta5) or later at runtime. Raise the minimum to the 2026 final once released.
  Build against the WinFsp 2.1 headers in `winfsp-sys`; 2.2 only adds one constant.
- Release build: `crt-static`, linker flag `/DEPENDENTLOADFLAG:0x800`, unwinding panics (the winfsp crate catches them).

## Repository layout

```
Cargo.toml            package `ufsmount` (lib + bin), [workspace] exclude = ["vendor"]
build.rs              delay-load link lines for winfsp-x64.dll
src/main.rs           CLI entry
src/lib.rs            module tree, shared by main and tests
src/device.rs         raw disk / image access, alignment, cache, bad sectors, unplug
src/partition.rs      MBR, BSD disklabel, GPT, UFS superblock probe
src/names.rs          UFS name bytes <-> UTF-16
src/volume.rs         UFS access on top of rufs: lookup, dir cache, attributes
src/winfs.rs          WinFsp FileSystemContext
src/security.rs       the one security descriptor
src/launcher.rs       launcher mode, argument checks, register / unregister
tests/                integration tests
vendor/fuse-ufs/      upstream fuse-ufs, git subtree, pinned to 36bc35a
```

- License AGPL-3.0-or-later, SPDX line in each source file.
- Dependencies: `winfsp` (`default-features = false`, `delayload`), `windows`, `clap`, `log`,
  `env_logger`, `anyhow`, `thiserror`, `crc32fast`. Dev: `zstd`.

## Step 1: repo setup and rufs subtree (done)

- `vendor/fuse-ufs` added with `git subtree add --squash` at 36bc35a5fb9e05e214fa3d9619822763d7d8cb9b.
- rufs is a path dependency. Cargo finds its upstream workspace for inherited keys.
- Vendored tests run with `--locked --ignore-rust-version`. Clippy on our crate runs with `--no-deps`.

## Step 2: rufs patches

One commit per patch, only under `vendor/fuse-ufs`, formatted with nightly rustfmt (upstream config),
each with a `ChangeLog.md` line and a message fit for an upstream PR. With tests where possible.

- P1 Non-Unix targets.
  - Internals work on byte names: `Header::name() -> &[u8]`, `check_name_is_legal(&[u8])`.
  - Byte APIs for reading: `dir_iter_bytes`, `dir_lookup_bytes`, xattr list/read by bytes.
  - The existing `OsStr` APIs stay as thin wrappers, `cfg(unix)`.
  - `BlockReader::open` / `Ufs::open` stay on all targets: block size from `blksize()` on unix, else 4096.
  - `ENOATTR` gets a value on other targets (`libc::ENODATA` if libc has it for windows-msvc).
- P2 Directory entries with inode 0. FreeBSD zeroes `d_ino` of a deleted first entry in a 512-byte chunk
  and keeps `reclen`. rufs stops the chunk there and loses the live entries behind it.
  Skip such entries; only `reclen == 0` or a decode error ends the chunk. Unit test with a built block.
- P3 No panics on bad disk data, `EIO` instead:
  directory reclen underflow, invalid `d_type`, `DT_UNKNOWN` (resolve the type from the inode),
  directory size assert, inode mode with an unknown type (checked in `read_inode`),
  `Inode::size` on special files, block pointers outside the file system (checked, with `checked_mul`),
  `inode_find_block` / `inode_get_block_size` out of bounds, `check()` unwraps, `info()` overflow.
- P4 Symlinks: short vs long by `size < maxsymlinklen` (as FreeBSD), length bounded by the block size,
  no `blocks <= 8` assert.
- P5 Reads at or past EOF return 0.
- P6 Timestamps saturate instead of panicking (`i64::MIN`, values outside `SystemTime`).
- P7 The decoder keeps the backend's I/O error instead of replacing it with "failed to decode".
- P8 Read-only open skips the per-cylinder-group checks (2596 scattered reads on the target disk,
  and one bad sector would refuse the mount). They protect the allocator, so they stay for read-write.
- P9 `Info` exposes superblock facts: `clean`, `flags`, `volname`, `fsmnt`, `id`, `size`, `maxsymlinklen`.
- Not patched: the `Write` bound on the backend (our reader returns an error). PLAN.md is aligned.
- Run rufs' tests on Windows after the patches.

## Step 3: device and partitions (`device.rs`, `partition.rs`)

- Source: `PhysicalDriveN`, `\\.\PhysicalDriveN`, or an image file path.
- Open with `GENERIC_READ` only, share read+write. Access denied gives "run as administrator".
- At open, query length (`IOCTL_DISK_GET_LENGTH_INFO`) and logical sector size
  (`IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`). Images: length from metadata, sector size 512.
  `--sector-size` overrides (some USB bridges change it).
- The OS read sits behind a small trait. Tests use a fake device that enforces the raw-disk rules
  (sector-multiple offset and length, aligned buffer, error 27 on reads crossing the end) and injects faults.
- Reads:
  - Chunk cache: 64 KiB chunks at absolute, aligned device offsets; LRU, 64 MiB; one Mutex.
  - The last chunk is clamped to the device length (sector aligned). Never read past the end.
  - Bounce buffers aligned to 4096 in memory. `seek_read` is looped until full.
  - A failed chunk read is retried for the requested range only, one sector at a time.
    Failed sectors go into a small negative cache, so USB timeouts are not repeated.
    Failures are never cached as data.
  - Unplug codes (1167, 433, 21, 55, 1110) mark the device gone: fail fast, stop the mount.
    Never reopen a drive by number.
- Errors: `io::Error::other(DeviceError { win32, offset })`, never a raw OS error.
  The device also keeps the last error, so the volume layer can recover the Win32 code
  even where rufs replaced the error.
- `PartitionReader { dev, base, len, pos }`: `Read + Seek + Write`.
  `len` is rounded down to the rufs block size. A read starting at or past `len` is an error, not a
  short read (rufs would turn a short read into zeros). `seek` returns the exact position. `Write` errors.
- rufs block size (`BlockReader::new`) = `fsize`, read from the superblock during probing.
  On-disk structs then never straddle a block; the device cache does the real caching.
- Probe (all offsets in logical sectors of the device; images try 512, then 4096):
  - Candidates: the whole source; each MBR primary entry with start != 0 (start 0 is FreeBSD boot1's fake entry);
    each 0xA5 slice itself; a BSD label at sector 1 of each slice and of the whole disk
    (dangerously dedicated); every non-empty GPT entry (GPT found by "EFI PART" at LBA 1).
  - Label: magic at 0 and 132, `npartitions` bounded (at most 20), entries at 148, 16 bytes each.
    Offsets relative to `c` (`p_offset - c.offset`). Checked arithmetic against the device length.
    `d_secsize` cross-checked, bad checksum only warned. `fstype` ignored.
  - GPT: header CRC over `header_size` (92..sector size), entry array CRC, bounded `num_entries` and
    `entry_size`. Backup header at the last LBA when the primary fails. Type GUID shown as info only.
  - Each candidate is checked for a UFS2 superblock at +65536 (magic, both byte orders), with
    `fs_sblockloc == 65536` and `fs_size * fs_fsize <= candidate length`. UFS1 (+8192) is reported
    as unsupported.
  - Candidates are deduplicated by absolute start; `c` or the bare slice lose against `a` at the same start.
- Partition ids: `s1a` (slice 1, label a; a..t), `s1` (bare slice), `p2` (GPT), `whole`.
  `--part ID` selects; without it the only remaining candidate, else an error listing them.

## Step 4: UFS volume layer (`volume.rs`, `names.rs`)

- `Volume { ufs: Mutex<Ufs<PartitionReader>>, dirs: Mutex<DirCache>, info, opts }`.
  Poisoned locks are recovered with `into_inner`: read-only rufs state cannot be left inconsistent.
- `DirTable`: one directory read once with `dir_iter_bytes`, without `.` and `..`.
  Maps: UFS bytes -> entry, and folded Windows name -> (entry, count) for the case-insensitive fallback.
  `DirCache`: LRU bounded by total entries (200k). The cache lock is not held while rufs reads.
- Lookup: split on `\`, map each component back to UFS bytes, walk from the root.
  Exact match first; else the folded map, used only if the count is 1. Folding = `to_lowercase` on the
  Windows name. Misses (Explorer's `desktop.ini` probes) are hash misses, not scans.
- Hidden: device nodes, fifos, sockets, whiteouts.
- Names (`names.rs`), reversible:
  - Valid UTF-8 becomes UTF-16 as is.
  - Characters Windows forbids (`"*:<>?\|`, 0x01-0x1F) map to U+F000 + char (Cygwin convention),
    as do a trailing space or dot (not for `.` and `..`).
  - Non-UTF-8 bytes map to U+F100 + byte, or with `--charset latin1` decode as Latin-1.
  - Reverse lookup tries the mapped meaning first, then the literal one.
    Two UFS names mapping to one Windows name: the listing keeps one and logs the other.
  - DOS device names (`NUL`, `CON`, ...) are documented as a limitation.
- Attributes:
  - Directory: DIRECTORY. Regular file: NORMAL (no READONLY bit; copies stay writable).
  - Symlink: REPARSE_POINT with tag IO_REPARSE_TAG_SYMLINK, plus DIRECTORY if the target resolves to a
    directory (depth limit 32; failure means file link). Absolute targets starting with the last mount
    point (`fsmnt`) are made volume-relative.
  - Size 0 for directories and symlinks. Allocation size `blocks * 512`. Index = inode number.
  - Times: creation = birthtime (mtime if 0), write = mtime, access = atime, change = ctime, as FILETIME.
- A failing entry (unreadable or freed inode) is listed with zero size and times and logged.
  It never fails the directory listing.
- Dirty file system: warn at open with the flags (soft updates, SU+J, needs fsck). Read as is.
- Volume size and free space come from `Info` in fragments times `fsize`.

## Step 5: WinFsp mount (`winfs.rs`, `security.rs`)

- Startup: `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)`, then load `winfsp-x64.dll` with
  `LoadLibraryExW(<InstallDir>\bin\winfsp-x64.dll, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32)`,
  then `winfsp_init()`. Check the DLL's `VS_FIXEDFILEINFO` is at least 2.2.26286; refuse below.
- `VolumeParams`: read_only_volume, case_sensitive_search, case_preserved_names, unicode_on_disk,
  reparse_points, pass_query_directory_filename (with `use_dir_info_by_name`), max_component_length 255,
  sector_size 4096, sectors_per_allocation_unit max(1, fsize / 4096), file_info_timeout 60 s,
  filesystem_name "UFS", volume serial from the superblock id, volume_prefix only in launcher mode.
  `persistent_acls` stays off on purpose: copies take the destination's ACLs.
- `FileContext { inr, kind, dir: DirBuffer }`.
- Callbacks:
  - `get_security_by_name`: reparse resolver first, then lookup; attributes plus the shared SD.
  - `open`: write or delete access, or FILE_DELETE_ON_CLOSE, gives STATUS_MEDIA_WRITE_PROTECTED.
    A symlink in the middle of the path gives OBJECT_PATH_NOT_FOUND (kernel callers skip the resolver).
  - `close`, `get_file_info` (with reparse tag), `get_security` (shared SD), `get_volume_info`
    (label from volname, else "UFS").
  - `read`: offset >= size gives STATUS_END_OF_FILE; else read, clamped to EOF.
  - `read_directory`: `if let Ok(lock) = dir.acquire(marker.is_none(), None)` fills from the DirTable
    (`.` and `..` except in the root); then `dir.read(marker, buffer)`. Names via `set_name_raw`.
  - `get_dir_info_by_name`: one hash lookup in the DirTable.
  - `get_reparse_point_by_name` / `get_reparse_point`: kind checked first. Non-symlink gives
    STATUS_NOT_A_REPARSE_POINT, missing gives OBJECT_NAME_NOT_FOUND, short buffer gives BUFFER_TOO_SMALL.
    Buffer: IO_REPARSE_TAG_SYMLINK, SubstituteName and PrintName, `/` -> `\`, names mapped,
    SYMLINK_FLAG_RELATIVE; absolute targets become `\...` (volume-relative).
  - All writing callbacks (`create`, `overwrite`, `write`, `set_basic_info`, `set_file_size`, `set_delete`,
    `rename`, `set_security`, `set_reparse_point`, `delete_reparse_point`, `set_volume_label`, `flush`)
    return STATUS_MEDIA_WRITE_PROTECTED. `read_only_volume` itself enforces nothing.
- Errors are explicit NTSTATUS values: ENOENT -> OBJECT_NAME_NOT_FOUND, ENOTDIR -> OBJECT_PATH_NOT_FOUND,
  DeviceError CRC -> DEVICE_DATA_ERROR, other DeviceError -> IO_DEVICE_ERROR, device gone ->
  DEVICE_NOT_CONNECTED, everything else -> FILE_CORRUPT_ERROR. A failed read fails only that request.
  Logs name errno values by name (on Windows, errno 5 would print as "Access is denied").
- Security descriptor: `O:BAG:BAD:P(A;;FRFX;;;SY)(A;;FRFX;;;BA)(A;;FRFX;;;<user SID>)`, built once,
  self-relative. User = token user (manual) or `-U` (launcher). `--sddl` overrides and is validated:
  owner and group present, at most 1024 bytes (WinFsp loops on larger ones).
- Threads: default fine guard, `start_with_threads(4)`; rufs access is serialized anyway.

## Step 6: CLI (`main.rs`) and launcher mode (`launcher.rs`)

```
ufsmount probe <source>
ufsmount ls    <source> [--part ID] [path]
ufsmount cp    <source> [--part ID] <ufs-path> <dest>    (file or tree; unreadable files skipped and logged)
ufsmount mount <source> [--part ID] <X:|dir> [--sddl SDDL] [--charset utf8|latin1]
ufsmount register | unregister
ufsmount -u <\ufs\Instance> -m <X:|""> -U <DOMAIN\user>   (launcher mode)
```

- Common options: `--sector-size`, `-v` (log level). `--version` prints the WinFsp credit line
  required by its FLOSS exception.
- `probe`, `ls`, `cp` work without WinFsp, to test the disk path end to end first.
- Manual `mount` on `X:` mounts through the Mount Manager (`\\.\X:`): a global drive, so the same user's
  non-elevated Explorer sees it. Runs as a `FileSystemService` in console mode (Ctrl+C, Ctrl+Break, close).
  The disk is opened and probed before the service starts.
- Launcher mode:
  - `-u` must match `^\\ufs\\PhysicalDrive[0-9]{1,3}(s[1-4]([a-t])?|p[0-9]{1,3})?\\?$`, case-insensitive.
  - `-m` must be `^[A-Za-z]:$` or empty (then no mount point). `-U` is resolved with `LookupAccountNameW`;
    failure stops the start, never a wider SD.
  - Ambiguous partitions: the error lists the candidates in the log.
- `register` (admin): refuses unless the exe and its folder are writable only by admins and SYSTEM
  (install under Program Files). Writes `HKLM\SOFTWARE\WOW6432Node\WinFsp\Services\ufs`:
  `Executable`, `CommandLine = -u %1 -m %2 -U %U`, `JobControl = 1`,
  `Stderr = %ProgramData%\ufsmount\log\%T.log` (folder created admin-only). `Security` stays at the
  WinFsp default (SYSTEM and Administrators). `unregister` removes the key.
- Docs: `net use U: \\ufs\PhysicalDrive1 /persistent:no` from an elevated prompt. robocopy with `/XJ`
  or `/SL` against symlink loops.

## Step 7: tests

- Unit: partition parsing on built buffers (target layout, a and c at 0, absolute label offsets,
  boot1 fake MBR, GPT good / bad primary + good backup, mixed-endian GUID, 4096-byte sectors);
  parsers never panic on random and truncated buffers; name mapping round trips; launcher argument checks;
  SD validation.
- Fake device: tail chunk readable, bad sector under file data / inode block / directory block fails only
  that object with the right DeviceError, neighbours in the same chunk still readable, unplug fails cleanly.
- Images: unpack the upstream `ufs-little` and `ufs-big` images. Checks: listing, file1, the 1 MiB file3,
  link1, long-link, sparse files, nested dirs, xattrs. Times pinned from one read.
- Partitioned images built in the test: target layout byte for byte (MBR 0xA5 at LBA 63, label a at 16,
  fstype 0, image at (63+16)*512), plus GPT and label variants. The image suite runs on each.
- Byte-patched images: `fs_clean = 0`, a name byte 0xFF, a `:` in a name, an inode with S_IFCHR,
  a block pointer outside the fs, a deleted first entry in a dir chunk.
- Mount test (needs WinFsp; env var opt-in): mount the image on a free drive letter, walk it with `std::fs`
  from the test process, compare with the volume layer, read a symlink, try a write (must fail), unmount.
- Real disk (manual, elevated): probe, ls, cp, mount, Explorer from a non-elevated process,
  `robocopy /E /XJ`, then register and `net use`.

## Step 8: README, credits, CI

- README: short. What it is, requirements, manual install, usage, limits, credits.
- Credits: rufs / fuse-ufs (Benjamin Stuerz, Alan Somers, Davids Paskevics; BSD-2-Clause),
  WinFsp (Bill Zissimopoulos; required credit line), winfsp-rs (SnowflakePowered; GPL-3.0).
- CI (`.github/workflows/ci.yml`): one `windows-latest` job, cancel superseded runs, cache.
  fmt, clippy `--no-deps -D warnings`, tests, rufs tests (`--locked`), WinFsp MSI pinned by SHA-256 and
  installed silently, mount test, release build, `ufsmount.exe` as artifact.

## Not in this round

- Installer (PLAN milestone 8). UFS1, MBR extended partitions. rufs performance work.

## Commits

Local commits on `main`, nothing pushed: one per rufs patch, then device/partition, volume + CLI,
WinFsp mount, launcher mode, tests, README + CI.
