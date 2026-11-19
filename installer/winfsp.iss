; The WinFsp release the setup installs when WinFsp is missing or too old.
; One source for the setup, CI and a Rust test (MIN_WINFSP in src/winfs.rs).
#define WinFspUrl "https://github.com/winfsp/winfsp/releases/download/v2.2/winfsp-2.2.26323.msi"
#define WinFspMsi "winfsp-2.2.26323.msi"
#define WinFspSha256 "36ce01f9eedbbe6842b7fb4bedc79d7d4f71983e8aceb6877b46a07b331c7e19"
#define WinFspMinMajor 2
#define WinFspMinMinor 2
#define WinFspMinBuild 26323
