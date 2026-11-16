// SPDX-License-Identifier: AGPL-3.0-or-later
//! ufsmount: FreeBSD UFS2 disks read-only on Windows. This crate holds what is
//! UFS or ufsmount; the rest is the shared winfsp-mount.

mod ufs;

pub use winfsp_mount;
use winfsp_mount::Product;

pub static PRODUCT: Product = Product {
    id: "ufsmount",
    fs: "UFS",
    fs_variant: "UFS2",
    launcher_class: "ufs",
    first_letter: 'U',
    repo: "poeggi/winfsp-ufs",
    version: env!("CARGO_PKG_VERSION"),
    description: env!("CARGO_PKG_DESCRIPTION"),
    about: "Mounts FreeBSD UFS2 disks read-only.",
    built_using: concat!(
        "- rufs ",
        env!("UFSMOUNT_RUFS"),
        ", from fuse-ufs by Benjamin Stuerz and contributors"
    ),
    credits: "  rufs / fuse-ufs, Copyright (c) Benjamin Stuerz, Alan Somers, Davids Paskevics
    and contributors (BSD-2-Clause)
    https://github.com/realchonk/fuse-ufs",
    license: "AGPL-3.0-or-later",
    quiet_log: "rufs",
    gpt_types: &[(ufs::GPT_FREEBSD_UFS, "freebsd-ufs")],
    backend: &ufs::Ufs,
};

/// Set this product for the shared code. Every exe and test calls it first.
pub fn init() {
    winfsp_mount::set_product(&PRODUCT);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The setup's defines match the product.
    #[test]
    fn setup_defines() {
        let iss = include_str!("../installer.iss");
        let define = |name: &str| {
            let prefix = format!("#define {name} \"");
            iss.lines()
                .find_map(|l| l.strip_prefix(prefix.as_str()))
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or_else(|| panic!("{name} is not defined"))
        };
        assert_eq!(define("AppName"), PRODUCT.id);
        assert_eq!(define("AppRepo"), PRODUCT.repo);
        assert_eq!(define("LauncherClass"), PRODUCT.launcher_class);
        // The update check downloads the setup under this name.
        let common = include_str!("../../../installer/common.iss");
        assert!(common.contains("OutputBaseFilename={#AppName}-{#AppVersion}-setup"));
    }

    #[test]
    fn versions_come_from_the_build() {
        let lock = include_str!("../../../Cargo.lock").replace("\r\n", "\n");
        let rufs = env!("UFSMOUNT_RUFS");
        assert!(lock.contains(&format!("name = \"rufs\"\nversion = \"{rufs}\"\n")));
        assert!(PRODUCT.built_using.contains(rufs));
    }
}
