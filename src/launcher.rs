// SPDX-License-Identifier: AGPL-3.0-or-later
//! WinFsp launcher mode: `net use U: \\ufs\PhysicalDriveN[part]`.
//!
//! The launcher starts `ufsmount -u \ufs\Instance -m U: -U DOMAIN\user` as
//! SYSTEM. Its arguments come from whoever may talk to the launcher, so they
//! are checked strictly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WOW64_32KEY, REG_DWORD, REG_OPTION_NON_VOLATILE,
    REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyExW, RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::device::{Device, Source, strip_prefix_ci};
use crate::names::Charset;
use crate::partition;
use crate::security::{
    ADMIN_ONLY_DIR, SecurityDescriptor, account_sid, check_admin_only, is_elevated, program_data,
    secure_dir,
};
use crate::volume::Volume;
use crate::winfs::serve;

const SERVICE_KEY: PCWSTR = w!(r"SOFTWARE\WinFsp\Services\ufs");

/// A checked launcher instance: `\ufs\PhysicalDrive1s1a`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub prefix: String,
    pub drive: u32,
    pub part: Option<String>,
}

/// Accept only `\ufs\PhysicalDrive<n>` with an optional partition id
/// (`s1`..`s4`, `s1a`..`s4t`, `p<n>`), any case, an optional trailing `\`.
pub fn parse_prefix(prefix: &str) -> anyhow::Result<Instance> {
    let bad = || anyhow::anyhow!("invalid volume prefix \"{prefix}\"");
    let trimmed = prefix.strip_suffix('\\').unwrap_or(prefix);
    let rest = strip_prefix_ci(trimmed, r"\ufs\").ok_or_else(bad)?;
    let rest = strip_prefix_ci(rest, "PhysicalDrive").ok_or_else(bad)?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=3).contains(&digits) {
        return Err(bad());
    }
    let drive: u32 = rest[..digits].parse().map_err(|_| bad())?;
    let part = rest[digits..].to_ascii_lowercase();
    let b = part.as_bytes();
    let ok = match b {
        [] => true,
        [b's', n] => (b'1'..=b'4').contains(n),
        [b's', n, l] => (b'1'..=b'4').contains(n) && (b'a'..=b't').contains(l),
        [b'p', n @ ..] => (1..=3).contains(&n.len()) && n.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    if !ok {
        return Err(bad());
    }
    Ok(Instance {
        prefix: trimmed.to_string(),
        drive,
        part: (!part.is_empty()).then_some(part),
    })
}

/// Accept only a drive letter (`U:`) or nothing.
pub fn parse_mountpoint(m: &str) -> anyhow::Result<Option<String>> {
    match m.as_bytes() {
        [] => Ok(None),
        [l, b':'] if l.is_ascii_alphabetic() => Ok(Some(m.to_ascii_uppercase())),
        _ => anyhow::bail!("invalid mount point \"{m}\""),
    }
}

pub fn run_service(
    prefix: Option<String>,
    mountpoint: Option<String>,
    user: Option<String>,
) -> anyhow::Result<()> {
    let result = (|| {
        let prefix = prefix.ok_or_else(|| anyhow::anyhow!("launcher mode needs -u"))?;
        let inst = parse_prefix(&prefix)?;
        let mountpoint = parse_mountpoint(mountpoint.as_deref().unwrap_or(""))?;
        let user = user.ok_or_else(|| anyhow::anyhow!("launcher mode needs -U"))?;
        let sid = account_sid(&user)?;
        let sd = SecurityDescriptor::read_only_for(&sid)?;
        log::info!("{} for {user} ({sid}) on {mountpoint:?}", inst.prefix);

        let dev = Arc::new(Device::open(&Source::PhysicalDrive(inst.drive), None)?);
        let cands = partition::probe(&dev);
        let cand = partition::select(&cands, inst.part.as_deref())?;
        let vol = Arc::new(Volume::open(dev.clone(), cand, Charset::Utf8)?);
        // The launcher runs as SYSTEM, so a drive letter is global without the Mount Manager.
        serve(vol, sd, Some(inst.prefix), mountpoint, false)
    })();
    if let Err(e) = &result {
        log::error!("{e:#}");
    }
    result
}

fn log_dir() -> anyhow::Result<PathBuf> {
    Ok(program_data()?.join("ufsmount"))
}

/// Only SYSTEM and Administrators may use the folder: the launcher writes its
/// logs there as SYSTEM.
fn admin_only_dir(dir: &Path) -> anyhow::Result<()> {
    secure_dir(dir, ADMIN_ONLY_DIR)
}

fn set_sz(key: HKEY, name: &str, value: &str) -> anyhow::Result<()> {
    let data: Vec<u8> = value
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    // SAFETY: key is open for writing; data is a NUL-terminated UTF-16 string.
    unsafe { RegSetValueExW(key, &HSTRING::from(name), None, REG_SZ, Some(&data)) }
        .ok()
        .map_err(|e| anyhow::anyhow!("cannot write {name}: {e}"))
}

pub fn register() -> anyhow::Result<()> {
    anyhow::ensure!(is_elevated(), "register needs an elevated prompt");
    // The launcher runs the exe as SYSTEM: only administrators may be able to change it.
    let exe = std::env::current_exe()?;
    let folder = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no folder for {}", exe.display()))?;
    check_admin_only(folder)
        .and_then(|()| check_admin_only(&exe))
        .map_err(|e| anyhow::anyhow!("{e}. Install ufsmount.exe under Program Files first."))?;

    let base = log_dir()?;
    admin_only_dir(&base)?;
    let logs = base.join("log");
    admin_only_dir(&logs)?;

    let mut key = HKEY::default();
    // SAFETY: creating a registry key with valid out pointers.
    unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            SERVICE_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_WOW64_32KEY,
            None,
            &mut key,
            None,
        )
    }
    .ok()
    .map_err(|e| anyhow::anyhow!("cannot create the launcher registration: {e}"))?;
    let result = (|| {
        set_sz(key, "Executable", &exe.to_string_lossy())?;
        set_sz(key, "CommandLine", "-u %1 -m %2 -U %U")?;
        set_sz(key, "Stderr", &format!(r"{}\%T.log", logs.display()))?;
        let one = 1u32.to_le_bytes();
        // SAFETY: key is open for writing.
        unsafe { RegSetValueExW(key, w!("JobControl"), None, REG_DWORD, Some(&one)) }
            .ok()
            .map_err(|e| anyhow::anyhow!("cannot write JobControl: {e}"))
    })();
    // SAFETY: key was opened above.
    let _ = unsafe { RegCloseKey(key) };
    result?;
    println!("Registered {} with the WinFsp launcher.", exe.display());
    println!(r"Use it from an elevated prompt: net use U: \\ufs\PhysicalDriveN /persistent:no");
    Ok(())
}

pub fn unregister() -> anyhow::Result<()> {
    // SAFETY: deleting a registry key by name.
    unsafe { RegDeleteKeyExW(HKEY_LOCAL_MACHINE, SERVICE_KEY, KEY_WOW64_32KEY.0, None) }
        .ok()
        .map_err(|e| anyhow::anyhow!("cannot remove the launcher registration: {e}"))?;
    println!("Removed the WinFsp launcher registration.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        let p = parse_prefix(r"\ufs\PhysicalDrive1").unwrap();
        assert_eq!((p.drive, p.part), (1, None));
        let p = parse_prefix(r"\UFS\physicaldrive12S1A\").unwrap();
        assert_eq!((p.drive, p.part.as_deref()), (12, Some("s1a")));
        assert_eq!(p.prefix, r"\UFS\physicaldrive12S1A");
        assert_eq!(
            parse_prefix(r"\ufs\PhysicalDrive0p2")
                .unwrap()
                .part
                .as_deref(),
            Some("p2")
        );
        for bad in [
            r"\ufs\PhysicalDrive",
            r"\ufs\PhysicalDrive1234",
            r"\ufs\PhysicalDrive1s5",
            r"\ufs\PhysicalDrive1s1u",
            r"\ufs\PhysicalDrive1x",
            r"\other\PhysicalDrive1",
            r"\ufs\..\PhysicalDrive1",
            r"\ufs\PhysicalDrive1\..",
            r"ufs\PhysicalDrive1",
            r"\ufs\C:",
        ] {
            assert!(parse_prefix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn mountpoints() {
        assert_eq!(parse_mountpoint("").unwrap(), None);
        assert_eq!(parse_mountpoint("u:").unwrap().as_deref(), Some("U:"));
        for bad in ["U", r"U:\", r"C:\dir", "UU:", "1:"] {
            assert!(parse_mountpoint(bad).is_err(), "{bad}");
        }
    }
}
