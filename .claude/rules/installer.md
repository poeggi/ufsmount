# winfsp-ufs: setup and tray

## Setup (`installer/ufsmount.iss`, Inno Setup 7.1.1)
- Build: `ISCC /DAppVersion=<version> installer\ufsmount.iss`. Run ISCC from PowerShell or cmd:
  Git Bash rewrites `/D` arguments.
- Read the Inno Setup help before changing directives. Do not try styles or options by trial.
- WinFsp is installed when missing or older. The MSI is copied from next to the setup, or downloaded, into `{tmp}`,
  checked by SHA-256 and run there. It is ready before the tray is stopped. WinFsp stays on uninstall.
- An unticked PATH or launcher task on an upgrade removes that PATH entry or registration.
- On an upgrade the launcher task is preselected when registered, and the logon task as
  `<ProgramData>\ufsmount\start-at-sign-in` says ("1" or "0", written with each task).
  The tray's "Start at sign-in" changes it. `/TASKS=` or `/MERGETASKS=` win over both.
- An older version over a newer one asks first. Silent installs need `/ALLOWDOWNGRADE` for it.
- PATH is edited by `ufsmount-tray --add-to-path` and `--remove-from-path`, which keep the registry type.
  Pascal script cannot read that type.
- Inno Setup 7.1.1 clips checkboxes in lists on the left at high DPI, in every wizard style.
  `InitializeWizard` gives every wizard list (components, tasks, run) more room through `Offset`.
  A new checkbox list must get the same. Fixed upstream in issrc commit 5b4c39b (after 7.1.1);
  update the pin in `ci.yml` once a release has it.
- `logos/wizard/*.png` are rendered from `logos/logo.svg` at the sizes the help lists per DPI.
- `logos/ufsmount.ico` (exe, tray and setup icon): 16-64 px from `logos/logo-small.svg`, a bold variant
  that stays readable in the tray; 256 px from `logos/logo.svg`. Both have a transparent background
  and a white drive with black "U:".
- The setup is unsigned: SmartScreen warns, Smart App Control blocks it.

## Tray (`src/tray`)
- The UI thread runs the window, icon and menu. One worker thread does all disk work.
- Calls that can dispatch messages (Shell_NotifyIconW, menus, message boxes) run outside `with_app`.
- Mounts every UFS2 file system found, at most 8. Letters: U:, then T: down to D:,
  skipping letters in use and the session user's network mappings (`HKEY_USERS\<SID>\Network`).
- Each worker command runs under `catch_unwind`; a panic becomes an "Internal error" note.
- A disk arrival forgets which removable-media disks (card readers) held no UFS, so a swapped card is
  probed again. Fixed disks are not probed again: that would wake a sleeping disk.
- A mount the user unmounted stays unmounted until the disk is plugged in again or "Rescan" is clicked.
- "Start at sign-in" is shown when `start-at-sign-in` can be read; a task changed by hand
  shows wrong until the next toggle or setup run.
- About is a TaskDialog (Common Controls 6 via the manifest; comctl32.dll is delay-loaded, so
  other targets start without it). Versions come from Cargo.lock and rustc through build.rs.
  The footer icons are drawn at run time for the dialog's DPI.
- The icon is loaded for the DPI of the taskbar's monitor and loaded again on DPI and display changes.
