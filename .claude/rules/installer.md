# winfsp-ufs: setup and tray

## Setup (`installer/ufsmount.iss`, Inno Setup 7.1.1)
- Build: `ISCC /DAppVersion=<version> installer\ufsmount.iss`. Run ISCC from PowerShell or cmd:
  Git Bash rewrites `/D` arguments.
- Read the Inno Setup help before changing directives. Do not try styles or options by trial.
- WinFsp is installed when missing or older. The MSI download is checked by SHA-256. WinFsp stays on uninstall.
- An older version over a newer one asks first. Silent installs need `/ALLOWDOWNGRADE` for it.
- PATH is edited by `ufsmount-tray --add-to-path` and `--remove-from-path`, which keep the registry type.
  Pascal script cannot read that type.
- Inno Setup 7.1.1 clips checkboxes in lists on the left at high DPI, in every wizard style.
  `InitializeWizard` gives every wizard list (components, tasks, run) more room through `Offset`.
  A new checkbox list must get the same. Fixed upstream in issrc commit 5b4c39b (after 7.1.1);
  update the pin in `ci.yml` once a release has it.
- `logos/wizard/*.png` are rendered from `logos/logo.svg` at the sizes the help lists per DPI.
- The setup is unsigned: SmartScreen warns, Smart App Control blocks it.

## Tray (`src/tray`)
- The UI thread runs the window, icon and menu. One worker thread does all disk work.
- Calls that can dispatch messages (Shell_NotifyIconW, menus, message boxes) run outside `with_app`.
- Mounts every UFS2 file system found, at most 8. Letters: U:, then T: down to D:,
  skipping letters in use and `HKCU\Network` mappings.
- A mount the user unmounted stays unmounted until the disk is plugged in again or "Rescan" is clicked.
- The icon is loaded for the DPI of the taskbar's monitor and loaded again on DPI and display changes.
