// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() {
    // Link settings for this crate's own tests. Each product's build.rs sets
    // them for its exes too: they do not carry over from here.
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
    println!("cargo:rustc-link-arg=/DELAYLOAD:comctl32.dll");
    // LNK4199: a target that ends up with no imports from a delay-loaded DLL.
    println!("cargo:rustc-link-arg=/IGNORE:4199");
    println!("cargo:rerun-if-changed=build.rs");

    // The Rust version, for the About box.
    let rustc = std::env::var("RUSTC").expect("RUSTC");
    let out = std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .expect("run rustc --version");
    let rust = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)
        .expect("rustc version")
        .to_string();
    println!("cargo:rustc-env=RUST_VERSION={rust}");
}
