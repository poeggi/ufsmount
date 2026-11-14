// SPDX-License-Identifier: AGPL-3.0-or-later
//! Security descriptors, SIDs and ACL checks.

use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorDacl,
    GetSecurityDescriptorGroup, GetSecurityDescriptorLength, GetSecurityDescriptorOwner,
    GetTokenInformation, IsValidSecurityDescriptor, LookupAccountNameW, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenUser,
};
use windows::Win32::Security::{
    PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SetFileSecurityW,
};
use windows::Win32::Storage::FileSystem::{CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::{BOOL, HSTRING, PWSTR};

/// WinFsp asks with a 1024-byte buffer first and loops on larger descriptors.
const MAX_SD: usize = 1024;

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
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

/// The allow ACEs of a DACL that apply to the object itself: (SID string, mask).
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
        // SAFETY: i is below the ACE count; GetAce returns a pointer into the ACL.
        unsafe {
            GetAce(dacl, i, &mut ace)?;
            let header = &*ace.cast::<ACE_HEADER>();
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags & INHERIT_ONLY_ACE != 0
            {
                continue;
            }
            let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            let sid = PSID((&raw const allowed.SidStart).cast_mut().cast());
            out.push((sid_to_string(sid)?, allowed.Mask));
        }
    }
    Ok(out)
}

/// Check that only SYSTEM, Administrators and TrustedInstaller own or can
/// change `path`. Used before the launcher is told to run a file as SYSTEM.
pub fn check_admin_only(path: &Path) -> anyhow::Result<()> {
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: out pointers are valid; psd is freed with LocalFree.
    let err = unsafe {
        GetNamedSecurityInfoW(
            &HSTRING::from(path.as_os_str()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut psd,
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
    // SAFETY: psd was allocated by GetNamedSecurityInfoW.
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

/// The process token, closed on drop.
struct Token(HANDLE);

impl Token {
    fn current() -> anyhow::Result<Token> {
        let mut token = HANDLE::default();
        // SAFETY: valid out pointer.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)? };
        Ok(Token(token))
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by OpenProcessToken.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The SID of the user this process runs as.
pub fn current_user_sid() -> anyhow::Result<String> {
    let token = Token::current()?;
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

/// Whether this process runs elevated.
pub fn is_elevated() -> bool {
    let Ok(token) = Token::current() else {
        return false;
    };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut needed = 0u32;
    // SAFETY: the buffer is a properly sized TOKEN_ELEVATION.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut needed,
        )
    };
    ok.is_ok() && elevation.TokenIsElevated != 0
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

/// The ProgramData folder, from the known-folder API. Environment variables
/// are not used: any process of the user can set them for an elevated child.
pub fn program_data() -> anyhow::Result<PathBuf> {
    // SAFETY: the returned string is freed with CoTaskMemFree after copying.
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None)
            .map_err(|e| anyhow::anyhow!("cannot find ProgramData: {e}"))?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0.cast()));
        Ok(PathBuf::from(s?))
    }
}

/// Make `dir` a real folder owned by Administrators with the protected DACL of
/// `sddl`. Elevated code writes there, so nobody else may have prepared it or
/// its contents: a new folder gets its security at creation; an existing one is
/// kept only if it is admin-only already, else it is emptied and created anew.
/// A junction or symlink is refused.
pub fn secure_dir(dir: &Path, sddl: &str) -> anyhow::Result<()> {
    let sd = SecurityDescriptor::from_sddl(sddl)?;
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
                "{} is not a plain folder; remove it and try again",
                dir.display()
            );
            if let Err(e) = check_admin_only(dir) {
                log::warn!("{e:#}; creating it anew");
                std::fs::remove_dir_all(dir)
                    .map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", dir.display()))?;
                return create_dir_with(dir, &sd);
            }
        }
        Err(_) => return create_dir_with(dir, &sd),
    }
    // SAFETY: sd outlives the call.
    unsafe {
        SetFileSecurityW(
            &HSTRING::from(dir.as_os_str()),
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            sd.as_psd(),
        )
    }
    .ok()
    .map_err(|e| anyhow::anyhow!("cannot secure {}: {e}", dir.display()))?;
    Ok(())
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
}
