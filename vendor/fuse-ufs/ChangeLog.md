# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),

## [Unreleased] - ReleaseDAte

### General

- The MSRV is now 1.90.0.
- rufs: no `needless_range_loop` warnings from newer clippy

### Add

- rufs: build on non-Unix targets, such as Windows
- rufs: `dir_iter_bytes()` and `dir_lookup_bytes()`, which pass names as raw on-disk bytes
- rufs: `Info` has the file system size in fragments, clean flag, status flags, id, volume name and last mount point
- rufs: open NetBSD's UFS2 with extended attributes (FFSv2ea magic 0x19012038)

### Fix

- rufs: backend I/O errors keep their kind and cause instead of becoming "failed to decode"
- rufs: a read-only open no longer reads every cylinder group
- rufs: `inode_read()` past the end of a file returns 0 instead of panicking
- rufs: timestamps that `SystemTime` cannot hold no longer panic; times before the epoch with nanoseconds are read and written correctly
- rufs: short symlinks with extattr blocks, and long symlinks that end in a fragment
- rufs: reject superblocks with impossible sizes or offsets; open file systems whose `sbsize` is not `fsize` (512 and 1024-byte fragments)
- rufs: inode numbers past the last cylinder group and unknown file types give `EIO`
- rufs: reading a device node, fifo or socket returns no data instead of panicking; block indexes past the file give `EIO`
- rufs: block pointers past the end of the file system give `EIO` on the read path
- rufs: corrupt extattr areas give `EIO` instead of panicking
- rufs: directory records leave room for the NUL after the name, as FreeBSD requires
- rufs: directory entries behind an unused (deleted) first entry of a chunk are listed and found
- rufs: corrupt directory records and directories with holes give `EIO` instead of panicking or looping; a read-only listing skips the rest of a corrupt chunk
- rufs: entries with `DT_UNKNOWN` or an undefined d_type take their type from the inode instead of panicking
- rufs: a superblock with a negative `fs_fpg` gives `EIO` instead of panicking in the summary log
- rufs: reject superblocks whose inode blocks do not fit in a cylinder group, as FreeBSD does
- rufs: the superblock layout no longer depends on the pointer size, so 32-bit targets read it correctly
- rufs: after a failed read, `BlockReader` no longer returns zeros or stale data on the next read
- rufs: reading the end of a file whose size is close to 2^64 gives an error instead of an overflow panic
- rufs: data, pointer and extattr blocks that run past the end of the file system give `EIO`, not just those that start past it
- rufs: a directory larger than the file system gives `EIO` instead of being read for that size

## [0.6.0] - 2025-09-27

### General

- Update dependencies
- rufs: more logging

### Add

- `truncate()`
- `rename()`

### Fix

- free block accounting
- fuse2: `readlink()` when buffer is too small

## [0.5.0] - 2025-05-15

### Added 

- fuse-ufs: add cargo features for specific platforms ([#84](https://github.com/realchonk/fuse-ufs/pull/84))
- write support ([#84](https://github.com/realchonk/fuse-ufs/pull/84))
- support for FUSE2 on Linux and FreeBSD (through fuse2rs) ([#84](https://github.com/realchonk/fuse-ufs/pull/84))

## [0.4.4] - 2025-02-16

### Add

- A little more documentation
- allow other configurations of UFS ([#93](https://github.com/realchonk/fuse-ufs/pull/93))

### Fix

- calculation of fragment blocks ([#89](https://github.com/realchonk/fuse-ufs/pull/89))
- reading directories ([#87](https://github.com/realchonk/fuse-ufs/pull/87))
- inode reading ([#92](https://github.com/realchonk/fuse-ufs/pull/92))
- update dependencies

## [0.4.3] - 2024-10-25

### Fix

- fuse-ufs: mounting without `-f` on FUSE3
- scripts/release: run test suite before publishing

## [0.4.2] - 2024-10-25

### Fix

- another fix of publishing

## [0.4.1] - 2024-10-25

### Fix

- minor publishing fix

## [0.4.0] - 2024-10-25

### Added

- basic fuzzing framework ([#74](https://github.com/realchonk/fuse-ufs/pull/74))
- pre-mount checks for verifying the filesystem ([#74](https://github.com/realchonk/fuse-ufs/pull/74))
- support for OpenBSD/FUSE2 via fuse2rs ([#79](https://github.com/realchonk/fuse-ufs/pull/79))

### Changed

- split project into binary and library part ([#74](https://github.com/realchonk/fuse-ufs/pull/74))
- make fuser optional ([#79](https://github.com/realchonk/fuse-ufs/pull/79))

## [0.3.0] - 2024-08-22

This was the final release as part of [Google Summer of Code 2024](https://summerofcode.withgoogle.com/programs/2024/projects/mCAcivuH).

### Added

- ChangeLog ([#62](https://github.com/realchonk/fuse-ufs/pull/62))
- man page ([#57](https://github.com/realchonk/fuse-ufs/pull/57))
- `-v` and `-q` flags ([#66](https://github.com/realchonk/fuse-ufs/pull/66))
- `-f` option ([#68](https://github.com/realchonk/fuse-ufs/pull/68))
- the ability to mount via `/etc/fstab` on Linux ([#69](https://github.com/realchonk/fuse-ufs/pull/69))

### Changed

- fuse-ufs now starts in the background by default ([#68](https://github.com/realchonk/fuse-ufs/pull/68))

### Fixed

- indirect block addressing ([#63](https://github.com/realchonk/fuse-ufs/pull/63))

### [0.2.1] - 2024-08-15

### Changed

- README
- Fix Cargo.toml for publishing

### [0.2.0] - 2024-08-11

This was the first formal release of fuse-ufs.

[unreleased]: https://github.com/realchonk/fuse-ufs/compare/0.6.0...HEAD
[0.6.0]: https://github.com/realchonk/fuse-ufs/compare/0.6.0...0.6.0
[0.6.0]: https://github.com/realchonk/fuse-ufs/compare/0.5.0...0.6.0
[0.5.0]: https://github.com/realchonk/fuse-ufs/compare/0.4.4...0.5.0
[0.4.4]: https://github.com/realchonk/fuse-ufs/compare/0.4.3...0.4.4
[0.4.3]: https://github.com/realchonk/fuse-ufs/compare/0.4.2...0.4.3
[0.4.2]: https://github.com/realchonk/fuse-ufs/compare/0.4.1...0.4.2
[0.4.1]: https://github.com/realchonk/fuse-ufs/compare/0.4.0...0.4.1
[0.4.0]: https://github.com/realchonk/fuse-ufs/compare/0.3.0...0.4.0
[0.3.0]: https://github.com/realchonk/fuse-ufs/compare/0.2.1...0.3.0
[0.2.1]: https://github.com/realchonk/fuse-ufs/compare/0.2.0...0.2.1
[0.2.0]: https://github.com/realchonk/fuse-ufs/releases/tag/0.2.0
