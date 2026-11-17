# ufsmount: setup and tray

## Setup (Inno Setup 7.1.1)
- `products/ufsmount/installer.iss` holds the product's defines (name, AppGuid, repo, launcher class) and
  includes `installer/common.iss`, which is the same for every product. Relative paths in it are relative
  to the product's script. The AppGuid of a product never changes: upgrades find the installed version by it.
- Build: `ISCC /DAppVersion=<version> products\ufsmount\installer.iss`. Run ISCC from PowerShell or cmd:
  Git Bash rewrites `/D` arguments.
- Read the Inno Setup help before changing directives. Do not try styles or options by trial.
- WinFsp is installed when missing or older. The MSI is copied from next to the setup, or downloaded, into `{tmp}`,
  checked by SHA-256 and run there. It is ready before the tray is stopped. WinFsp stays on uninstall.
- Before a WinFsp update, WinFsp drives still mounted after the tray stopped (other products, command line
  mounts) are listed by WinFsp's `fsptool lsvol`, and the setup asks first. Silent installs with
  `/SUPPRESSMSGBOXES` go on. Cancel starts the stopped tray again.
- An unticked PATH or launcher task on an upgrade removes that PATH entry or registration.
- On an upgrade the launcher task is preselected when registered, and the logon task as
  `<ProgramData>\ufsmount\start-at-sign-in` says ("1" or "0", written with each task).
  The tray's "Start at sign-in" changes it. Silent installs pass the tasks page too (CI checks it).
  `/TASKS=` or `/MERGETASKS=` win over both.
- No Ready to Install page (`DisableReadyPage`): the tasks page is the last one, and `CurPageChanged`
  sets its button to Install.
- An older version over a newer one asks first. Silent installs need `/ALLOWDOWNGRADE` for it.
- PATH is edited by `ufsmount-tray --add-to-path` and `--remove-from-path`, which keep the registry type.
  Pascal script cannot read that type.
- Inno Setup 7.1.1 clips checkboxes in lists on the left at high DPI, in every wizard style.
  `InitializeWizard` gives every wizard list (components, tasks, run) more room through `Offset`.
  A new checkbox list must get the same. Fixed upstream in issrc commit 5b4c39b (after 7.1.1);
  update the pin in `ci.yml` once a release has it.
- `products/ufsmount/logos/wizard/*.png` are rendered from `logos/logo.svg` at the sizes the help lists per DPI:
  small ones fill the image, large ones hold a square of 70% of the width, centred at 38% of the height.
- `products/ufsmount/logos/ufsmount.ico` (exe, tray and setup icon): 16-64 px from `logos/logo-small.svg`,
  a bold variant that stays readable in the tray; 256 px from `logos/logo.svg`. Both have a transparent
  background and a white drive with black "U:".
- Icons fill their canvas like other apps' icons: `logo.svg`'s viewBox ends at the drawing, and 48 and 64 px
  use `logo-small.svg` with viewBox `1.5 0.6 30.5 30.5`. 16-40 px keep the 32-unit grid, so they stay sharp.
- The setup is unsigned: SmartScreen warns, Smart App Control blocks it.

## Tray (`crates/win-mountkit/src/tray`)
- Names and texts come from the product: `<product>-tray` task and window class, `Global\<product>-tray`
  objects, `<ProgramData>\<product>`, `SOFTWARE\<product>`, the menu's "No <fs> disks".
- The UI thread runs the window, icon and menu. One worker thread does all disk work.
- Calls that can dispatch messages (Shell_NotifyIconW, menus, message boxes) run outside `with_app`.
- Scans the `PhysicalDriveN` and `CdRomN` names QueryDosDevice lists, any number; PhysicalDrive0-31 if
  that fails. Drive arrivals come through the disk and CD-ROM interface classes, discs as volume arrivals
  with DBTF_MEDIA (a drive without a letter gets none: Rescan). A disc without UFS, or with read errors,
  is probed again only on such a media arrival or Rescan; a disc not ready yet is retried.
  Optical drives are untested on hardware.
- Mounts every file system found, at most 8. Letters: the product's first letter (ufsmount: U:), the ones
  below it down to D:, then the ones above it up to U:. So ufsmount (U:) and a product starting at M: keep to
  their own letters (8 mounts each) unless other drives take the ones between.
  Skipped: letters in use and the session user's network mappings (`HKEY_USERS\<SID>\Network`).
  The letter menus list U: down to D: for every product.
- Each worker command runs under `catch_unwind`; a panic becomes an "Internal error" note.
- A disk arrival forgets which removable-media disks (card readers) held no UFS, so a swapped card is
  probed again. Fixed disks are not probed again: that would wake a sleeping disk.
- "Mount automatically" (default on, `<ProgramData>\ufsmount\auto-mount`): when off, found file systems
  are listed under "Not mounted". A file system the user unmounted is listed there too and is not
  mounted automatically until "Rescan" or a new plug-in. Listed ones hold no disk handle open.
  Automatic scans skip a fixed disk with listed file systems; a removable one is probed again.
- A file system the backend marks `Found.on_request` (a container's helper volumes) is always listed under
  "Not mounted" and mounts only from the menu; after a Rescan or a refused eject it is listed again.
  The command line and the launcher take it only when its part is named. ufsmount marks none.
- A letter the user picks ("Mount on", "Mount as", "Drive letter") is remembered per file system id
  (UFS: `<id0>-<id1>` in hex) in `<ProgramData>\<product>\letters`; automatic mounts try it first when it is free.
- Each mounted disk's handle is registered for removal requests (DBT_DEVTYP_HANDLE). On
  DBT_DEVICEQUERYREMOVE (Eject) the tray unmounts every mount of that disk and closes it before it
  answers, else the open handle blocks the eject. After 10 s it refuses and scans the disk again
  (mounted, or listed when "Mount automatically" is off).
  If another program blocks the eject, "Rescan" mounts the disk again.
- The menu's Quit unmounts everything. While a drive is mounted it asks first (TaskDialog, OK and Cancel,
  Cancel the default). The quit event (the setup) and session end never ask. While the question is up,
  a click on the icon brings it to the front instead of opening the menu.
- "Start at sign-in" is shown when `start-at-sign-in` can be read; a task changed by hand
  shows wrong until the next toggle or setup run.
- About is a TaskDialog (Common Controls 6 via the manifest; comctl32.dll is delay-loaded, so
  other targets start without it). Versions come from rustc and the product's Cargo.lock (`Product.winfsp_rs`):
  a git dependency on win-mountkit cannot see the consumer's lock.
  The footer icons are drawn at run time for the dialog's DPI.
- The icon is loaded for the DPI of the taskbar's monitor and loaded again on DPI and display changes.
