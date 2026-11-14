// SPDX-License-Identifier: AGPL-3.0-or-later
//! The system PATH entry for the command line, set and removed by the setup.
//! The value keeps its registry type (REG_SZ or REG_EXPAND_SZ), and the
//! uninstaller takes back exactly what was added.

use std::path::Path;

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY,
    REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ,
    RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyExW, RegGetValueW, RegOpenKeyExW,
    RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR, w};

const ENV_KEY: PCWSTR = w!(r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
const OWN_KEY: PCWSTR = w!(r"SOFTWARE\ufsmount");

fn same_dir(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches('\\').to_ascii_lowercase();
    !a.trim().is_empty() && norm(a) == norm(b)
}

/// Whether `paths` (`;`-separated) lists `dir`.
pub fn has_dir(paths: &str, dir: &str) -> bool {
    paths.split(';').any(|p| same_dir(p, dir))
}

/// What to append to `paths` for `dir`: `;dir`, or `dir` after an empty value
/// or a trailing `;`.
pub fn suffix_for(paths: &str, dir: &str) -> String {
    if paths.is_empty() || paths.ends_with(';') {
        dir.to_string()
    } else {
        format!(";{dir}")
    }
}

/// `paths` without what was added: the exact suffix if it is still at the end,
/// else every entry for `dir`. All other entries stay as they are.
pub fn without(paths: &str, dir: &str, added: &str) -> String {
    if let Some(rest) = paths.strip_suffix(added) {
        return rest.to_string();
    }
    paths
        .split(';')
        .filter(|p| !same_dir(p, dir))
        .collect::<Vec<_>>()
        .join(";")
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key is owned.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// A string value with its type; None if it does not exist.
fn read(root: HKEY, key: PCWSTR, name: PCWSTR) -> Option<(String, REG_VALUE_TYPE)> {
    let mut kind = REG_VALUE_TYPE::default();
    let mut size = 0u32;
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    // SAFETY: two-call pattern with a buffer of the reported size.
    unsafe {
        if RegGetValueW(
            root,
            key,
            name,
            flags,
            Some(&mut kind),
            None,
            Some(&mut size),
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        size = (buf.len() * 2) as u32;
        if RegGetValueW(
            root,
            key,
            name,
            flags,
            Some(&mut kind),
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some((String::from_utf16_lossy(&buf[..len]), kind))
    }
}

fn write(key: PCWSTR, name: PCWSTR, value: &str, kind: REG_VALUE_TYPE) -> anyhow::Result<()> {
    let mut hkey = HKEY::default();
    // SAFETY: the key is closed by the guard; the data lives through the call.
    unsafe {
        let err = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            key,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_WOW64_64KEY,
            None,
            &mut hkey,
            None,
        );
        anyhow::ensure!(
            err == ERROR_SUCCESS,
            "cannot open the registry key: {err:?}"
        );
        let hkey = Key(hkey);
        let data: Vec<u8> = value
            .encode_utf16()
            .chain(Some(0))
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let err = RegSetValueExW(hkey.0, name, None, kind, Some(&data));
        anyhow::ensure!(
            err == ERROR_SUCCESS,
            "cannot write the registry value: {err:?}"
        );
    }
    Ok(())
}

fn user_path_has(dir: &str) -> bool {
    let mut key = HKEY::default();
    // SAFETY: the key is closed by the guard.
    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Environment"),
            Some(0),
            KEY_READ,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let key = Key(key);
        read(key.0, PCWSTR::null(), w!("Path")).is_some_and(|(p, _)| has_dir(&p, dir))
    }
}

/// Add the folder of `exe` to the system PATH, unless the system or user PATH
/// has it already. Remembers what was appended.
pub fn add(exe: &Path) -> anyhow::Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no folder for {}", exe.display()))?
        .to_string_lossy()
        .into_owned();
    let (paths, kind) = read(HKEY_LOCAL_MACHINE, ENV_KEY, w!("Path"))
        .ok_or_else(|| anyhow::anyhow!("cannot read the system PATH; not changed"))?;
    if has_dir(&paths, &dir) || user_path_has(&dir) {
        log::info!("PATH has {dir} already");
        return Ok(());
    }
    let added = suffix_for(&paths, &dir);
    write(ENV_KEY, w!("Path"), &format!("{paths}{added}"), kind)?;
    write(OWN_KEY, w!("PathAdded"), &added, REG_SZ)?;
    log::info!("added {dir} to the system PATH");
    Ok(())
}

/// Take back what `add` appended, keeping the value's type.
pub fn remove(exe: &Path) -> anyhow::Result<()> {
    let Some((added, _)) = read(HKEY_LOCAL_MACHINE, OWN_KEY, w!("PathAdded")) else {
        return Ok(());
    };
    let dir = exe
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some((paths, kind)) = read(HKEY_LOCAL_MACHINE, ENV_KEY, w!("Path")) {
        write(ENV_KEY, w!("Path"), &without(&paths, &dir, &added), kind)?;
    }
    // SAFETY: plain call.
    let err = unsafe {
        RegDeleteKeyExW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(r"SOFTWARE\ufsmount"),
            KEY_WOW64_64KEY.0,
            None,
        )
    };
    if err != ERROR_SUCCESS {
        log::info!("SOFTWARE\\ufsmount: {err:?}");
    }
    log::info!("removed {dir} from the system PATH");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = r"C:\Program Files\ufsmount";

    #[test]
    fn add_and_remove() {
        for before in [r"C:\a;C:\b", r"C:\a;C:\b;", ""] {
            let added = suffix_for(before, DIR);
            let after = format!("{before}{added}");
            assert!(has_dir(&after, DIR));
            assert_eq!(without(&after, DIR, &added), before);
        }
        assert!(has_dir(r"C:\a;c:\program files\UFSMOUNT\;C:\b", DIR));
        assert!(!has_dir(r"C:\a;;C:\b", DIR));
    }

    #[test]
    fn remove_when_not_last() {
        let paths = format!(r"C:\a;{DIR};C:\later");
        assert_eq!(without(&paths, DIR, &format!(";{DIR}")), r"C:\a;C:\later");
        // Empty entries of others stay.
        let paths = format!(r"C:\a;;{DIR};C:\x");
        assert_eq!(without(&paths, DIR, &format!(";{DIR}")), r"C:\a;;C:\x");
    }
}
