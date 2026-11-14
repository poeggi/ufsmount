// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() {
    // winfsp-x64.dll is loaded at runtime by full path from the WinFsp install
    // directory, so it must not be resolved at process start.
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg-bins=/DELAYLOAD:winfsp-x64.dll");
    // Static imports are searched in System32 only, never in the exe folder.
    println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/ufsmount.ico");

    // The logo as the exe icon, plus version info from Cargo.toml.
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/ufsmount.ico");
    res.compile().expect("compile the Windows resources");
}
