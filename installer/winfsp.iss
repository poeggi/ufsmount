; The WinFsp release the setup installs when WinFsp is missing or too old.
; One source for the setup, CI and a Rust test (MIN_WINFSP in src/winfs.rs).
#define WinFspUrl "https://github.com/winfsp/winfsp/releases/download/v2.2B5/winfsp-2.2.26286.msi"
#define WinFspMsi "winfsp-2.2.26286.msi"
#define WinFspSha256 "293ffb9bcc0b625c85079fe85fa77f8fe98170aba51a02ac9c83d58f6c3b0e2f"
#define WinFspMinMajor 2
#define WinFspMinMinor 2
#define WinFspMinBuild 26286
