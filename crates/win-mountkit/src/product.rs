// SPDX-License-Identifier: AGPL-3.0-or-later
//! What makes a product: its names, texts and file system code. Everything
//! else in this crate is the same for every product.

use std::sync::OnceLock;

use crate::fs::Backend;

pub struct Product {
    /// Exe, task, mutex, folder and registry names, and the name users see.
    /// Lower-case letters, digits and `-`.
    pub id: &'static str,
    /// The file system as users know it.
    pub fs: &'static str,
    /// The exact variant that is read, for command line messages.
    pub fs_variant: &'static str,
    /// The WinFsp launcher class: `net use X: \\<class>\PhysicalDriveN`.
    pub launcher_class: &'static str,
    /// The drive letter mounts try first (D to U).
    pub first_letter: char,
    /// "owner/name" on GitHub: source link, update check and setup downloads.
    pub repo: &'static str,
    pub version: &'static str,
    /// One line for `--help`.
    pub description: &'static str,
    /// The first line of the About box.
    pub about: &'static str,
    /// About box lines for the file system code, each starting with "- ".
    pub built_using: &'static str,
    /// Credit lines for the file system code in `--help`, indented by two.
    pub credits: &'static str,
    pub license: &'static str,
    /// Log target of the file system code; the tray logs it from warnings on.
    pub quiet_log: &'static str,
    /// GPT partition types shown by name: (lower-case GUID, name).
    pub gpt_types: &'static [(&'static str, &'static str)],
    pub backend: &'static dyn Backend,
}

static PRODUCT: OnceLock<&'static Product> = OnceLock::new();

/// A leading `-` would read as a switch on a command line.
fn valid_name(s: &str) -> bool {
    let ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    (1..=32).contains(&s.len())
        && s.bytes().next().is_some_and(ok)
        && s.bytes().all(|b| ok(b) || b == b'-')
}

/// Set the product. Each exe and test does this first; later calls do nothing.
/// The names reach command lines and registry paths unquoted, so they are checked.
pub fn set_product(p: &'static Product) {
    assert!(valid_name(p.id) && valid_name(p.launcher_class));
    assert!(('D'..='U').contains(&p.first_letter));
    let _ = PRODUCT.set(p);
}

#[cfg(not(test))]
pub fn product() -> &'static Product {
    PRODUCT.get().expect("set_product runs first")
}

#[cfg(test)]
pub fn product() -> &'static Product {
    PRODUCT.get_or_init(|| &tests::DEMO)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::fs::{Found, FsKind};
    use crate::partition::Candidate;
    use std::sync::Arc;

    struct NoFs;

    impl Backend for NoFs {
        fn probe(&self, _: &Arc<crate::device::Device>, _: &Candidate) -> Vec<FsKind> {
            Vec::new()
        }

        fn open(
            &self,
            _: Arc<crate::device::Device>,
            _: &Candidate,
            _: &Found,
        ) -> anyhow::Result<Box<dyn crate::fs::Fs>> {
            anyhow::bail!("no file system")
        }
    }

    /// The product of this crate's own tests.
    pub static DEMO: Product = Product {
        id: "demo",
        fs: "DEMO",
        fs_variant: "DEMO2",
        launcher_class: "demo",
        first_letter: 'U',
        repo: "example/demo",
        version: "1.2.3",
        description: "Demo",
        about: "Mounts demo disks.",
        built_using: "- demo 1.0",
        credits: "  demo",
        license: "AGPL-3.0-or-later",
        quiet_log: "demofs",
        gpt_types: &[],
        backend: &NoFs,
    };

    #[test]
    fn names() {
        assert!(valid_name("demo-2"));
        for bad in ["", "-x", "Demo", "a b", "a\\b", "a%b", &"a".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
