// SPDX-License-Identifier: AGPL-3.0-or-later
//! Security descriptors, SIDs and ACL checks.

use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE, HLOCAL,
    LocalFree, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce,
    GetAclInformation, GetLengthSid, GetSecurityDescriptorDacl, GetSecurityDescriptorGroup,
    GetSecurityDescriptorLength, GetSecurityDescriptorOwner, GetTokenInformation,
    IsValidSecurityDescriptor, IsValidSid, LookupAccountNameW, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_QUERY,
    TOKEN_USER, TokenElevation, TokenElevationType, TokenUser,
};
use windows::Win32::Security::{
    PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SetKernelObjectSecurity,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::{BOOL, GUID, HRESULT, HSTRING, PWSTR};

/// WinFsp asks with a 1024-byte buffer first and loops on larger descriptors.
const MAX_SD: usize = 1024;

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
const ACCESS_DENIED_OBJECT_ACE_TYPE: u8 = 6;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;
const ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE: u8 = 12;
const INHERIT_ONLY_ACE: u8 = 0x08;
/// Rights that change a file: write/append data, write EA, delete child,
/// write attributes, DELETE, WRITE_DAC, WRITE_OWNER, GENERIC_ALL, GENERIC_WRITE.
pub const WRITE_RIGHTS: u32 = 0x5000_0000 | 0x000d_0000 | 0x0156;

/// SYSTEM, Administrators, TrustedInstaller.
const TRUSTED: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// A self-relative security descriptor.
#[derive(Clone, Debug)]
pub struct SecurityDescriptor(Vec<u8>);

impl SecurityDescriptor {
    /// Parse and check an SDDL string: owner, group and a DACL present, at most 1024 bytes.
    pub fn from_sddl(sddl: &str) -> anyhow::Result<SecurityDescriptor> {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        let mut size = 0u32;
        // SAFETY: the out pointers are valid; the result is freed with LocalFree below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut psd,
                Some(&mut size),
            )
        }
        .map_err(|e| anyhow::anyhow!("invalid SDDL \"{sddl}\": {e}"))?;
        // SAFETY: psd points to `size` bytes of a valid self-relative descriptor.
        let bytes =
            unsafe { std::slice::from_raw_parts(psd.0.cast::<u8>(), size as usize) }.to_vec();
        // SAFETY: psd was allocated by the conversion call.
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
        let sd = SecurityDescriptor(bytes);
        sd.check()?;
        Ok(sd)
    }

    /// An SDDL for the files of a mount: like [`Self::from_sddl`], and it may
    /// grant read rights only (the volume is read-only).
    pub fn for_files(sddl: &str) -> anyhow::Result<SecurityDescriptor> {
        let sd = Self::from_sddl(sddl)?;
        // SAFETY: the DACL points into sd, which lives through the call.
        let aces = unsafe { allow_aces(sd.dacl()?)? };
        if let Some((sid, _)) = aces.iter().find(|(_, mask)| mask & WRITE_RIGHTS != 0) {
            anyhow::bail!("the SDDL grants write rights to {sid}; grant read rights only (FR, FX)");
        }
        Ok(sd)
    }

    /// Read access for SYSTEM, Administrators and one user.
    pub fn read_only_for(user_sid: &str) -> anyhow::Result<SecurityDescriptor> {
        Self::for_files(&format!(
            "O:BAG:BAD:P(A;;FRFX;;;SY)(A;;FRFX;;;BA)(A;;FRFX;;;{user_sid})"
        ))
    }

    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.0.len() <= MAX_SD,
            "security descriptor too large ({} bytes, at most {MAX_SD})",
            self.0.len()
        );
        let psd = self.as_psd();
        // SAFETY: psd points into self.0, which outlives these calls.
        unsafe {
            anyhow::ensure!(
                IsValidSecurityDescriptor(psd).as_bool(),
                "invalid security descriptor"
            );
            anyhow::ensure!(
                GetSecurityDescriptorLength(psd) as usize == self.0.len(),
                "security descriptor length mismatch"
            );
            let mut sid = PSID::default();
            let mut defaulted = BOOL::default();
            GetSecurityDescriptorOwner(psd, &mut sid, &mut defaulted)?;
            anyhow::ensure!(!sid.is_invalid(), "security descriptor has no owner (O:)");
            GetSecurityDescriptorGroup(psd, &mut sid, &mut defaulted)?;
            anyhow::ensure!(!sid.is_invalid(), "security descriptor has no group (G:)");
        }
        self.dacl()?;
        Ok(())
    }

    /// The DACL; a missing or NULL DACL (which grants everyone everything) is an error.
    fn dacl(&self) -> anyhow::Result<*const ACL> {
        let mut present = BOOL::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut defaulted = BOOL::default();
        // SAFETY: the descriptor is valid; out pointers are valid.
        unsafe {
            GetSecurityDescriptorDacl(self.as_psd(), &mut present, &mut dacl, &mut defaulted)?;
        }
        anyhow::ensure!(
            present.as_bool() && !dacl.is_null(),
            "security descriptor has no DACL (D:), which would grant everyone full access"
        );
        Ok(dacl)
    }

    pub(crate) fn as_psd(&self) -> PSECURITY_DESCRIPTOR {
        PSECURITY_DESCRIPTOR(self.0.as_ptr().cast_mut().cast())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Copy into a WinFsp buffer if it is large enough.
    pub fn copy_to(&self, buf: Option<&mut [c_void]>) {
        if let Some(buf) = buf
            && buf.len() >= self.0.len()
        {
            // SAFETY: buf is a writable byte buffer of at least self.0.len() bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.0.as_ptr(),
                    buf.as_mut_ptr().cast::<u8>(),
                    self.0.len(),
                )
            };
        }
    }
}

/// Where the SID starts in an ACE that grants access; None for an ACE that
/// denies. Other ACE types are refused.
///
/// # Safety
/// `ace` must point to a valid ACE.
unsafe fn allow_sid_offset(ace: *const u8) -> anyhow::Result<Option<usize>> {
    // SAFETY: the header is at the start of every ACE.
    let kind = unsafe { (*ace.cast::<ACE_HEADER>()).AceType };
    Ok(match kind {
        ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => Some(8),
        ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => {
            // Header, mask, flags, then only the GUIDs the flags name.
            // SAFETY: object ACEs have the flags after the mask.
            let flags = unsafe { ace.add(8).cast::<u32>().read_unaligned() };
            Some(12 + 16 * usize::from(flags & 1 != 0) + 16 * usize::from(flags & 2 != 0))
        }
        ACCESS_DENIED_ACE_TYPE
        | ACCESS_DENIED_OBJECT_ACE_TYPE
        | ACCESS_DENIED_CALLBACK_ACE_TYPE
        | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => None,
        t => anyhow::bail!("unsupported ACE type {t}"),
    })
}

/// The ACEs of a DACL that grant access to the object itself: (SID string, mask).
///
/// # Safety
/// `dacl` must point to a valid ACL.
unsafe fn allow_aces(dacl: *const ACL) -> anyhow::Result<Vec<(String, u32)>> {
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: valid ACL per the contract; info is properly sized.
    unsafe {
        GetAclInformation(
            dacl,
            (&raw mut info).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )?;
    }
    let mut out = Vec::new();
    for i in 0..info.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: i is below the ACE count; GetAce returns a pointer into the
        // ACL. The SID is read only within the ACE's size.
        unsafe {
            GetAce(dacl, i, &mut ace)?;
            let ace = ace.cast::<u8>().cast_const();
            let header = &*ace.cast::<ACE_HEADER>();
            let Some(offset) = allow_sid_offset(ace)? else {
                continue;
            };
            if header.AceFlags & INHERIT_ONLY_ACE != 0 {
                continue;
            }
            let size = usize::from(header.AceSize);
            // A SID has at least 8 bytes.
            anyhow::ensure!(offset + 8 <= size, "malformed ACE");
            let sid = PSID(ace.add(offset).cast_mut().cast());
            anyhow::ensure!(
                IsValidSid(sid).as_bool() && offset + GetLengthSid(sid) as usize <= size,
                "malformed ACE"
            );
            let mask = ace.add(4).cast::<u32>().read_unaligned();
            out.push((sid_to_string(sid)?, mask));
        }
    }
    Ok(out)
}

/// A kernel handle, closed on drop.
pub(crate) struct Handle(pub(crate) HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Open `path` itself: a junction or symlink there is opened, not followed.
fn open_no_follow(path: &Path, access: u32) -> windows::core::Result<Handle> {
    // SAFETY: the path lives through the call; the handle goes to the guard.
    unsafe {
        CreateFileW(
            &HSTRING::from(path.as_os_str()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map(Handle)
}

/// The attributes of an open file or folder.
fn attributes(h: &Handle, path: &Path) -> anyhow::Result<u32> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: valid handle and out pointer.
    unsafe { GetFileInformationByHandle(h.0, &mut info) }
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    Ok(info.dwFileAttributes)
}

fn has_win32_code(e: &windows::core::Error, codes: &[WIN32_ERROR]) -> bool {
    codes.iter().any(|c| e.code() == HRESULT::from_win32(c.0))
}

/// Check that `path` is no junction or symlink, and that only SYSTEM,
/// Administrators and TrustedInstaller own or can change it.
pub fn check_admin_only(path: &Path) -> anyhow::Result<()> {
    let h = open_no_follow(path, READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    anyhow::ensure!(
        attributes(&h, path)? & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
        "{} is a junction or symlink",
        path.display()
    );
    check_handle_admin_only(&h, path)
}

/// [`check_admin_only`] for an open file or folder.
fn check_handle_admin_only(h: &Handle, path: &Path) -> anyhow::Result<()> {
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: out pointers are valid; psd is freed with LocalFree.
    let err = unsafe {
        GetSecurityInfo(
            h.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut psd),
        )
    };
    anyhow::ensure!(
        err.is_ok(),
        "{}: cannot read its permissions",
        path.display()
    );
    let result = (|| {
        anyhow::ensure!(!dacl.is_null(), "{}: has no DACL", path.display());
        let owner = sid_to_string(owner)?;
        anyhow::ensure!(
            TRUSTED.contains(&owner.as_str()),
            "{}: owned by {owner}, not by SYSTEM or Administrators",
            path.display()
        );
        // SAFETY: dacl points into psd, which is still allocated.
        for (sid, mask) in unsafe { allow_aces(dacl)? } {
            anyhow::ensure!(
                mask & WRITE_RIGHTS == 0 || TRUSTED.contains(&sid.as_str()),
                "{}: {sid} may change it; only SYSTEM and Administrators may",
                path.display()
            );
        }
        Ok(())
    })();
    // SAFETY: psd was allocated by GetSecurityInfo.
    unsafe { LocalFree(Some(HLOCAL(psd.0))) };
    result
}

fn sid_to_string(sid: PSID) -> anyhow::Result<String> {
    let mut s = PWSTR::null();
    // SAFETY: sid is valid for the duration of the call; s is freed with LocalFree.
    unsafe {
        ConvertSidToStringSidW(sid, &mut s)?;
        let out = s.to_string();
        LocalFree(Some(HLOCAL(s.0.cast())));
        Ok(out?)
    }
}

fn process_token() -> anyhow::Result<Handle> {
    let mut token = HANDLE::default();
    // SAFETY: valid out pointer.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)? };
    Ok(Handle(token))
}

/// The SID of the user this process runs as.
pub fn current_user_sid() -> anyhow::Result<String> {
    let token = process_token()?;
    // SAFETY: two-call pattern with a buffer of the reported size.
    unsafe {
        let mut needed = 0u32;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        sid_to_string(user.User.Sid)
    }
}

/// Whether `token` (open for TOKEN_QUERY) is elevated.
fn token_elevated(token: HANDLE) -> anyhow::Result<bool> {
    let mut elevation = TOKEN_ELEVATION::default();
    let mut needed = 0u32;
    // SAFETY: the buffer is a properly sized TOKEN_ELEVATION.
    unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut needed,
        )?;
    }
    Ok(elevation.TokenIsElevated != 0)
}

/// The elevation type of `token` (open for TOKEN_QUERY). Full and Limited are
/// the two halves of a UAC split logon; Default is a token that has no other
/// half (no admin rights, UAC off, or the built-in Administrator).
pub(crate) fn token_elevation_type(token: HANDLE) -> anyhow::Result<TOKEN_ELEVATION_TYPE> {
    let mut kind = TOKEN_ELEVATION_TYPE::default();
    let mut needed = 0u32;
    // SAFETY: the buffer is a properly sized TOKEN_ELEVATION_TYPE.
    unsafe {
        GetTokenInformation(
            token,
            TokenElevationType,
            Some((&raw mut kind).cast()),
            size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut needed,
        )?;
    }
    Ok(kind)
}

/// Whether this process runs elevated.
pub fn is_elevated() -> bool {
    process_token()
        .and_then(|t| token_elevated(t.0))
        .unwrap_or(false)
}

/// The SID of an account name such as `DOMAIN\user`.
pub fn account_sid(name: &str) -> anyhow::Result<String> {
    let wname = HSTRING::from(name);
    let mut sid_len = 0u32;
    let mut dom_len = 0u32;
    let mut kind = SID_NAME_USE::default();
    // SAFETY: two-call pattern; the second call gets buffers of the reported sizes.
    unsafe {
        let _ = LookupAccountNameW(
            None,
            &wname,
            None,
            &mut sid_len,
            None,
            &mut dom_len,
            &mut kind,
        );
        anyhow::ensure!(sid_len > 0, "unknown account \"{name}\"");
        let mut sid = vec![0u64; (sid_len as usize).div_ceil(8)];
        let mut dom = vec![0u16; dom_len as usize + 1];
        LookupAccountNameW(
            None,
            &wname,
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut sid_len,
            Some(PWSTR(dom.as_mut_ptr())),
            &mut dom_len,
            &mut kind,
        )
        .map_err(|e| anyhow::anyhow!("unknown account \"{name}\": {e}"))?;
        sid_to_string(PSID(sid.as_mut_ptr().cast()))
    }
}

/// Folder SDDL: only SYSTEM and Administrators.
pub const ADMIN_ONLY_DIR: &str = "O:BAG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// Folder SDDL: SYSTEM and Administrators full, Users read.
pub const ADMIN_DIR_USERS_READ: &str =
    "O:BAG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)";

fn known_folder(id: &GUID, name: &str) -> anyhow::Result<PathBuf> {
    // SAFETY: the returned string is freed with CoTaskMemFree after copying.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)
            .map_err(|e| anyhow::anyhow!("cannot find {name}: {e}"))?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0.cast()));
        Ok(PathBuf::from(s?))
    }
}

/// The ProgramData folder, from the known-folder API. Environment variables
/// are not used: any process of the user can set them for an elevated child.
pub fn program_data() -> anyhow::Result<PathBuf> {
    known_folder(&FOLDERID_ProgramData, "ProgramData")
}

/// Check where `exe` is installed: it runs elevated or as SYSTEM without
/// asking. Its folder must be Program Files or lie below it. The exe and every
/// folder below Program Files down to it must be admin-only, and none may be a
/// junction or symlink.
pub fn check_install_path(exe: &Path) -> anyhow::Result<()> {
    check_below(&known_folder(&FOLDERID_ProgramFiles, "Program Files")?, exe)
}

/// [`check_install_path`] with another trusted `root`. From the top down: once
/// a folder passes, nobody else can rename or replace what lies in it.
fn check_below(root: &Path, exe: &Path) -> anyhow::Result<()> {
    let folders = exe
        .parent()
        .and_then(|dir| folders_below(root, dir))
        .ok_or_else(|| anyhow::anyhow!("{} is not under {}", exe.display(), root.display()))?;
    for dir in &folders {
        check_admin_only(dir)?;
    }
    check_admin_only(exe)
}

/// The folders from just below `root` down to `dir`, top first, spelled as in
/// `dir`. None unless `dir` is `root` or lies below it. Case is ignored; `..`
/// is refused.
fn folders_below(root: &Path, dir: &Path) -> Option<Vec<PathBuf>> {
    let mut rest = dir.components();
    let mut cur = PathBuf::new();
    for want in root.components() {
        let got = rest.next()?;
        if !got.as_os_str().eq_ignore_ascii_case(want.as_os_str()) {
            return None;
        }
        cur.push(got);
    }
    rest.map(|c| match c {
        Component::Normal(name) => {
            cur.push(name);
            Some(cur.clone())
        }
        _ => None,
    })
    .collect()
}

/// Make `dir` a real folder owned by Administrators with the protected DACL of
/// `sddl`. Elevated code writes there, so nobody else may have prepared it or
/// its contents: a new folder gets its security at creation; an existing one is
/// kept only if it is admin-only already, else it is emptied and created anew.
/// A junction or symlink is refused. An existing folder is checked and changed
/// through one handle.
pub fn secure_dir(dir: &Path, sddl: &str) -> anyhow::Result<()> {
    let sd = SecurityDescriptor::from_sddl(sddl)?;
    let access = READ_CONTROL.0 | WRITE_DAC.0 | WRITE_OWNER.0 | FILE_READ_ATTRIBUTES.0;
    let h = match open_no_follow(dir, access) {
        Ok(h) => h,
        Err(e) if has_win32_code(&e, &[ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]) => {
            return create_dir_with(dir, &sd);
        }
        Err(e) if has_win32_code(&e, &[ERROR_ACCESS_DENIED]) => {
            return create_anew(dir, &sd, anyhow::anyhow!("{}: {e}", dir.display()));
        }
        Err(e) => anyhow::bail!("cannot open {}: {e}", dir.display()),
    };
    let attrs = attributes(&h, dir)?;
    anyhow::ensure!(
        attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0 && attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
        "{} is not a plain folder; remove it and try again",
        dir.display()
    );
    if let Err(e) = check_handle_admin_only(&h, dir) {
        drop(h);
        return create_anew(dir, &sd, e);
    }
    // Not SetSecurityInfo: it would also walk the children by path to
    // propagate the inheritable ACEs.
    // SAFETY: the handle is open for WRITE_DAC and WRITE_OWNER; sd outlives the call.
    unsafe {
        SetKernelObjectSecurity(
            h.0,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            sd.as_psd(),
        )
    }
    .map_err(|e| anyhow::anyhow!("cannot secure {}: {e}", dir.display()))
}

fn create_anew(dir: &Path, sd: &SecurityDescriptor, why: anyhow::Error) -> anyhow::Result<()> {
    log::warn!("{why:#}; creating it anew");
    std::fs::remove_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", dir.display()))?;
    create_dir_with(dir, sd)
}

fn create_dir_with(dir: &Path, sd: &SecurityDescriptor) -> anyhow::Result<()> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.as_psd().0,
        bInheritHandle: false.into(),
    };
    // SAFETY: sa and sd live through the call.
    unsafe { CreateDirectoryW(&HSTRING::from(dir.as_os_str()), Some(&sa)) }
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))
}

/// Remove `dir` and everything below it. A junction or symlink at `dir` is refused.
pub fn remove_dir(dir: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
                "{} is not a plain folder; not removed",
                dir.display()
            );
            std::fs::remove_dir_all(dir)
                .map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", dir.display()))
        }
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_descriptor() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"));
        let sd = SecurityDescriptor::read_only_for(&sid).unwrap();
        assert!(sd.len() < MAX_SD);
    }

    #[test]
    fn rejects_bad_sddl() {
        assert!(SecurityDescriptor::from_sddl("garbage").is_err());
        assert!(
            SecurityDescriptor::from_sddl("D:P(A;;FA;;;WD)").is_err(),
            "no owner/group"
        );
        assert!(
            SecurityDescriptor::from_sddl("O:BAG:BA").is_err(),
            "no DACL"
        );
        assert!(SecurityDescriptor::from_sddl("O:BAG:BAD:P(A;;FA;;;WD)").is_ok());
        assert!(SecurityDescriptor::for_files("O:BAG:BAD:P(A;;FA;;;WD)").is_err());
        assert!(SecurityDescriptor::for_files("O:BAG:BAD:P(A;;FRFX;;;WD)").is_ok());
    }

    #[test]
    fn every_allow_type_grants() {
        const G1: &str = "00000000-0000-0000-0000-000000000001";
        const G2: &str = "00000000-0000-0000-0000-000000000002";
        const COND: &str = "(Member_of {SID(BA)})";
        for sddl in [
            format!("O:BAG:BAD:(XA;;FA;;;WD;{COND})"),
            "O:BAG:BAD:(OA;;FA;;;WD)".to_string(),
            format!("O:BAG:BAD:(OA;;FA;{G1};;WD)"),
            format!("O:BAG:BAD:(OA;;FA;;{G2};WD)"),
            format!("O:BAG:BAD:(OA;;FA;{G1};{G2};WD)"),
            format!("O:BAG:BAD:(ZA;;FA;{G1};;WD;{COND})"),
        ] {
            let e = SecurityDescriptor::for_files(&sddl).unwrap_err();
            assert!(e.to_string().contains("S-1-1-0"), "{sddl}: {e}");
        }
        assert!(
            SecurityDescriptor::for_files(&format!("O:BAG:BAD:(XA;;FRFX;;;WD;{COND})")).is_ok()
        );
        // Deny ACEs only take rights away.
        assert!(SecurityDescriptor::for_files("O:BAG:BAD:(D;;FA;;;WD)(A;;FRFX;;;WD)").is_ok());
    }

    #[test]
    fn elevation_type_matches_elevation() {
        use windows::Win32::Security::{TokenElevationTypeFull, TokenElevationTypeLimited};
        let token = process_token().unwrap();
        let kind = token_elevation_type(token.0).unwrap();
        let elevated = token_elevated(token.0).unwrap();
        assert!((1..=3).contains(&kind.0), "{kind:?}");
        assert!(kind != TokenElevationTypeFull || elevated);
        assert!(kind != TokenElevationTypeLimited || !elevated);
    }

    #[test]
    fn account_lookup() {
        assert_eq!(account_sid("NT AUTHORITY\\SYSTEM").unwrap(), "S-1-5-18");
        assert!(account_sid("no-such-user-zz9").is_err());
    }

    #[test]
    fn admin_only_check() {
        let windows = std::env::var("SystemRoot").unwrap();
        check_admin_only(&Path::new(&windows).join(r"System32\notepad.exe")).unwrap();
        let tmp = std::env::temp_dir();
        assert!(
            check_admin_only(&tmp).is_err(),
            "a user temp folder is user-writable"
        );
    }

    #[test]
    fn install_folders() {
        let root = Path::new(r"C:\Program Files");
        let f = |d: &str| folders_below(root, Path::new(d));
        let p = PathBuf::from;
        assert_eq!(f(r"C:\Program Files"), Some(vec![]));
        assert_eq!(
            f(r"c:\program files\ufsmount"),
            Some(vec![p(r"c:\program files\ufsmount")])
        );
        assert_eq!(
            f(r"C:\Program Files\a\b"),
            Some(vec![p(r"C:\Program Files\a"), p(r"C:\Program Files\a\b")])
        );
        for bad in [
            r"C:\",
            r"C:\Users\a",
            r"D:\Program Files\a",
            r"C:\Program Files (x86)\a",
            r"C:\Program Files\..\Users\a",
            r"\\?\C:\Program Files\a",
        ] {
            assert_eq!(f(bad), None, "{bad}");
        }
    }

    #[test]
    fn install_path_check() {
        let windows = PathBuf::from(std::env::var("SystemRoot").unwrap());
        check_below(&windows, &windows.join(r"System32\notepad.exe")).unwrap();
        let tmp = std::env::temp_dir();
        assert!(check_below(tmp.parent().unwrap(), &tmp.join("x.exe")).is_err());
        assert!(check_install_path(&std::env::current_exe().unwrap()).is_err());
    }

    #[test]
    fn secure_dir_replaces_and_refuses() {
        // The owner is the test user, so this runs without admin rights; the
        // folder therefore never counts as admin-only.
        let me = current_user_sid().unwrap();
        let sddl = format!("O:{me}G:{me}D:P(A;OICI;FA;;;{me})");
        let base = std::env::temp_dir().join(format!("ufsmount-secure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir(&base).unwrap();

        let dir = base.join("dir");
        secure_dir(&dir, &sddl).unwrap();
        std::fs::write(dir.join("planted"), b"x").unwrap();
        secure_dir(&dir, &sddl).unwrap();
        assert!(dir.is_dir() && !dir.join("planted").exists());

        let file = base.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(secure_dir(&file, &sddl).is_err());

        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = base.join("link");
        let made = std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success());
        assert!(secure_dir(&link, &sddl).is_err());
        let e = check_admin_only(&link).unwrap_err();
        assert!(e.to_string().contains("junction"), "{e}");

        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }
}
