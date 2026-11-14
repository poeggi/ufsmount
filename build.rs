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
</assembly>
"#;

fn main() {
    // winfsp-x64.dll is loaded at runtime by full path from the WinFsp install
    // directory, so it must not be resolved at process start.
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg-bins=/DELAYLOAD:winfsp-x64.dll");
    println!("cargo:rustc-link-arg-tests=/DELAYLOAD:winfsp-x64.dll");
    // Static imports are searched in System32 only, never in the exe folder.
    println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/ufsmount.ico");

    // The logo as the icon (resource id 1, also used by the tray), plus version info.
    let mut cli = winresource::WindowsResource::new();
    cli.set_icon_with_id("assets/ufsmount.ico", "1").set(
        "FileDescription",
        "ufsmount - mount FreeBSD UFS2 disks read-only",
    );
    cli.compile_for("ufsmount")
        .expect("compile the resources of ufsmount");

    let mut tray = winresource::WindowsResource::new();
    tray.set_icon_with_id("assets/ufsmount.ico", "1")
        .set("FileDescription", "ufsmount tray")
        .set_manifest(TRAY_MANIFEST);
    tray.compile_for("ufsmount-tray")
        .expect("compile the resources of ufsmount-tray");
}
