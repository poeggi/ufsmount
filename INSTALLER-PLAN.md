# Installer and tray app plan

Reviewed by two Fable reviewers (installer/CI; tray/elevation/security). Their findings are folded in.

## Goal

- A normal Windows install: setup wizard, entry in Settings > Apps, clean uninstall.
- Start menu entry "ufsmount" starts the tray app.
- The tray app finds UFS disks, mounts them, and shows its state in the system tray.
- Tray menu: open a drive, unmount it, rescan, quit. Quit unmounts everything.
- The installer, both exes and the tray use the logo.

## Parts

1. `ufsmount.exe`: the CLI as today.
2. `ufsmount-tray.exe`: new, second binary of the same crate, GUI subsystem, manifest asInvoker + PerMonitorV2.
3. `installer/ufsmount.iss`: Inno Setup 7.1.1 script. `installer/winfsp.iss` holds the WinFsp URL, SHA-256
   and minimum version: the one source for the setup, CI and a Rust test against `MIN_WINFSP`.

## Elevation

Reading a physical disk and a global drive letter need admin rights.

- Scheduled task `ufsmount-tray` (root folder):
  - Principal: the user of the interactive session (WTS query, not the account that elevated setup),
    InteractiveToken, RunLevel Highest. Its security descriptor lets that user run it.
  - Action: the literal path `C:\Program Files\ufsmount\ufsmount-tray.exe --from-task`. No variables, no arguments from callers.
  - Settings: no time limit, runs on battery, priority 4, IgnoreNew, start on demand. Logon trigger only if chosen.
  - Created and deleted by the tray exe (`--install-task [--logon]`, `--remove-task`) through `schtasks.exe /xml`,
    by full System32 path. The XML file lives in the admin-only ProgramData folder.
- Starting (no arguments, not elevated):
  1. A tray already runs: signal it (it shows a balloon), exit.
  2. The task exists: run it, wait up to 5 s for the tray to come up.
  3. Otherwise, or on timeout: relaunch our own exe with "runas" (UAC). This also serves standard users.
- `--from-task` and not elevated (standard user): exit at once, no relaunch. The starter then falls back to UAC.
- The task-started tray refuses to run from an install folder that is not admin-only (`check_admin_only`).
  A tray started elevated by hand (UAC prompt, admin prompt) had the user's consent.
- Rules for the elevated process: no environment variables for paths (known-folder API, `GetSystemDirectory`),
  no `RUST_LOG`, no ShellExecute. "Open" starts `explorer.exe` by full path with the shell's (non-elevated) token.
- File security on the mounts: read for SYSTEM, Administrators and the session user (not the process user).
- `--source <image>` (tests) works only without `--from-task`, so it needs an elevated caller.

## Tray app

- Window: hidden top-level window (not message-only, so it gets TaskbarCreated, session end and device broadcasts).
  `ChangeWindowMessageFilterEx` for TaskbarCreated. Icon by hWnd + uID. Re-add on TaskbarCreated; retry NIM_ADD on a timer.
- Single instance, machine-wide:
  - `Global\ufsmount-tray` mutex: running. Any user may wait on it.
  - `Global\ufsmount-tray-quit` event: only SYSTEM and Administrators may set it.
  - `Global\ufsmount-tray-show` event: any authenticated user may set it.
  - Access denied when opening counts as "running". Squatting a name is a denial of service only.
- `--quit`: set the quit event, wait up to 15 s until the mutex is gone. Exit code 0 when no tray runs or it quit.
- Threads: the UI thread runs the message loop, icon and menu only. One worker thread does all disk I/O,
  probing, mounting and unmounting. Commands go over a channel; the worker posts WM_APP when the state changes.
  Probe and mount run under `catch_unwind`: one bad disk does not end the other mounts.
- Disks: scan `PhysicalDrive0..31` on start and on "Rescan". `RegisterDeviceNotification` (disk interface) triggers
  a scan of new disks after a 1.5 s debounce. Only disks not mounted yet are probed. Empty card readers are logged quietly.
- Every UFS2 file system found is mounted (a FreeBSD disk may have several). Cap: 8 mounts.
- A mount the user unmounted stays unmounted until the disk is re-plugged or "Rescan" is clicked.
- Drive letter: U: if free, else the highest free letter from T: down to D:. Skip letters in use and the user's
  `HKCU\Network` mappings. If the mount fails, try the next letter.
- Unplug: each mount has a watcher thread that checks the disk every 2 s (IOCTL, no spin-up) and the device's
  "gone" flag. Device removal broadcasts trigger a check at once. A gone mount is stopped by the worker,
  identified by a unique mount id, never by disk number.
- Menu: one line per mount ("U:  PhysicalDrive1 s1a  465.8 GiB") with "Open" and "Unmount";
  "Rescan disks", "About" (message box, no links), "Quit". Tooltip: the mounted letters (max 127 chars).
  Balloons: first start ("ufsmount runs in the notification area"), unplug, errors. Errors also show in the tooltip.
- Session end: WM_QUERYENDSESSION returns TRUE; WM_ENDSESSION unmounts all (up to 4 s).
- Log: `<ProgramData>\ufsmount\tray\tray.log` (known-folder path). Folder: SYSTEM and Administrators full,
  Users read; created with the reparse-point check. Own file logger, rotation at 1 MiB.

### Refactor in the crate

- WinFsp allows one service per process. New `winfs::Mount` sits directly on `FileSystemHost`:
  `Mount::start(...)` (build, mount, start) and an idempotent `stop()` that unmounts and drops the host,
  so the disk handle closes. `serve()` (console and launcher) wraps one `Mount` in the WinFsp service as now.
- `load_winfsp()` runs once per process.
- `admin_only_dir` moves from launcher.rs to security.rs and gains a "Users read" variant.
- `src/tray/` holds the tray; `src/bin/ufsmount-tray.rs` is a thin `main` that calls `harden_dll_search()` first.
- `build.rs`: resources per binary (icon, version info; manifest for the tray).

## Installer

- Inno Setup 7.1.1. `[Setup]`: fixed `AppId`, `SetupArchitecture=x64`, `ArchitecturesAllowed=x64os`,
  `ArchitecturesInstallIn64BitMode=x64os`, `PrivilegesRequired=admin`, `MinVersion=10.0`, `DisableDirPage=yes`
  (always `C:\Program Files\ufsmount`), `DisableProgramGroupPage=yes`, `UsePreviousTasks=yes`, `SetupLogging=yes`,
  `CloseApplications=no`, `DirExistsWarning=no`, icons from `assets/ufsmount.ico`, `UninstallDisplayIcon` = the tray exe.
- Files: both exes, `LICENSE`, `THIRD-PARTY-NOTICES.txt`. One Start menu link "ufsmount" (the tray).
- Options: start at logon (default off), add to PATH (default on), WinFsp launcher for `net use`
  (default off; preselected when a registration exists).
- WinFsp: read `InstallDir` from the 32-bit registry view, compare the file version of `bin\winfsp-x64.dll`
  with 2.2.26286 numerically. Newer is kept. If missing or older: use `winfsp-2.2.26286.msi` next to the setup
  exe if its SHA-256 matches, else download it (`DownloadTemporaryFile` with SHA-256). Run
  `msiexec /i ... /norestart /l*v <log>` (`/passive`, or `/qn` when silent). Exit 0 ok, 3010 ok with restart
  request, 1618 retry message, else stop with the code and log path. Check the version again afterwards.
  Download failure or hash mismatch: stop, show the WinFsp download page. WinFsp stays on uninstall.
- Before install (`PrepareToInstall`) and before uninstall: `ufsmount-tray --quit`, wait, then
  `taskkill /F` on `ufsmount-tray.exe` and `ufsmount.exe` (read-only; WinFsp drops the mount). Then the MSI if needed.
- After install: `ufsmount-tray --install-task [--logon]` (creates the task and the admin-only ProgramData folders),
  `ufsmount register` if chosen. The last page offers "Start ufsmount" (runs as the original user).
- PATH: Pascal code on the system Path. Keep REG_EXPAND_SZ, no duplicates (system or user Path), write only after a
  successful read. Remember that we added it; remove on uninstall only then. `ChangesEnvironment=yes`.
- Uninstall: quit and kill as above, `ufsmount-tray --remove-task` (also removes `<ProgramData>\ufsmount`, refusing
  reparse points), `ufsmount unregister` (exit code ignored), PATH, files, link.
- A manual install in the same folder is taken over; its launcher registration is kept.
- `/NORESTART` for silent installs (documented).

## CI

- Build job: install Inno Setup 7.1.1 from its GitHub release, check our pinned SHA-256 and the Authenticode
  signature, run ISCC with `/DAppVersion=<Cargo version>`. Artifacts: `ufsmount.exe`, `ufsmount-tray.exe`, setup.
- Smoke job on a fresh runner (no WinFsp): silent install with `/NORESTART` (exercises the WinFsp download),
  check files, link, uninstall entry, task and its settings (not run), PATH; run the tray with `--source` on a test
  image, read a file through U:, `--quit`; silent uninstall; check that nothing is left and PATH is unchanged
  (value and type).
- Release job (tags): setup exe and `ufsmount.exe`, one `SHA256SUMS`. The tag must match the Cargo version.

## Unsigned

- SmartScreen: "More info" > "Run anyway". UAC shows "Unknown publisher".
- Smart App Control (Windows 11) blocks unsigned installers outright.
- README tells this and how to check the SHA-256. Signing (e.g. SignPath for FOSS) comes later.

## Tests

- Unit: drive letter choice, task XML, menu text, PATH logic where it lives in Rust, MIN_WINFSP vs `winfsp.iss`.
- Mount: `stop()` releases the device (Arc count back to 1). Existing 58 tests keep passing.
- Local, elevated: install, start from a non-elevated process through the task, real disk mount, open, unmount,
  rescan, quit, uninstall, nothing left.

## Out of scope for v1

- Write support, settings dialog, code signing, MSI, per-user tasks for further accounts (they get UAC).
