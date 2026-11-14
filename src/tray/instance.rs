// SPDX-License-Identifier: AGPL-3.0-or-later
//! One tray per machine. Named kernel objects in the Global namespace: a mutex
//! that says "running", an event to quit (administrators only) and an event to
//! show a message (any signed-in user).

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, OpenMutexW,
    SYNCHRONIZATION_SYNCHRONIZE, SetEvent,
};
use windows::core::{HRESULT, PCWSTR, w};

use crate::security::SecurityDescriptor;

const MUTEX: PCWSTR = w!(r"Global\ufsmount-tray");
const QUIT: PCWSTR = w!(r"Global\ufsmount-tray-quit");
const SHOW: PCWSTR = w!(r"Global\ufsmount-tray-show");

// Owner and group are set so that SecurityDescriptor accepts the SDDL.
const MUTEX_SDDL: &str = "O:BAG:SYD:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100000;;;AU)";
const QUIT_SDDL: &str = "O:BAG:SYD:(A;;GA;;;SY)(A;;GA;;;BA)";
const SHOW_SDDL: &str = "O:BAG:SYD:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100002;;;AU)";

/// The objects of the running tray. Dropping it ends "running".
pub struct Instance {
    mutex: HANDLE,
    pub quit: HANDLE,
    pub show: HANDLE,
}

// SAFETY: kernel object handles may be used from any thread.
unsafe impl Send for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: the handles are owned.
        unsafe {
            let _ = CloseHandle(self.show);
            let _ = CloseHandle(self.quit);
            let _ = CloseHandle(self.mutex);
        }
    }
}

fn attributes(sd: &SecurityDescriptor) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.as_psd().0,
        bInheritHandle: false.into(),
    }
}

fn is_denied(e: &windows::core::Error) -> bool {
    e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0)
}

impl Instance {
    /// Claim the single instance. `None` if another tray runs.
    pub fn create() -> anyhow::Result<Option<Instance>> {
        let msd = SecurityDescriptor::from_sddl(MUTEX_SDDL)?;
        let qsd = SecurityDescriptor::from_sddl(QUIT_SDDL)?;
        let ssd = SecurityDescriptor::from_sddl(SHOW_SDDL)?;
        // SAFETY: the attributes and descriptors live through each call.
        unsafe {
            let mutex = match CreateMutexW(Some(&attributes(&msd)), false, MUTEX) {
                Ok(h) if GetLastError() == ERROR_ALREADY_EXISTS => {
                    let _ = CloseHandle(h);
                    return Ok(None);
                }
                Ok(h) => h,
                Err(e) if is_denied(&e) => return Ok(None),
                Err(e) => anyhow::bail!("cannot create the instance mutex: {e}"),
            };
            // An event that exists already was made by someone else, with a DACL of
            // their choice: refuse it.
            let new_event = |sd: &SecurityDescriptor, name: PCWSTR| match CreateEventW(
                Some(&attributes(sd)),
                false,
                false,
                name,
            ) {
                Ok(h) if GetLastError() == ERROR_ALREADY_EXISTS => {
                    let _ = CloseHandle(h);
                    None
                }
                Ok(h) => Some(h),
                Err(_) => None,
            };
            let quit = new_event(&qsd, QUIT);
            let show = new_event(&ssd, SHOW);
            match (quit, show) {
                (Some(quit), Some(show)) => Ok(Some(Instance { mutex, quit, show })),
                (q, s) => {
                    for h in [q, s].into_iter().flatten() {
                        let _ = CloseHandle(h);
                    }
                    let _ = CloseHandle(mutex);
                    anyhow::bail!(
                        "cannot create the tray events; another program may hold their names"
                    )
                }
            }
        }
    }
}

/// Whether a tray runs (in any session).
pub fn running() -> bool {
    // SAFETY: the handle is closed at once.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, MUTEX) } {
        Ok(h) => {
            // SAFETY: owned handle.
            let _ = unsafe { CloseHandle(h) };
            true
        }
        Err(e) => is_denied(&e),
    }
}

fn set_event(name: PCWSTR) -> bool {
    // SAFETY: the handle is closed at once.
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, name) {
            Ok(h) => {
                let ok = SetEvent(h).is_ok();
                let _ = CloseHandle(h);
                ok
            }
            Err(_) => false,
        }
    }
}

/// Ask the running tray to show that it runs.
pub fn signal_show() -> bool {
    set_event(SHOW)
}

/// Wait up to `timeout` until a tray runs.
pub fn wait_running(timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    running()
}

/// Ask the running tray to unmount and exit; wait up to `timeout`.
/// Ok(true) when it is gone (or none ran), Ok(false) on timeout.
pub fn request_quit(timeout: Duration) -> anyhow::Result<bool> {
    if !running() {
        return Ok(true);
    }
    anyhow::ensure!(
        set_event(QUIT),
        "cannot signal the tray (administrator rights needed)"
    );
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if !running() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(!running())
}
