// SPDX-License-Identifier: AGPL-3.0-or-later
//! The update check of the About box: the version of the latest release.

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_QUERY_LOCATION, WinHttpCloseHandle,
    WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::core::{HSTRING, PCWSTR, w};

/// Redirects to `TAG_URL` + the version of the latest release.
const LATEST_PATH: &str = "/poeggi/winfsp-ufs/releases/latest";
const TAG_URL: &str = "https://github.com/poeggi/winfsp-ufs/releases/tag/v";
const DOWNLOAD_URL: &str = "https://github.com/poeggi/winfsp-ufs/releases/download/v";
const TIMEOUT_MS: i32 = 5_000;

pub type Version = (u32, u32, u32);

/// "1.2.3" as numbers; anything else is None.
pub fn parse_version(s: &str) -> Option<Version> {
    let mut parts = s.split('.').map(|p| {
        let digits = !p.is_empty() && p.len() <= 9 && p.bytes().all(|b| b.is_ascii_digit());
        if digits { p.parse().ok() } else { None }
    });
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

pub fn current() -> Version {
    parse_version(env!("CARGO_PKG_VERSION")).expect("the package version is x.y.z")
}

/// The release page of `v`.
pub fn release_url((a, b, c): Version) -> String {
    format!("{TAG_URL}{a}.{b}.{c}")
}

/// The setup of `v`, named as `OutputBaseFilename` in installer/ufsmount.iss.
pub fn setup_url((a, b, c): Version) -> String {
    format!("{DOWNLOAD_URL}{a}.{b}.{c}/ufsmount-{a}.{b}.{c}-setup.exe")
}

struct Handle(*mut c_void);

impl Handle {
    fn new(h: *mut c_void) -> windows::core::Result<Handle> {
        if h.is_null() {
            Err(windows::core::Error::from_thread())
        } else {
            Ok(Handle(h))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle from WinHTTP, closed once.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// The version of the latest release. One HEAD request whose redirect is not
/// followed: only its target is read, nothing else from the network.
pub fn latest() -> anyhow::Result<Version> {
    let agent = HSTRING::from(format!("ufsmount/{}", env!("CARGO_PKG_VERSION")));
    // SAFETY: each handle is closed by its guard, the request before the
    // connection before the session; `buf` holds `size` bytes.
    unsafe {
        let session = Handle::new(WinHttpOpen(
            &agent,
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS)?;
        let connect = Handle::new(WinHttpConnect(session.0, w!("github.com"), 443, 0))?;
        let request = Handle::new(WinHttpOpenRequest(
            connect.0,
            w!("HEAD"),
            &HSTRING::from(LATEST_PATH),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        let never = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
        WinHttpSetOption(
            Some(request.0),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(&never),
        )?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;
        let mut buf = [0u16; 512];
        let mut size = size_of_val(&buf) as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_LOCATION,
            PCWSTR::null(),
            Some(buf.as_mut_ptr().cast()),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|e| anyhow::anyhow!("no redirect to a release: {e}"))?;
        let target = String::from_utf16_lossy(&buf[..size as usize / 2]);
        target
            .strip_prefix(TAG_URL)
            .and_then(parse_version)
            .ok_or_else(|| anyhow::anyhow!("unexpected redirect to {target}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.2.4"), Some((0, 2, 4)));
        assert_eq!(parse_version("10.20.300"), Some((10, 20, 300)));
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1..3",
            "1.2.x",
            "-1.2.3",
            "1.2.3-rc1",
            "+1.2.3",
            "1.2.9999999999",
        ] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert!((0, 10, 0) > (0, 9, 9), "numeric, not text order");
        assert_eq!(Some(current()), parse_version(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn links() {
        assert_eq!(
            release_url((0, 3, 1)),
            "https://github.com/poeggi/winfsp-ufs/releases/tag/v0.3.1"
        );
        assert_eq!(
            setup_url((0, 3, 1)),
            "https://github.com/poeggi/winfsp-ufs/releases/download/v0.3.1/ufsmount-0.3.1-setup.exe"
        );
        let iss = include_str!("../../installer/ufsmount.iss");
        assert!(
            iss.contains("OutputBaseFilename=ufsmount-{#AppVersion}-setup\r\n")
                || iss.contains("OutputBaseFilename=ufsmount-{#AppVersion}-setup\n")
        );
    }

    /// Asks github.com; run with `--ignored`.
    #[test]
    #[ignore]
    fn latest_release() {
        assert!(latest().unwrap() >= (0, 2, 4));
    }
}
