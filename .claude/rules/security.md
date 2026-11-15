# winfsp-ufs: security rules

Raw disk reads and global drive letters need admin rights. The tray and the launcher mode run elevated.

## Elevated code
- No environment variables for paths. Use the known-folder API, `GetSystemDirectoryW`, `GetWindowsDirectoryW`.
- No `RUST_LOG`, no ShellExecute. Child processes start by full path.
- "Open" starts `explorer.exe` with the shell's non-elevated token.
- ProgramData folders are created together with their security descriptor (`CreateDirectoryW` with
  `SECURITY_ATTRIBUTES`). Reparse points are refused. An existing folder that is not admin-only is replaced.
- The task-started tray (`--from-task`) refuses an install folder that is not admin-only.
- `--source <image>` (tests) needs an elevated caller and is refused with `--from-task`.

## Scheduled task `ufsmount-tray`
- Principal: the user of the interactive session (WTS query), InteractiveToken, HighestAvailable.
- Action: the literal exe path with `--from-task`. Callers cannot add arguments.
- Created and removed by `ufsmount-tray --install-task [--logon]` and `--remove-task`,
  through `schtasks.exe` by its full System32 path.

## Single instance
- `Global\ufsmount-tray` mutex: any authenticated user may wait on it.
- Quit event: SYSTEM and Administrators only. Show event: any authenticated user.
- Access denied when opening the mutex counts as "running".

## Mounts and the WinFsp launcher
- File security: read for SYSTEM, Administrators and the session user.
  `--sddl` is validated: owner and group present, at most 1024 bytes.
- Launcher instance names must match `^\\ufs\\PhysicalDrive[0-9]{1,3}(s[1-4]([a-t])?|p[0-9]{1,3})?\\?$`.
  A `-U` user that does not resolve stops the start. Never fall back to a wider security descriptor.
- The launcher registration keeps WinFsp's default Security (SYSTEM and Administrators).
  Never grant Everyone: that exposes every raw disk.
- `register` refuses unless the exe folder is writable only by admins and SYSTEM.
- A disk is never reopened by its number. A disk that is gone stops its mount.
