// SPDX-License-Identifier: AGPL-3.0-or-later
//! The system PATH entry for the command line, set and removed by the setup.
//! The value keeps its registry type (REG_SZ or REG_EXPAND_SZ), and the
//! uninstaller takes back exactly what was added.

use std::path::Path;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY,
    REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ,
    RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyExW, RegGetValueW, RegOpenKeyExW,
    RegSetValueExW,
};
use windows::core::{PCWSTR, w};

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

/// The text of a registry string: trailing NULs are dropped. An embedded NUL
/// or invalid UTF-16 is an error: written back, the value would be cut or
/// changed.
fn decode(units: &[u16]) -> anyhow::Result<String> {
    let end = units.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
    let units = &units[..end];
    anyhow::ensure!(!units.contains(&0), "the value holds a NUL character");
    String::from_utf16(units).map_err(|_| anyhow::anyhow!("the value is not valid UTF-16"))
}

/// A string value with its type; None if it does not exist.
fn read(root: HKEY, key: PCWSTR, name: PCWSTR) -> anyhow::Result<Option<(String, REG_VALUE_TYPE)>> {
    let mut kind = REG_VALUE_TYPE::default();
    let mut size = 0u32;
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    // SAFETY: two-call pattern with a buffer of the reported size.
    unsafe {
        let err = RegGetValueW(
            root,
            key,
            name,
            flags,
            Some(&mut kind),
            None,
            Some(&mut size),
        );
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        anyhow::ensure!(
            err == ERROR_SUCCESS,
            "cannot read the registry value: {err:?}"
        );
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        size = (buf.len() * 2) as u32;
        let err = RegGetValueW(
            root,
            key,
            name,
            flags,
            Some(&mut kind),
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        );
        anyhow::ensure!(
            err == ERROR_SUCCESS,
            "cannot read the registry value: {err:?}"
        );
        let len = (size as usize / 2).min(buf.len());
        Ok(Some((decode(&buf[..len])?, kind)))
    }
}

/// The longest value of an environment variable, in UTF-16 units.
const MAX_VALUE: usize = 32767;

fn check_len(value: &str) -> anyhow::Result<()> {
    let n = value.encode_utf16().count();
    anyhow::ensure!(
        n <= MAX_VALUE,
        "the system PATH would be {n} characters long, more than {MAX_VALUE}; not changed"
    );
    Ok(())
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
        read(key.0, PCWSTR::null(), w!("Path"))
            .ok()
            .flatten()
            .is_some_and(|(p, _)| has_dir(&p, dir))
    }
}

/// Delete what `add` remembered.
fn forget() {
    // SAFETY: plain call.
    let err = unsafe { RegDeleteKeyExW(HKEY_LOCAL_MACHINE, OWN_KEY, KEY_WOW64_64KEY.0, None) };
    if err != ERROR_SUCCESS {
        log::info!("SOFTWARE\\ufsmount: {err:?}");
    }
}

/// Add the folder of `exe` to the system PATH, unless the system or user PATH
/// has it already. Remembers what was appended.
pub fn add(exe: &Path) -> anyhow::Result<()> {
    let dir = exe
        .parent()
        .and_then(Path::to_str)
        .ok_or_else(|| anyhow::anyhow!("no usable folder for {}", exe.display()))?;
    let (paths, kind) = read(HKEY_LOCAL_MACHINE, ENV_KEY, w!("Path"))
        .and_then(|v| v.ok_or_else(|| anyhow::anyhow!("it does not exist")))
        .map_err(|e| anyhow::anyhow!("cannot read the system PATH: {e:#}; not changed"))?;
    if has_dir(&paths, dir) || user_path_has(dir) {
        log::info!("PATH has {dir} already");
        return Ok(());
    }
    let added = suffix_for(&paths, dir);
    let value = format!("{paths}{added}");
    check_len(&value)?;
    // The marker goes first: an entry without it could never be taken back.
    write(OWN_KEY, w!("PathAdded"), &added, REG_SZ)?;
    if let Err(e) = write(ENV_KEY, w!("Path"), &value, kind) {
        forget();
        return Err(e);
    }
    log::info!("added {dir} to the system PATH");
    Ok(())
}

/// Take back what `add` appended, keeping the value's type.
pub fn remove(exe: &Path) -> anyhow::Result<()> {
    let Some((added, _)) = read(HKEY_LOCAL_MACHINE, OWN_KEY, w!("PathAdded"))? else {
        return Ok(());
    };
    let dir = exe
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some((paths, kind)) = read(HKEY_LOCAL_MACHINE, ENV_KEY, w!("Path"))
        .map_err(|e| anyhow::anyhow!("cannot read the system PATH: {e:#}; not changed"))?
    {
        write(ENV_KEY, w!("Path"), &without(&paths, &dir, &added), kind)?;
    }
    forget();
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
    fn registry_strings() {
        let u = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        assert_eq!(decode(&[]).unwrap(), "");
        assert_eq!(decode(&[0, 0]).unwrap(), "");
        let mut v = u(r"C:\a;C:\b");
        v.extend([0, 0]);
        assert_eq!(decode(&v).unwrap(), r"C:\a;C:\b");
        let mut v = u(r"C:\a");
        v.push(0);
        v.extend(u(r";C:\b"));
        assert!(decode(&v).is_err(), "embedded NUL");
        let mut v = u(r"C:\a");
        v.push(0xd800);
        assert!(decode(&v).is_err(), "unpaired surrogate");
    }

    #[test]
    fn length_limit() {
        assert!(check_len(&"a".repeat(MAX_VALUE)).is_ok());
        assert!(check_len(&"a".repeat(MAX_VALUE + 1)).is_err());
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
