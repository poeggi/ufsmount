// SPDX-License-Identifier: AGPL-3.0-or-later
//! The UI thread: a hidden top-level window, the notification icon, the menu
//! and the message loop. All disk work happens on the worker thread.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Ioctl::GUID_DEVINTERFACE_DISK;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::INFINITE;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CW_USEDEFAULT, ChangeWindowMessageFilterEx, CreatePopupMenu, CreateWindowExW,
    DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_DEVICEINTERFACE,
    DEV_BROADCAST_DEVICEINTERFACE_W, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetSystemMetrics, HICON, IMAGE_ICON, KillTimer,
    LR_DEFAULTCOLOR, LoadImageW, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, MSGFLT_ALLOW,
    MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage, QS_ALLINPUT,
    RegisterClassW, RegisterDeviceNotificationW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON,
    SetForegroundWindow, SetTimer, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_DEVICECHANGE,
    WM_ENDSESSION, WM_LBUTTONUP, WM_NULL, WM_QUERYENDSESSION, WM_QUIT, WM_TIMER, WNDCLASSW,
    WS_OVERLAPPED,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::instance::Instance;
use super::sys::{message_box, open_unelevated};
use super::worker::{self, Cmd, Shared, Snapshot};
use crate::security::SecurityDescriptor;

const WM_TRAY: u32 = WM_APP + 1;
// NIN_SELECT | NINF_KEY
const NIN_KEYSELECT: u32 = 0x401;
const WM_STATE: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;
const TIMER_ADD_ICON: usize = 1;
const ID_RESCAN: u32 = 10;
const ID_ABOUT: u32 = 11;
const ID_QUIT: u32 = 12;
const ID_MOUNTS: u32 = 100;

const ABOUT: &str = concat!(
    "ufsmount ",
    env!("CARGO_PKG_VERSION"),
    "\n\nMounts FreeBSD UFS2 disks read-only.\n\n",
    "Uses WinFsp by Bill Zissimopoulos, rufs / fuse-ufs by Benjamin Stuerz and\n",
    "contributors, and winfsp-rs by SnowflakePowered and contributors.\n\n",
    "License: AGPL-3.0-or-later. Source: github.com/poeggi/winfsp-ufs"
);

enum Action {
    Open(String),
    Unmount(u64),
}

struct App {
    hwnd: HWND,
    icon: HICON,
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    taskbar_created: u32,
    added: bool,
    quitting: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` on the app state. Never call anything modal inside `f`: modal loops
/// dispatch messages back into the window procedure.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.borrow_mut().as_mut().map(f))
}

fn copy_str(dst: &mut [u16], s: &str) {
    let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

fn tooltip(snap: &Snapshot) -> String {
    if snap.mounts.is_empty() {
        format!("ufsmount: {}", snap.status)
    } else {
        let letters: Vec<&str> = snap.mounts.iter().map(|m| m.letter.as_str()).collect();
        format!("ufsmount: {}", letters.join(", "))
    }
}

fn add_icon(app: &mut App) -> bool {
    let tip = tooltip(&app.shared.snap.lock().unwrap_or_else(|e| e.into_inner()));
    let mut nid = icon_data(app.hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = app.icon;
    copy_str(&mut nid.szTip, &tip);
    // SAFETY: nid is fully initialised.
    let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
    if ok {
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: as above.
        let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) };
    }
    app.added = ok;
    ok
}

fn set_tip(hwnd: HWND, tip: &str) {
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_TIP | NIF_SHOWTIP;
    copy_str(&mut nid.szTip, tip);
    // SAFETY: nid is fully initialised.
    let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
}

fn balloon(hwnd: HWND, title: &str, text: &str, warn: bool) {
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = if warn { NIIF_WARNING } else { NIIF_INFO };
    copy_str(&mut nid.szInfoTitle, title);
    copy_str(&mut nid.szInfo, text);
    // SAFETY: nid is fully initialised.
    let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
}

fn delete_icon(hwnd: HWND) {
    let nid = icon_data(hwnd);
    // SAFETY: nid is fully initialised.
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
}

/// New state from the worker: tooltip, and the queued notes as one balloon.
fn on_state() {
    let Some((hwnd, snap)) = with_app(|a| {
        let mut s = a.shared.snap.lock().unwrap_or_else(|e| e.into_inner());
        let snap = s.clone();
        s.notes.clear();
        (a.hwnd, snap)
    }) else {
        return;
    };
    set_tip(hwnd, &tooltip(&snap));
    if let Some((title, _, _)) = snap.notes.first() {
        let warn = snap.notes.iter().any(|n| n.2);
        let text: Vec<&str> = snap.notes.iter().map(|n| n.1.as_str()).collect();
        let title = if snap.notes.len() == 1 {
            title.as_str()
        } else {
            "ufsmount"
        };
        balloon(hwnd, title, &text.join("\n"), warn);
    }
}

fn send(cmd: Cmd) {
    with_app(|a| {
        let _ = a.tx.send(cmd);
    });
}

/// Unmount everything (wait up to `wait`), then close the window.
fn quit(wait: Duration) {
    let Some(Some((tx, hwnd))) = with_app(|a| {
        if a.quitting {
            None
        } else {
            a.quitting = true;
            Some((a.tx.clone(), a.hwnd))
        }
    }) else {
        return;
    };
    let (done_tx, done_rx) = channel();
    if tx.send(Cmd::Quit(done_tx)).is_ok() {
        let _ = done_rx.recv_timeout(wait);
    }
    // SAFETY: our own window.
    let _ = unsafe { DestroyWindow(hwnd) };
}

fn show_menu() {
    let Some((hwnd, snap)) = with_app(|a| {
        (
            a.hwnd,
            a.shared
                .snap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        )
    }) else {
        return;
    };
    let mut actions: Vec<(u32, Action)> = Vec::new();
    // SAFETY: menu handles are created and destroyed here; strings live through each call.
    let cmd = unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        for (i, m) in snap.mounts.iter().enumerate() {
            let open = ID_MOUNTS + 2 * i as u32;
            if let Ok(sub) = CreatePopupMenu() {
                let _ = AppendMenuW(sub, MF_STRING, open as usize, w!("Open"));
                let _ = AppendMenuW(sub, MF_STRING, open as usize + 1, w!("Unmount"));
                let label = HSTRING::from(format!("{}   {}", m.letter, m.label));
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &label);
            }
            actions.push((open, Action::Open(m.letter.clone())));
            actions.push((open + 1, Action::Unmount(m.id)));
        }
        if snap.mounts.is_empty() {
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, w!("No UFS disk mounted"));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_RESCAN as usize, w!("Rescan disks"));
        let _ = AppendMenuW(menu, MF_STRING, ID_ABOUT as usize, w!("About"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT as usize, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        )
        .0 as u32;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd
    };
    match cmd {
        0 => {}
        ID_RESCAN => send(Cmd::Rescan { manual: true }),
        ID_ABOUT => message_box(hwnd, ABOUT, false),
        ID_QUIT => quit(Duration::from_secs(15)),
        id => match actions.into_iter().find(|a| a.0 == id).map(|a| a.1) {
            Some(Action::Open(letter)) => {
                if let Err(e) = open_unelevated(&format!("{letter}\\")) {
                    log::warn!("open {letter}: {e:#}");
                    balloon(hwnd, "Cannot open", &format!("{letter}: {e:#}"), true);
                }
            }
            Some(Action::Unmount(id)) => send(Cmd::Unmount(id)),
            None => {}
        },
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let taskbar_created = with_app(|a| a.taskbar_created).unwrap_or(0);
    match msg {
        WM_TRAY => {
            let event = (lparam.0 as u32) & 0xffff;
            if event == WM_CONTEXTMENU
                || event == NIN_SELECT
                || event == NIN_KEYSELECT
                || event == WM_LBUTTONUP
            {
                show_menu();
            }
            LRESULT(0)
        }
        WM_STATE => {
            on_state();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_ADD_ICON => {
            if with_app(add_icon) == Some(true) {
                // SAFETY: our own timer.
                let _ = unsafe { KillTimer(Some(hwnd), TIMER_ADD_ICON) };
                on_state();
            }
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            match wparam.0 as u32 {
                DBT_DEVICEARRIVAL => send(Cmd::Arrived),
                DBT_DEVICEREMOVECOMPLETE => send(Cmd::Removed),
                _ => {}
            }
            LRESULT(1)
        }
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                quit(Duration::from_secs(4));
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            delete_icon(hwnd);
            // SAFETY: plain call.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_COMMAND => LRESULT(0),
        m if m != 0 && m == taskbar_created => {
            // Explorer restarted: the icon is gone.
            with_app(add_icon);
            on_state();
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn create_window() -> anyhow::Result<(HWND, HICON)> {
    // SAFETY: standard window class registration and creation.
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = w!("ufsmount-tray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        anyhow::ensure!(RegisterClassW(&wc) != 0, "cannot register the window class");
        let hwnd = CreateWindowExW(
            Default::default(),
            class,
            w!("ufsmount"),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        let icon = LoadImageW(
            Some(hinst.into()),
            PCWSTR(std::ptr::without_provenance(1)), // MAKEINTRESOURCE(1)
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default();
        Ok((hwnd, icon))
    }
}

fn watch_devices(hwnd: HWND) {
    let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
        dbcc_size: size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
        dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
        dbcc_classguid: GUID_DEVINTERFACE_DISK,
        ..Default::default()
    };
    // SAFETY: filter lives through the call; the registration lasts as long as the window.
    if let Err(e) = unsafe {
        RegisterDeviceNotificationW(
            HANDLE(hwnd.0),
            (&filter as *const DEV_BROADCAST_DEVICEINTERFACE_W).cast::<c_void>(),
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
    } {
        log::warn!("no device notifications: {e}");
    }
}

/// Run the tray until Quit, the quit event or session end.
pub fn run(inst: Instance, source: Option<PathBuf>, sd: SecurityDescriptor) -> ExitCode {
    let (hwnd, icon) = match create_window() {
        Ok(x) => x,
        Err(e) => {
            log::error!("cannot create the window: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    // SAFETY: plain calls on our own window.
    let taskbar_created = unsafe {
        let tc = RegisterWindowMessageW(w!("TaskbarCreated"));
        // The elevated window must accept these from the non-elevated Explorer.
        for m in [tc, WM_TRAY] {
            let _ = ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, None);
        }
        tc
    };
    watch_devices(hwnd);
    let hwnd_val = hwnd.0 as usize;
    let shared = Arc::new(Shared {
        snap: Mutex::new(Snapshot::default()),
        notify: Box::new(move || {
            // SAFETY: posting to a window handle; harmless if it is gone.
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(hwnd_val as *mut c_void)),
                    WM_STATE,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        }),
    });
    let (tx, worker): (Sender<Cmd>, JoinHandle<()>) = worker::spawn(source, sd, shared.clone());
    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            hwnd,
            icon,
            tx,
            shared,
            taskbar_created,
            added: false,
            quitting: false,
        })
    });
    if with_app(add_icon) != Some(true) {
        log::warn!("the notification area is not ready; retrying");
        // SAFETY: our own window.
        unsafe { SetTimer(Some(hwnd), TIMER_ADD_ICON, 2000, None) };
    }
    log::info!("tray started");

    let mut msg = MSG::default();
    'outer: loop {
        // SAFETY: the handles are owned by `inst` and live through the loop.
        let r = unsafe {
            MsgWaitForMultipleObjects(Some(&[inst.quit, inst.show]), false, INFINITE, QS_ALLINPUT)
        };
        match r.0 {
            0 => {
                log::info!("quit requested");
                quit(Duration::from_secs(15));
            }
            1 => {
                if let Some(true) = with_app(|a| a.added) {
                    balloon(
                        hwnd,
                        "ufsmount",
                        "ufsmount is running. Use its icon in the notification area.",
                        false,
                    );
                }
            }
            2 => {}
            _ => break,
        }
        // SAFETY: standard message pump.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'outer;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    APP.with(|a| a.borrow_mut().take());
    // A worker stuck in a dead disk must not keep the process alive.
    for _ in 0..50 {
        if worker.is_finished() {
            let _ = worker.join();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    log::info!("tray ended");
    drop(inst);
    ExitCode::SUCCESS
}
