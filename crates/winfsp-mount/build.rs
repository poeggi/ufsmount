// SPDX-License-Identifier: AGPL-3.0-or-later

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
    // Link settings for this crate's own tests. Each product's build.rs sets
    // them for its exes too: they do not carry over from here.
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
    println!("cargo:rustc-link-arg=/DELAYLOAD:comctl32.dll");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../Cargo.lock");

    // The versions of winfsp-rs and Rust, for the About box.
    let lock = std::fs::read_to_string("../../Cargo.lock").expect("read Cargo.lock");
    // "0.13.2+winfsp-2.1": the bindings are for the WinFsp 2.1 API.
    let winfsp = locked_version(&lock, "winfsp");
    let winfsp = match winfsp.split_once("+winfsp-") {
        Some((v, api)) => format!("{v} (WinFsp {api} API)"),
        None => winfsp,
    };
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
    println!("cargo:rustc-env=WINFSP_RS_VERSION={winfsp}");
    println!("cargo:rustc-env=RUST_VERSION={rust}");
}
