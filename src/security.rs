// SPDX-License-Identifier: AGPL-3.0-or-later
//! The one security descriptor every file and directory gets.

use std::ffi::c_void;

use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetSecurityDescriptorGroup, GetSecurityDescriptorLength, GetSecurityDescriptorOwner,
    GetTokenInformation, IsValidSecurityDescriptor, LookupAccountNameW, PSECURITY_DESCRIPTOR, PSID,
    SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{BOOL, HSTRING, PWSTR};

/// WinFsp asks with a 1024-byte buffer first and loops on larger descriptors.
const MAX_SD: usize = 1024;

/// A self-relative security descriptor.
#[derive(Clone, Debug)]
pub struct SecurityDescriptor(Vec<u8>);

impl SecurityDescriptor {
    /// Parse and check an SDDL string: owner and group present, at most 1024 bytes.
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

    /// Read access for SYSTEM, Administrators and one user.
    pub fn read_only_for(user_sid: &str) -> anyhow::Result<SecurityDescriptor> {
        Self::from_sddl(&format!(
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
        Ok(())
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

fn sid_to_string(sid: PSID) -> anyhow::Result<String> {
    let mut s = PWSTR::null();
    // SAFETY: sid is valid for the duration of the call; s is freed with LocalFree.
    unsafe {
        ConvertSidToStringSidW(sid, &mut s)?;
        let out = s.to_string()?;
        LocalFree(Some(HLOCAL(s.0.cast())));
        Ok(out)
    }
}

/// The SID of the user this process runs as.
pub fn current_user_sid() -> anyhow::Result<String> {
    // SAFETY: standard token query with properly sized buffers.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )?;
        let _ = windows::Win32::Foundation::CloseHandle(token);
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        sid_to_string(user.User.Sid)
    }
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
        assert!(SecurityDescriptor::from_sddl("O:BAG:BAD:P(A;;FA;;;WD)").is_ok());
    }

    #[test]
    fn account_lookup() {
        assert_eq!(account_sid("NT AUTHORITY\\SYSTEM").unwrap(), "S-1-5-18");
        assert!(account_sid("no-such-user-zz9").is_err());
    }
}
