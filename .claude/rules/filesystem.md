# winfsp-ufs: file system behaviour

- Read-only. Every writing WinFsp callback returns `STATUS_MEDIA_WRITE_PROTECTED`.
  `read_only_volume` alone enforces nothing.
- A dirty file system (soft updates, SU+J) is read as is, with a warning. No journal replay.
- UFS is found by the superblock magic at +65536, not by the label fstype (fstype 0 occurs).
  NetBSD's FFSv2ea magic counts as UFS2. Clean means `fs_clean` 1; NetBSD keeps 2 while mounted.
- Layouts: the module docs of `partition.rs` (tables) and `ufs.rs` (BSD labels) list what was checked
  and what is not read. A new layout gets a test in `products/ufsmount/tests/layouts.rs`.
- Tables are read with the device's sector size, then 512 and 4096. The first size that finds a file system wins.
- A BSD label in a slice with c at 0 may count from the slice or the disk start. A superblock decides;
  else FreeBSD's reading, or for OpenBSD version 1 labels the absolute one.
  A label whose sector size (512-4096) differs from the one tables are read with is skipped.
- A UFS2 superblock counts only when cylinder group 0 or the group's superblock copy follows it:
  a backup copy seen through a wrong offset does not.
- Device reads are sector aligned. Failed reads are retried per sector and never cached as data.
  A bad sector fails only the file that uses it, never the whole listing or copy.
- A failed read first checks that the disk is still there. Only media errors of a present disk are
  remembered as bad sectors; ERROR_IO_DEVICE (1117, USB resets) never is. After any failed chunk read
  on a present disk, that chunk is read sector by sector.
- A gone disk stops its mount. `ufsmount` then exits with 3, after 30 s at the latest if the unmount hangs.
- Errors carry the Win32 code (`DeviceError`). Log errno values by name: on Windows errno 5 prints as "Access is denied".
- Names: valid UTF-8 as is. Characters Windows forbids, and `/`, map to U+F000 + char. Invalid bytes map to U+F100 + byte,
  or decode as Latin-1 with `--charset latin1`. The mapping is never reversed.
- Lookups are case-sensitive. A case-insensitive match is used only when it is unique.
- Symlinks are Windows symlinks. Device nodes, fifos, sockets and whiteouts are hidden.
- `persistent_acls` stays off: copies take the destination's ACLs.
- WinFsp allows one service per process. `winfs::Mount` sits on `FileSystemHost`; its `stop()` is idempotent
  and closes the disk.
- Panics never cross the C boundary.
