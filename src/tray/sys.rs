// SPDX-License-Identifier: AGPL-3.0-or-later
//! Win32 helpers for the tray. The elevated tray takes no paths from
//! environment variables and starts nothing through ShellExecute: both can be
//! steered by any process of the same user.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary,
};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_INFO_CLASS, WTSDomainName, WTSFreeMemory,
    WTSQuerySessionInformationW, WTSUserName,
};
use windows::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_PROCESS_LOGON_FLAGS, CreateProcessW, CreateProcessWithTokenW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_PARENT_PROCESS, PROCESS_CREATE_PROCESS, PROCESS_CREATION_FLAGS,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOEXW, STARTUPINFOW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetShellWindow, GetWindowThreadProcessId, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    MB_SETFOREGROUND, MessageBoxW, SW_SHOWNORMAL,
};
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

fn dir_from(f: impl Fn(Option<&mut [u16]>) -> u32) -> PathBuf {
    let mut buf = vec![0u16; 512];
    let n = f(Some(&mut buf)) as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..n.min(buf.len())]))
}

/// `C:\Windows\System32`, from the API.
pub fn system_dir() -> PathBuf {
    // SAFETY: the buffer is passed with its length.
    dir_from(|b| unsafe { GetSystemDirectoryW(b) })
}

/// `C:\Windows`, from the API.
pub fn windows_dir() -> PathBuf {
    // SAFETY: the buffer is passed with its length.
    dir_from(|b| unsafe { GetWindowsDirectoryW(b) })
}

fn session_string(class: WTS_INFO_CLASS) -> Option<String> {
    let mut buf = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: WTS allocates the buffer; it is freed with WTSFreeMemory.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            class,
            &mut buf,
            &mut len,
        )
        .ok()?;
        let s = buf.to_string().ok();
        WTSFreeMemory(buf.0.cast());
        s.filter(|s| !s.is_empty())
    }
}

/// The user logged on to this session (`DOMAIN\user`). It differs from the
/// process user when another account elevated us.
pub fn session_user() -> Option<String> {
    let user = session_string(WTSUserName)?;
    match session_string(WTSDomainName) {
        Some(domain) => Some(format!(r"{domain}\{user}")),
        None => Some(user),
    }
}

fn quote(s: &str) -> String {
    format!("\"{s}\"")
}

/// Run `exe` (a full path) with `args` and no window, wait, return its exit code.
pub fn run_hidden(exe: &Path, args: &str) -> anyhow::Result<u32> {
    anyhow::ensure!(exe.is_absolute(), "not a full path: {}", exe.display());
    let mut cmd: Vec<u16> = format!("{} {args}", quote(&exe.to_string_lossy()))
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let si = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: all pointers are valid for the call; handles are closed below.
    unsafe {
        CreateProcessW(
            &HSTRING::from(exe.as_os_str()),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
        .map_err(|e| anyhow::anyhow!("cannot run {}: {e}", exe.display()))?;
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 1u32;
        let r = GetExitCodeProcess(pi.hProcess, &mut code);
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        r.map_err(|e| anyhow::anyhow!("no exit code from {}: {e}", exe.display()))?;
        Ok(code)
    }
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Open `path` in Explorer as the non-elevated user: explorer.exe by full path,
/// with the token of the desktop shell. Without the Secondary Logon service the
/// shell is made the parent of the new process instead.
pub fn open_unelevated(path: &str) -> anyhow::Result<()> {
    // SAFETY: plain calls.
    let pid = unsafe {
        let shell = GetShellWindow();
        anyhow::ensure!(!shell.is_invalid(), "no desktop shell");
        let mut pid = 0u32;
        GetWindowThreadProcessId(shell, Some(&mut pid));
        pid
    };
    anyhow::ensure!(pid != 0, "no desktop shell process");
    let exe = windows_dir().join("explorer.exe");
    anyhow::ensure!(exe.is_absolute(), "no Windows folder");
    let cmd = format!("{} {}", quote(&exe.to_string_lossy()), quote(path));
    match start_with_shell_token(pid, &exe, &cmd) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::info!("shell token: {e:#}; trying the shell as parent");
            start_as_child_of(pid, &exe, &cmd)
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn start_as_child_of(pid: u32, exe: &Path, cmd: &str) -> anyhow::Result<()> {
    let mut cmd = wide(cmd);
    // SAFETY: the attribute list lives in `buf` until it is deleted; every
    // handle is owned by a guard.
    unsafe {
        let parent = Handle(OpenProcess(PROCESS_CREATE_PROCESS, false, pid)?);
        let mut size = 0usize;
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
        let mut buf = vec![0u8; size];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr().cast());
        InitializeProcThreadAttributeList(Some(list), 1, None, &mut size)?;
        let result = (|| -> anyhow::Result<()> {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PARENT_PROCESS as usize,
                Some((&parent.0 as *const HANDLE).cast()),
                size_of::<HANDLE>(),
                None,
                None,
            )?;
            let si = STARTUPINFOEXW {
                StartupInfo: STARTUPINFOW {
                    cb: size_of::<STARTUPINFOEXW>() as u32,
                    ..Default::default()
                },
                lpAttributeList: list,
            };
            let mut pi = PROCESS_INFORMATION::default();
            CreateProcessW(
                &HSTRING::from(exe.as_os_str()),
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT,
                None,
                PCWSTR::null(),
                &si.StartupInfo,
                &mut pi,
            )?;
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            Ok(())
        })();
        DeleteProcThreadAttributeList(list);
        result
    }
}

fn start_with_shell_token(pid: u32, exe: &Path, cmd: &str) -> anyhow::Result<()> {
    let mut cmd = wide(cmd);
    // SAFETY: plain Win32 calls; every handle is owned by a guard.
    unsafe {
        let proc = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
        let mut tok = HANDLE::default();
        OpenProcessToken(proc.0, TOKEN_DUPLICATE, &mut tok)?;
        let tok = Handle(tok);
        let mut dup = HANDLE::default();
        DuplicateTokenEx(
            tok.0,
            TOKEN_QUERY
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_DUPLICATE
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup,
        )?;
        let dup = Handle(dup);
        let si = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessWithTokenW(
            dup.0,
            CREATE_PROCESS_LOGON_FLAGS(0),
            &HSTRING::from(exe.as_os_str()),
            Some(PWSTR(cmd.as_mut_ptr())),
            PROCESS_CREATION_FLAGS(0),
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(())
}

/// Start our own exe elevated (UAC prompt), without arguments. Only used by the
/// non-elevated starter.
pub fn relaunch_elevated() -> anyhow::Result<()> {
    let exe = HSTRING::from(std::env::current_exe()?.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: info and the strings it points to live through the call.
    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|e| anyhow::anyhow!("cannot start ufsmount as administrator: {e}"))
}

/// A message box. `owner` may be null.
pub fn message_box(owner: HWND, text: &str, error: bool) {
    let style = MB_OK
        | MB_SETFOREGROUND
        | if error {
            MB_ICONERROR
        } else {
            MB_ICONINFORMATION
        };
    // SAFETY: the strings live through the call.
    unsafe {
        MessageBoxW(
            Some(owner),
            &HSTRING::from(text),
            &HSTRING::from("ufsmount"),
            style,
        );
    }
}
