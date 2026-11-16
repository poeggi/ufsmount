// SPDX-License-Identifier: AGPL-3.0-or-later
//! WinFsp launcher mode: `net use U: \\<class>\PhysicalDriveN[part]`.
//!
//! The launcher starts `<exe> -u \<class>\Instance -m U: -U DOMAIN\user` as
//! SYSTEM. Its arguments come from whoever may talk to the launcher, so they
//! are checked strictly. The class is the product's.

use std::path::PathBuf;
use std::sync::Arc;

use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WOW64_32KEY, REG_DWORD, REG_OPTION_NON_VOLATILE,
    REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyExW, RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::device::{Device, Disk, Source, strip_prefix_ci};
use crate::names::Charset;
use crate::partition;
use crate::product::product;
use crate::security::{
    ADMIN_ONLY_DIR, SecurityDescriptor, account_sid, check_install_path, is_elevated, program_data,
    secure_dir,
};
use crate::volume::Volume;
use crate::winfs::serve;

fn service_key() -> HSTRING {
    HSTRING::from(format!(
        r"SOFTWARE\WinFsp\Services\{}",
        product().launcher_class
    ))
}

/// A checked launcher instance: `\<class>\PhysicalDrive1s1a`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub prefix: String,
    pub drive: u32,
    pub part: Option<String>,
}

fn number(s: &[u8]) -> bool {
    (1..=3).contains(&s.len()) && s.iter().all(u8::is_ascii_digit)
}

/// A partition id as `probe` shows it: `whole`, a label letter `a`..`t`, or
/// `s<n>`, `p<n>`, `m<n>` with an optional label letter; each with an
/// optional `v<n>` (one of several file systems in one place).
fn valid_part(part: &[u8]) -> bool {
    let base = match part.iter().rposition(|&b| b == b'v') {
        Some(i) if number(&part[i + 1..]) => &part[..i],
        _ => part,
    };
    let letter = |l: &u8| (b'a'..=b't').contains(l);
    match base {
        b"whole" => true,
        [l] => letter(l),
        [b's' | b'p' | b'm', rest @ ..] => match rest.split_last() {
            Some((l, n)) if letter(l) => number(n),
            _ => number(rest),
        },
        _ => false,
    }
}

/// Accept only `\<class>\PhysicalDrive<n>` with an optional partition id
/// (see [`valid_part`]), any case, an optional trailing `\`.
pub fn parse_prefix(class: &str, prefix: &str) -> anyhow::Result<Instance> {
    let bad = || anyhow::anyhow!("invalid volume prefix \"{prefix}\"");
    let trimmed = prefix.strip_suffix('\\').unwrap_or(prefix);
    let rest = strip_prefix_ci(trimmed, &format!(r"\{class}\")).ok_or_else(bad)?;
    let rest = strip_prefix_ci(rest, "PhysicalDrive").ok_or_else(bad)?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=3).contains(&digits) {
        return Err(bad());
    }
    let drive: u32 = rest[..digits].parse().map_err(|_| bad())?;
    let part = rest[digits..].to_ascii_lowercase();
    if !part.is_empty() && !valid_part(part.as_bytes()) {
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
        let inst = parse_prefix(product().launcher_class, &prefix)?;
        let mountpoint = parse_mountpoint(mountpoint.as_deref().unwrap_or(""))?;
        let user = user.ok_or_else(|| anyhow::anyhow!("launcher mode needs -U"))?;
        let sid = account_sid(&user)?;
        let sd = SecurityDescriptor::read_only_for(&sid)?;
        log::info!("{} for {user} ({sid}) on {mountpoint:?}", inst.prefix);

        let dev = Arc::new(Device::open(&Source::Disk(Disk::Drive(inst.drive)), None)?);
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
    Ok(program_data()?.join(product().id))
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
    check_install_path(&exe).map_err(|e| {
        anyhow::anyhow!(
            "{e:#}. Install {}.exe under Program Files first.",
            product().id
        )
    })?;

    let base = log_dir()?;
    // Only SYSTEM and Administrators: the launcher writes its logs there as SYSTEM.
    secure_dir(&base, ADMIN_ONLY_DIR)?;
    let logs = base.join("log");
    secure_dir(&logs, ADMIN_ONLY_DIR)?;

    let mut key = HKEY::default();
    // SAFETY: creating a registry key with valid out pointers.
    unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            &service_key(),
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
    println!(
        r"Use it from an elevated prompt: net use {}: \\{}\PhysicalDriveN /persistent:no",
        product().first_letter,
        product().launcher_class
    );
    Ok(())
}

pub fn unregister() -> anyhow::Result<()> {
    // SAFETY: deleting a registry key by name.
    unsafe { RegDeleteKeyExW(HKEY_LOCAL_MACHINE, &service_key(), KEY_WOW64_32KEY.0, None) }
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
        let parse = |p: &str| parse_prefix("demo", p);
        let p = parse(r"\demo\PhysicalDrive1").unwrap();
        assert_eq!((p.drive, p.part), (1, None));
        let p = parse(r"\DEMO\physicaldrive12S1A\").unwrap();
        assert_eq!((p.drive, p.part.as_deref()), (12, Some("s1a")));
        assert_eq!(p.prefix, r"\DEMO\physicaldrive12S1A");
        assert_eq!(
            parse(r"\demo\PhysicalDrive0p2").unwrap().part.as_deref(),
            Some("p2")
        );
        for part in [
            "whole", "a", "t", "s1", "s132", "s1a", "s6t", "p2", "p128d", "m3", "m3a", "s1v2",
            "p1av12", "wholev1", "av3",
        ] {
            let p = parse(&format!(r"\demo\PhysicalDrive3{part}")).unwrap();
            assert_eq!(p.part.as_deref(), Some(part));
        }
        for bad in [
            r"\demo\PhysicalDrive",
            r"\demo\PhysicalDrive1234",
            r"\demo\PhysicalDrive1s1u",
            r"\demo\PhysicalDrive1s1234",
            r"\demo\PhysicalDrive1sa",
            r"\demo\PhysicalDrive1s1ab",
            r"\demo\PhysicalDrive1u",
            r"\demo\PhysicalDrive1v1",
            r"\demo\PhysicalDrive1s1v",
            r"\demo\PhysicalDrive1s1v1234",
            r"\demo\PhysicalDrive1s1v1v1",
            r"\demo\PhysicalDrive1whol",
            r"\demo\PhysicalDrive1x",
            r"\demo\PhysicalDrive1x1",
            r"\demo\PhysicalDrive1p1.",
            r"\other\PhysicalDrive1",
            r"\demo\..\PhysicalDrive1",
            r"\demo\PhysicalDrive1\..",
            r"demo\PhysicalDrive1",
            r"\demo\C:",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
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
