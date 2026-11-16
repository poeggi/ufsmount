// SPDX-License-Identifier: AGPL-3.0-or-later
//! The UI thread: a hidden top-level window, the notification icon, the menu
//! and the message loop. All disk work happens on the worker thread.

use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromWindow};
use windows::Win32::System::Ioctl::{GUID_DEVINTERFACE_CDROM, GUID_DEVINTERFACE_DISK};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForSystem, GetSystemMetricsForDpi, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BROADCAST_QUERY_DENY, CW_USEDEFAULT, ChangeWindowMessageFilterEx, CreatePopupMenu,
    CreateWindowExW, DBT_DEVICEARRIVAL, DBT_DEVICEQUERYREMOVE, DBT_DEVICEREMOVECOMPLETE,
    DBT_DEVTYP_DEVICEINTERFACE, DBT_DEVTYP_HANDLE, DBT_DEVTYP_VOLUME, DBTF_MEDIA,
    DEV_BROADCAST_DEVICEINTERFACE_W, DEV_BROADCAST_HANDLE, DEV_BROADCAST_HDR, DEV_BROADCAST_VOLUME,
    DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowW, GetMessageW, HICON, HMENU, IMAGE_ICON, KillTimer,
    LR_DEFAULTCOLOR, LoadImageW, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MF_UNCHECKED, MSG, MSGFLT_ALLOW, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterDeviceNotificationW, RegisterWindowMessageW, SM_CXSMICON, SetForegroundWindow,
    SetTimer, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
    WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_DPICHANGED,
    WM_ENDSESSION, WM_NULL, WM_QUERYENDSESSION, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::about;
use super::instance::Instance;
use super::sys::open_unelevated;
use super::worker::{self, Cmd, Shared, Snapshot};
use crate::product::product;
use crate::security::SecurityDescriptor;

const WM_TRAY: u32 = WM_APP + 1;
// NIN_SELECT | NINF_KEY
const NIN_KEYSELECT: u32 = 0x401;
const WM_STATE: u32 = WM_APP + 2;
const WM_QUIT_REQUEST: u32 = WM_APP + 3;
const WM_SHOW_REQUEST: u32 = WM_APP + 4;
const ICON_ID: u32 = 1;
const TIMER_ADD_ICON: usize = 1;
const ID_RESCAN: u32 = 10;
const ID_ABOUT: u32 = 11;
const ID_QUIT: u32 = 12;
const ID_MOUNTS: u32 = 100;
/// Longest wait for an unmount before an eject request is refused.
const EJECT_WAIT: Duration = Duration::from_secs(10);

enum Action {
    Open(String),
    Unmount(u64),
    Mount(u64, char),
    Move(u64, char),
    AutoMount(bool),
    SignIn(bool),
}

/// Menu commands from `ID_MOUNTS` on, and what each does.
struct Actions(Vec<Action>);

impl Actions {
    /// The menu id for `action`: `ID_MOUNTS` plus its index.
    fn add(&mut self, action: Action) -> usize {
        self.0.push(action);
        ID_MOUNTS as usize + self.0.len() - 1
    }
}

/// A submenu of drive letters: `current` checked, used ones grayed. The
/// caller appends it to a parent menu, which destroys it.
fn letter_menu(
    actions: &mut Actions,
    letters: &[(char, bool)],
    current: Option<char>,
    action: impl Fn(char) -> Action,
) -> Option<HMENU> {
    // SAFETY: plain menu calls; each string lives through its call.
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for &(letter, free) in letters {
            let text = HSTRING::from(format!("{letter}:"));
            let _ = if Some(letter) == current {
                AppendMenuW(menu, MF_STRING | MF_CHECKED, 0, &text)
            } else if free {
                AppendMenuW(menu, MF_STRING, actions.add(action(letter)), &text)
            } else {
                AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &text)
            };
        }
        Some(menu)
    }
}

struct App {
    hwnd: HWND,
    icon: HICON,
    /// The DPI `icon` was loaded for.
    dpi: u32,
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    taskbar_created: u32,
    added: bool,
    /// The loss of the icon is logged; cleared once the shell takes a call again.
    loss_logged: bool,
    quitting: bool,
    menu_open: bool,
    about_open: bool,
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

/// The drive letters, and the last problem if there is one.
fn tooltip(snap: &Snapshot) -> String {
    let letters: Vec<&str> = snap.mounts.iter().map(|m| m.letter.as_str()).collect();
    let id = product().id;
    match (letters.is_empty(), snap.status.is_empty()) {
        (true, _) => format!("{id}: {}", snap.status),
        (false, true) => format!("{id}: {}", letters.join(", ")),
        (false, false) => format!("{id}: {} - {}", letters.join(", "), snap.status),
    }
}

/// The notification area is on the taskbar of the primary display. Its icons
/// follow that monitor's DPI, which can change without a new sign-in.
fn taskbar_dpi() -> u32 {
    // SAFETY: plain calls; without a taskbar window the primary monitor is used.
    unsafe {
        let taskbar = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).unwrap_or_default();
        let monitor = MonitorFromWindow(taskbar, MONITOR_DEFAULTTOPRIMARY);
        let (mut x, mut y) = (0, 0);
        match GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) {
            Ok(()) if x > 0 => x,
            _ => GetDpiForSystem(),
        }
    }
}

/// The app icon at `size` pixels; the caller owns the handle.
fn load_icon(size: i32) -> HICON {
    // SAFETY: resource 1 is the app icon.
    unsafe {
        let Ok(hinst) = GetModuleHandleW(None) else {
            return HICON::default();
        };
        LoadImageW(
            Some(hinst.into()),
            PCWSTR(std::ptr::without_provenance(1)), // MAKEINTRESOURCE(1)
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default()
    }
}

/// Load the icon again if the taskbar's DPI changed. True if it did.
fn reload_icon() -> bool {
    let dpi = taskbar_dpi();
    if with_app(|a| a.dpi == dpi && !a.icon.is_invalid()).unwrap_or(true) {
        return false;
    }
    // SAFETY: plain call.
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
    let icon = load_icon(size);
    if icon.is_invalid() {
        log::warn!("cannot load the icon at {size} px");
        return false;
    }
    log::info!("icon: {size} px for {dpi} dpi");
    let old = with_app(|a| {
        a.dpi = dpi;
        std::mem::replace(&mut a.icon, icon)
    });
    if let Some(old) = old.filter(|h| !h.is_invalid()) {
        // SAFETY: our own icon; the shell keeps a copy of the one it shows.
        let _ = unsafe { DestroyIcon(old) };
    }
    true
}

/// Add the icon, or update it if it is there already. Shell calls run outside
/// `with_app`: while Shell_NotifyIconW waits for the taskbar, messages sent to
/// this thread are dispatched.
fn add_icon() -> bool {
    reload_icon();
    let Some((hwnd, icon, tip)) = with_app(|a| {
        let tip = tooltip(&a.shared.snap.lock().unwrap_or_else(|e| e.into_inner()));
        (a.hwnd, a.icon, tip)
    }) else {
        return false;
    };
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = icon;
    copy_str(&mut nid.szTip, &tip);
    // SAFETY: nid is fully initialised.
    let mut ok = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
    if !ok {
        // The icon may exist already: TaskbarCreated also comes on a DPI change.
        // SAFETY: as above.
        ok = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.as_bool();
    }
    if ok {
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: as above.
        let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) };
    }
    with_app(|a| a.added = ok);
    ok
}

/// A DPI or display change: show the icon at the new size.
fn on_dpi_change() {
    if !reload_icon() {
        return;
    }
    let Some((hwnd, icon, added)) = with_app(|a| (a.hwnd, a.icon, a.added)) else {
        return;
    };
    if added {
        let mut nid = icon_data(hwnd);
        nid.uFlags = NIF_ICON;
        nid.hIcon = icon;
        // SAFETY: nid is fully initialised.
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }
}

fn set_tip(hwnd: HWND, tip: &str) -> bool {
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_TIP | NIF_SHOWTIP;
    copy_str(&mut nid.szTip, tip);
    // SAFETY: nid is fully initialised.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.as_bool()
}

fn balloon(hwnd: HWND, title: &str, text: &str, warn: bool) -> bool {
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = if warn { NIIF_WARNING } else { NIIF_INFO };
    copy_str(&mut nid.szInfoTitle, title);
    copy_str(&mut nid.szInfo, text);
    // SAFETY: nid is fully initialised.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.as_bool()
}

fn delete_icon(hwnd: HWND) {
    let nid = icon_data(hwnd);
    // SAFETY: nid is fully initialised.
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
}

/// New state from the worker: tooltip, and the queued notes as one balloon.
/// Notes wait until the shell has taken them.
fn on_state() {
    let Some((hwnd, snap, added)) = with_app(|a| {
        let snap = a
            .shared
            .snap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        (a.hwnd, snap, a.added)
    }) else {
        return;
    };
    if !added {
        return;
    }
    let mut ok = set_tip(hwnd, &tooltip(&snap));
    if ok && let Some((title, _, _)) = snap.notes.first() {
        let warn = snap.notes.iter().any(|n| n.2);
        let text: Vec<&str> = snap.notes.iter().map(|n| n.1.as_str()).collect();
        let title = if snap.notes.len() == 1 {
            title.as_str()
        } else {
            product().id
        };
        ok = balloon(hwnd, title, &text.join("\n"), warn);
        if ok {
            with_app(|a| {
                let mut s = a.shared.snap.lock().unwrap_or_else(|e| e.into_inner());
                // A nested call during the shell call may have shown them already.
                if s.notes.starts_with(&snap.notes) {
                    s.notes.drain(..snap.notes.len());
                }
            });
        }
    }
    if ok {
        with_app(|a| a.loss_logged = false);
    } else {
        icon_lost(hwnd);
    }
}

/// A shell call failed: Explorer is gone or busy. Add the icon again until
/// that works; on_state follows.
fn icon_lost(hwnd: HWND) {
    let first = with_app(|a| {
        a.added = false;
        !std::mem::replace(&mut a.loss_logged, true)
    });
    if first == Some(true) {
        log::warn!("the notification icon is gone; adding it again");
    }
    // SAFETY: our own window.
    unsafe { SetTimer(Some(hwnd), TIMER_ADD_ICON, 2000, None) };
}

fn send(cmd: Cmd) {
    with_app(|a| {
        let _ = a.tx.send(cmd);
    });
}

/// Unmount everything (wait up to `wait`), then close the window.
fn quit(wait: Duration) {
    if stop_worker(wait)
        && let Some(hwnd) = with_app(|a| a.hwnd)
    {
        // SAFETY: our own window.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}

/// Have the worker unmount everything and end, and wait up to `wait`.
/// False when that was asked before.
fn stop_worker(wait: Duration) -> bool {
    let Some(Some(tx)) =
        with_app(|a| (!std::mem::replace(&mut a.quitting, true)).then(|| a.tx.clone()))
    else {
        return false;
    };
    let (done_tx, done_rx) = channel();
    if tx.send(Cmd::Quit(done_tx)).is_ok() {
        let _ = done_rx.recv_timeout(wait);
    }
    true
}

/// Unmount the disk with `handle` and close it, so Windows may remove it.
/// True once done; Windows waits for the answer.
fn eject(handle: usize) -> bool {
    let Some(tx) = with_app(|a| a.tx.clone()) else {
        return false;
    };
    let (done_tx, done_rx) = channel();
    if tx.send(Cmd::Eject(handle, done_tx)).is_err() {
        return false;
    }
    if done_rx.recv_timeout(EJECT_WAIT).is_ok() {
        return true;
    }
    // Refused, but the queued eject still unmounts: mount the disk again after it.
    let _ = tx.send(Cmd::Rescan { manual: false });
    false
}

/// The menu at `x`, `y` (the icon's anchor). Not reentrant: one click can
/// bring more than one notification.
fn show_menu(x: i32, y: i32) {
    let Some(Some((hwnd, snap))) = with_app(|a| {
        if a.menu_open {
            return None;
        }
        a.menu_open = true;
        Some((
            a.hwnd,
            a.shared
                .snap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        ))
    }) else {
        return;
    };
    let picked = track_menu(hwnd, &snap, x, y);
    // The About box is modal too; the icon must answer while it is up.
    with_app(|a| a.menu_open = false);
    if let Some((cmd, actions)) = picked {
        act(hwnd, cmd, actions);
    }
}

/// Show the menu; the chosen command (0 for none) and the mount actions.
fn track_menu(hwnd: HWND, snap: &Snapshot, x: i32, y: i32) -> Option<(u32, Vec<Action>)> {
    let mut actions = Actions(Vec::new());
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    // SAFETY: menu handles are created and destroyed here; strings live through each call.
    let cmd = unsafe {
        let menu = CreatePopupMenu().ok()?;
        for m in &snap.mounts {
            if let Ok(sub) = CreatePopupMenu() {
                let open = actions.add(Action::Open(m.letter.clone()));
                let _ = AppendMenuW(sub, MF_STRING, open, w!("Open"));
                let unmount = actions.add(Action::Unmount(m.id));
                let _ = AppendMenuW(sub, MF_STRING, unmount, w!("Unmount"));
                let current = m.letter.chars().next();
                let id = m.id;
                if let Some(letters) = letter_menu(&mut actions, &snap.letters, current, |l| {
                    Action::Move(id, l)
                }) {
                    let _ = AppendMenuW(sub, MF_POPUP, letters.0 as usize, w!("Drive letter"));
                }
                let label = HSTRING::from(format!("{}   {}", m.letter, m.label));
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &label);
            }
        }
        if !snap.idle.is_empty() {
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, w!("Not mounted"));
        } else if snap.mounts.is_empty() {
            let text = HSTRING::from(format!("No {} disks", product().fs));
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &text);
        }
        for i in &snap.idle {
            if let Ok(sub) = CreatePopupMenu() {
                if let Some(l) = i.letter {
                    let text = HSTRING::from(format!("Mount on {l}:"));
                    let _ = AppendMenuW(sub, MF_STRING, actions.add(Action::Mount(i.id, l)), &text);
                }
                let id = i.id;
                if let Some(letters) =
                    letter_menu(&mut actions, &snap.letters, None, |l| Action::Mount(id, l))
                {
                    let _ = AppendMenuW(sub, MF_POPUP, letters.0 as usize, w!("Mount as"));
                }
                let label = HSTRING::from(format!("      {}", i.label));
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &label);
            }
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_RESCAN as usize, w!("Rescan disks"));
        let auto = actions.add(Action::AutoMount(!snap.auto_mount));
        let _ = AppendMenuW(
            menu,
            MF_STRING | check(snap.auto_mount),
            auto,
            w!("Mount automatically"),
        );
        if let Some(on) = snap.sign_in {
            let sign_in = actions.add(Action::SignIn(!on));
            let _ = AppendMenuW(menu, MF_STRING | check(on), sign_in, w!("Start at sign-in"));
        }
        let _ = AppendMenuW(menu, MF_STRING, ID_ABOUT as usize, w!("About"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT as usize, w!("Quit"));
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            x,
            y,
            Some(0),
            hwnd,
            None,
        )
        .0 as u32;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd
    };
    Some((cmd, actions.0))
}

fn act(hwnd: HWND, cmd: u32, actions: Vec<Action>) {
    match cmd {
        0 => {}
        ID_RESCAN => send(Cmd::Rescan { manual: true }),
        ID_ABOUT => {
            if with_app(|a| std::mem::replace(&mut a.about_open, true)) == Some(false) {
                // A panic must not unwind into the window procedure.
                if catch_unwind(AssertUnwindSafe(|| about::show(hwnd))).is_err() {
                    log::error!("about box: internal error");
                }
                with_app(|a| a.about_open = false);
            }
        }
        ID_QUIT => quit(Duration::from_secs(15)),
        id => match id
            .checked_sub(ID_MOUNTS)
            .and_then(|i| actions.into_iter().nth(i as usize))
        {
            Some(Action::Open(letter)) => {
                if let Err(e) = open_unelevated(&format!("{letter}\\")) {
                    log::warn!("open {letter}: {e:#}");
                    balloon(hwnd, "Cannot open", &format!("{letter}: {e:#}"), true);
                }
            }
            Some(Action::Unmount(id)) => send(Cmd::Unmount(id)),
            Some(Action::Mount(id, letter)) => send(Cmd::Mount(id, letter)),
            Some(Action::Move(id, letter)) => send(Cmd::Move(id, letter)),
            Some(Action::AutoMount(on)) => send(Cmd::AutoMount(on)),
            Some(Action::SignIn(on)) => send(Cmd::SignIn(on)),
            None => {}
        },
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let taskbar_created = with_app(|a| a.taskbar_created).unwrap_or(0);
    match msg {
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event in the low word of lparam, the
            // anchor point in wparam.
            let event = (lparam.0 as u32) & 0xffff;
            if event == WM_CONTEXTMENU || event == NIN_SELECT || event == NIN_KEYSELECT {
                let x = (wparam.0 & 0xffff) as u16 as i16 as i32;
                let y = ((wparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
                show_menu(x, y);
            }
            LRESULT(0)
        }
        WM_QUIT_REQUEST => {
            log::info!("quit requested");
            quit(Duration::from_secs(15));
            LRESULT(0)
        }
        WM_SHOW_REQUEST => {
            if with_app(|a| a.added) == Some(true) {
                let id = product().id;
                balloon(
                    hwnd,
                    id,
                    &format!("{id} is running. Use its icon in the notification area."),
                    false,
                );
            }
            LRESULT(0)
        }
        WM_STATE => {
            on_state();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_ADD_ICON => {
            if add_icon() {
                // SAFETY: our own timer.
                let _ = unsafe { KillTimer(Some(hwnd), TIMER_ADD_ICON) };
                on_state();
            }
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            let event = wparam.0 as u32;
            if event == DBT_DEVICEQUERYREMOVE && lparam.0 != 0 {
                // SAFETY: for this event lparam points to a DEV_BROADCAST_HDR.
                let kind = unsafe { (*(lparam.0 as *const DEV_BROADCAST_HDR)).dbch_devicetype };
                if kind == DBT_DEVTYP_HANDLE {
                    // SAFETY: a handle event: lparam is a DEV_BROADCAST_HANDLE.
                    let handle =
                        unsafe { (*(lparam.0 as *const DEV_BROADCAST_HANDLE)).dbch_handle };
                    return if eject(handle.0 as usize) {
                        LRESULT(1)
                    } else {
                        LRESULT(BROADCAST_QUERY_DENY as isize)
                    };
                }
            }
            if (event == DBT_DEVICEARRIVAL || event == DBT_DEVICEREMOVECOMPLETE) && lparam.0 != 0 {
                // SAFETY: for these events lparam points to a DEV_BROADCAST_HDR.
                let kind = unsafe { (*(lparam.0 as *const DEV_BROADCAST_HDR)).dbch_devicetype };
                // Disks come and go as disk interfaces. Volume arrivals also mean new
                // media; volume removals come from our own unmounts too.
                if event == DBT_DEVICEARRIVAL
                    && (kind == DBT_DEVTYP_DEVICEINTERFACE || kind == DBT_DEVTYP_VOLUME)
                {
                    // SAFETY: for a volume, lparam points to a DEV_BROADCAST_VOLUME.
                    let media = kind == DBT_DEVTYP_VOLUME
                        && unsafe { (*(lparam.0 as *const DEV_BROADCAST_VOLUME)).dbcv_flags }.0
                            & DBTF_MEDIA.0
                            != 0;
                    send(Cmd::Arrived { media });
                } else if event == DBT_DEVICEREMOVECOMPLETE && kind == DBT_DEVTYP_DEVICEINTERFACE {
                    send(Cmd::Removed);
                }
            }
            LRESULT(1)
        }
        WM_DPICHANGED | WM_DISPLAYCHANGE => {
            on_dpi_change();
            LRESULT(0)
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
        m if m != 0 && m == taskbar_created => {
            // Explorer restarted (the icon is gone) or the DPI changed.
            if add_icon() {
                on_state();
            } else {
                // SAFETY: our own window.
                unsafe { SetTimer(Some(hwnd), TIMER_ADD_ICON, 2000, None) };
            }
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn create_window() -> anyhow::Result<HWND> {
    // SAFETY: standard window class registration and creation.
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = HSTRING::from(format!("{}-tray", product().id));
        let title = HSTRING::from(product().id);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        anyhow::ensure!(RegisterClassW(&wc) != 0, "cannot register the window class");
        let hwnd = CreateWindowExW(
            Default::default(),
            &class,
            &title,
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
        Ok(hwnd)
    }
}

/// Disks and optical drives that come and go. A disc put into a drive comes
/// as a volume arrival, which every top-level window gets.
fn watch_devices(hwnd: HWND) {
    for class in [GUID_DEVINTERFACE_DISK, GUID_DEVINTERFACE_CDROM] {
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
            dbcc_classguid: class,
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
}

/// Run the tray until Quit, the quit event or session end.
pub fn run(inst: Instance, source: Option<PathBuf>, sd: SecurityDescriptor) -> ExitCode {
    let hwnd = match create_window() {
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
        window: hwnd_val,
    });
    let (tx, worker): (Sender<Cmd>, JoinHandle<()>) = worker::spawn(source, sd, shared.clone());
    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            hwnd,
            icon: HICON::default(),
            dpi: 0,
            tx,
            shared,
            taskbar_created,
            added: false,
            loss_logged: false,
            quitting: false,
            menu_open: false,
            about_open: false,
        })
    });
    if !add_icon() {
        log::warn!("the notification area is not ready; retrying");
        // SAFETY: our own window.
        unsafe { SetTimer(Some(hwnd), TIMER_ADD_ICON, 2000, None) };
    }
    log::info!("tray started");

    // The quit and show events are watched on their own thread, so that they
    // also work while a menu or message box runs its modal loop.
    // SAFETY: an unnamed event, closed below.
    let stop = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
        Ok(h) => h,
        Err(e) => {
            log::error!("cannot create an event: {e}");
            return ExitCode::FAILURE;
        }
    };
    let waiter = spawn_event_waiter(hwnd, &inst, stop);

    let mut msg = MSG::default();
    // SAFETY: standard message loop; 0 is WM_QUIT, -1 an error.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Not quit through the menu or the event: unmount now.
    stop_worker(Duration::from_secs(15));
    // SAFETY: our own event; the waiter ends on it before the handles close.
    unsafe {
        let _ = SetEvent(stop);
    }
    let _ = waiter.join();
    // SAFETY: owned handle, no longer waited on.
    let _ = unsafe { CloseHandle(stop) };
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

/// Wait on the instance's quit and show events and on `stop`; post the first
/// two to the window.
fn spawn_event_waiter(hwnd: HWND, inst: &Instance, stop: HANDLE) -> JoinHandle<()> {
    let raw = [inst.quit.0 as usize, inst.show.0 as usize, stop.0 as usize];
    let hwnd = hwnd.0 as usize;
    std::thread::spawn(move || {
        let handles = raw.map(|h| HANDLE(h as *mut c_void));
        loop {
            // SAFETY: the handles stay open until this thread has ended.
            let r = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            let msg = match r.0 {
                0 => WM_QUIT_REQUEST,
                1 => WM_SHOW_REQUEST,
                _ => return,
            };
            // SAFETY: posting to our window; harmless if it is gone.
            let _ =
                unsafe { PostMessageW(Some(HWND(hwnd as *mut c_void)), msg, WPARAM(0), LPARAM(0)) };
            if msg == WM_QUIT_REQUEST {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tray::worker::MountView;

    #[test]
    fn tooltips() {
        let mut snap = Snapshot {
            status: "no DEMO disk mounted".into(),
            ..Default::default()
        };
        assert_eq!(tooltip(&snap), "demo: no DEMO disk mounted");
        snap.status.clear();
        snap.mounts.push(MountView {
            id: 1,
            letter: "U:".into(),
            label: "x".into(),
        });
        assert_eq!(tooltip(&snap), "demo: U:");
        snap.status = "Cannot mount: y".into();
        assert_eq!(tooltip(&snap), "demo: U: - Cannot mount: y");
    }
}
