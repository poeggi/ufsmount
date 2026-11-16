# winfsp-ufs: security rules

Raw disk reads and global drive letters need admin rights. The tray and the launcher mode run elevated.

## Elevated code
- No environment variables for paths. Use the known-folder API, `GetSystemDirectoryW`,
  `GetSystemWindowsDirectoryW`.
- The elevated tray resets `SystemRoot`, `windir`, `TEMP`, `TMP` and `PATH` from the APIs before any thread
  starts: the task gives it the user's environment. Child processes start by full path, in System32.
- No `RUST_LOG`, no ShellExecute. A failed DLL search hardening is fatal.
- "Open" starts `<Windows>\explorer.exe` in System32 as working folder, with the shell's token and the shell user's environment. It refuses
  a UAC full token (the elevated half of a split logon); with UAC off there is no lower token to use.
- ProgramData folders are created together with their security descriptor (`CreateDirectoryW` with
  `SECURITY_ATTRIBUTES`). An existing folder is opened once without following links; its checks and
  its new security use that handle. Reparse points are refused. A folder that is not admin-only is replaced.
- Install path (`--install-task`, `--from-task`, `register`): the exe lies under Program Files
  (`FOLDERID_ProgramFiles`), and every folder below Program Files and the exe are admin-only, no reparse points.
- Admin-only checks count every allow ACE type (plain, object, callback) as granting.
- About's update check: one HEAD request to github.com over WinHTTP, redirects not followed.
  Only the version number in the redirect target is read. A link opens only if the dialog shows it
  (source; setup and release page of the newer version), unelevated like "Open".
- `--source <image>` (tests) needs an elevated caller and is refused with `--from-task`.

## Scheduled task `ufsmount-tray`
- Principal: the user of the interactive session (WTS query), InteractiveToken, HighestAvailable.
- Action: the literal exe path with `--from-task`. Callers cannot add arguments.
  A path with `%` is refused: Task Scheduler expands variables in the command.
- Created and removed by `ufsmount-tray --install-task [--logon]` and `--remove-task`,
  through `schtasks.exe` by its full System32 path. The tray's "Start at sign-in" recreates it the same way.

## Single instance
- `Global\ufsmount-tray` mutex: any authenticated user may wait on it.
- Quit event: SYSTEM and Administrators only. Show event: any authenticated user.
- Access denied when opening the mutex counts as "running".

## Mounts and the WinFsp launcher
- File security: read for SYSTEM, Administrators and the session user.
  `--sddl` is validated: owner and group present, at most 1024 bytes.
- Launcher instance names must match `^\\<class>\\PhysicalDrive[0-9]{1,3}(s[1-4]([a-t])?|p[0-9]{1,3})?\\?$`,
  `<class>` being the product's launcher class (ufsmount: `ufs`).
  A `-U` user that does not resolve stops the start. Never fall back to a wider security descriptor.
- Product names (id, launcher class) are `[a-z0-9][a-z0-9-]{0,31}`, checked by `set_product`: they reach
  `schtasks` command lines and registry paths unquoted.
- The launcher registration keeps WinFsp's default Security (SYSTEM and Administrators).
  Never grant Everyone: that exposes every raw disk.
- A mounted disk is never reopened by its number. A disk that is gone stops and unmounts its mount.
  A listed (not mounted) file system is opened by its disk number again and mounts only if
  offset and UFS id match.

## Untrusted disks
- A name never leaves its folder: `/` maps like the other forbidden characters, and `cp` refuses a name
  that is empty, `.`, `..` or holds a separator.
- `cp` copies each directory once (a second visit means a loop) and at most 512 levels deep, on its own
  16 MiB stack. It holds at most 4,000,000 entries over all levels.
- A directory listing stops past 1,000,000 entries or 256 MiB of names.
  A directory larger than 1 GiB or the file system is refused (the latter by rufs).
