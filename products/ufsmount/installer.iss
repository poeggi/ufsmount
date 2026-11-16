; ufsmount setup (Inno Setup 7). Build: ISCC /DAppVersion=x.y.z products\ufsmount\installer.iss
; Expects temp\release\ufsmount.exe and temp\release\ufsmount-tray.exe.

#define AppName "ufsmount"
; Never change it: upgrades find the installed version by it.
#define AppGuid "9A3E6C51-2F4B-4E8D-9C27-5B1D3F0A8E64"
#define AppPublisher "poeggi"
#define AppRepo "poeggi/ufsmount"
#define AppComment "Mount FreeBSD UFS disks read-only"
#define LauncherClass "ufs"

#include "..\..\installer\common.iss"
