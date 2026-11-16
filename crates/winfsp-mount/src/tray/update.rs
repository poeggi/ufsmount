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

use crate::product::product;

const TIMEOUT_MS: i32 = 5_000;

/// The release page of version `v`: the latest release redirects there.
fn tag_url() -> String {
    format!("https://github.com/{}/releases/tag/v", product().repo)
}

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
    parse_version(product().version).expect("the product version is x.y.z")
}

/// The release page of `v`.
pub fn release_url((a, b, c): Version) -> String {
    format!("{}{a}.{b}.{c}", tag_url())
}

/// The setup of `v`, named as `OutputBaseFilename` in installer/common.iss.
pub fn setup_url((a, b, c): Version) -> String {
    let p = product();
    format!(
        "https://github.com/{}/releases/download/v{a}.{b}.{c}/{}-{a}.{b}.{c}-setup.exe",
        p.repo, p.id
    )
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
    let p = product();
    let agent = HSTRING::from(format!("{}/{}", p.id, p.version));
    let latest = HSTRING::from(format!("/{}/releases/latest", p.repo));
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
            &latest,
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
            .strip_prefix(tag_url().as_str())
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
        assert_eq!(current(), (1, 2, 3));
    }

    #[test]
    fn links() {
        assert_eq!(
            release_url((0, 3, 1)),
            "https://github.com/example/demo/releases/tag/v0.3.1"
        );
        assert_eq!(
            setup_url((0, 3, 1)),
            "https://github.com/example/demo/releases/download/v0.3.1/demo-0.3.1-setup.exe"
        );
    }
}
