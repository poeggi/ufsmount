// SPDX-License-Identifier: AGPL-3.0-or-later

const TRAY_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#;

/// The version of `name` in the workspace's Cargo.lock.
fn locked_version(lock: &str, name: &str) -> String {
    let key = format!("name = \"{name}\"");
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line == key
            && let Some(v) = lines.next().and_then(|l| l.strip_prefix("version = \""))
        {
            return v.trim_end_matches('"').to_string();
        }
    }
    panic!("{name} is not in Cargo.lock");
}

fn main() {
    // winfsp-x64.dll is loaded at runtime by full path from the WinFsp install
    // directory, so it must not be resolved at process start.
    println!("cargo:rustc-link-lib=dylib=delayimp");
    // All targets, the tests included.
    println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
    // TaskDialogIndirect is in Common Controls 6 only, which the tray's
    // manifest selects; other targets would fail to start on its import.
    println!("cargo:rustc-link-arg=/DELAYLOAD:comctl32.dll");
    // Static imports are searched in System32 only, never in the exe folder.
    println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=logos/ufsmount.ico");
    println!("cargo:rerun-if-changed=../../Cargo.lock");

    // The rufs version, for the About box.
    let lock = std::fs::read_to_string("../../Cargo.lock").expect("read Cargo.lock");
    let rufs = locked_version(&lock, "rufs");
    println!("cargo:rustc-env=UFSMOUNT_RUFS={rufs}");

    // The logo as the icon (resource id 1, also used by the tray), plus version info.
    let mut cli = winresource::WindowsResource::new();
    cli.set_icon_with_id("logos/ufsmount.ico", "1").set(
        "FileDescription",
        "ufsmount - mount FreeBSD UFS2 disks read-only",
    );
    cli.compile_for("ufsmount")
        .expect("compile the resources of ufsmount");

    let mut tray = winresource::WindowsResource::new();
    tray.set_icon_with_id("logos/ufsmount.ico", "1")
        .set("FileDescription", "ufsmount tray")
        .set_manifest(TRAY_MANIFEST);
    tray.compile_for("ufsmount-tray")
        .expect("compile the resources of ufsmount-tray");
}
