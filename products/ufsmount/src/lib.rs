// SPDX-License-Identifier: AGPL-3.0-or-later
//! ufsmount: FreeBSD UFS2 disks read-only on Windows. This crate holds what is
//! UFS or ufsmount; the rest is the shared win-mountkit.

mod ufs;

pub use win_mountkit;
use win_mountkit::Product;

pub static PRODUCT: Product = Product {
    id: "ufsmount",
    fs: "UFS",
    fs_variant: "UFS2",
    launcher_class: "ufs",
    first_letter: 'U',
    repo: "poeggi/ufsmount",
    version: env!("CARGO_PKG_VERSION"),
    winfsp_rs: env!("UFSMOUNT_WINFSP_RS"),
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
    gpt_types: &[
        ("516e7cb6-6ecf-11d6-8ff8-00022d09712b", "freebsd-ufs"),
        ("516e7cb4-6ecf-11d6-8ff8-00022d09712b", "freebsd, BSD label"),
        ("49f48d5a-b10e-11dc-b99b-0019d1879648", "netbsd-ffs"),
        ("824cc7a0-36a8-11e3-890a-952519ad3f61", "openbsd, BSD label"),
        ("55465300-0000-11aa-aa11-00306543ecac", "apple-ufs"),
    ],
    backend: &ufs::Ufs,
};

/// Set this product for the shared code. Every exe and test calls it first.
pub fn init() {
    win_mountkit::set_product(&PRODUCT);
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
        let winfsp = PRODUCT.winfsp_rs;
        assert!(lock.contains(&format!("name = \"winfsp\"\nversion = \"{winfsp}\"\n")));
    }
}
