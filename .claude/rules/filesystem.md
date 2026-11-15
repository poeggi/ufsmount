# winfsp-ufs: file system behaviour

- Read-only. Every writing WinFsp callback returns `STATUS_MEDIA_WRITE_PROTECTED`.
  `read_only_volume` alone enforces nothing.
- A dirty file system (soft updates, SU+J) is read as is, with a warning. No journal replay.
- UFS is found by the superblock magic at +65536, not by the label fstype (fstype 0 occurs).
- Device reads are sector aligned. Failed reads are retried per sector and never cached as data.
  A bad sector fails only the file that uses it, never the whole listing or copy.
- Errors carry the Win32 code (`DeviceError`). Log errno values by name: on Windows errno 5 prints as "Access is denied".
- Names: valid UTF-8 as is. Characters Windows forbids map to U+F000 + char. Invalid bytes map to U+F100 + byte,
  or decode as Latin-1 with `--charset latin1`. The mapping is never reversed.
- Lookups are case-sensitive. A case-insensitive match is used only when it is unique.
- Symlinks are Windows symlinks. Device nodes, fifos, sockets and whiteouts are hidden.
- `persistent_acls` stays off: copies take the destination's ACLs.
- WinFsp allows one service per process. `winfs::Mount` sits on `FileSystemHost`; its `stop()` is idempotent
  and closes the disk.
- Panics never cross the C boundary.
