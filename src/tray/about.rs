// SPDX-License-Identifier: AGPL-3.0-or-later
//! The About box: a TaskDialog with the versions and an update check.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};

use windows::Win32::Foundation::{HWND, LPARAM, S_OK, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    TASKDIALOG_NOTIFICATIONS, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TASKDIALOGCONFIG_1,
    TDCBF_OK_BUTTON, TDE_FOOTER, TDF_ALLOW_DIALOG_CANCELLATION, TDF_CALLBACK_TIMER,
    TDF_ENABLE_HYPERLINKS, TDF_USE_HICON_FOOTER, TDIE_ICON_FOOTER, TDM_SET_ELEMENT_TEXT,
    TDM_UPDATE_ICON, TDN_CREATED, TDN_HYPERLINK_CLICKED, TDN_TIMER, TaskDialogIndirect,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, HICON, ICONINFO, SM_CXSMICON, SendMessageW,
    SetForegroundWindow,
};
use windows::core::{HRESULT, HSTRING, PCWSTR, w};

use super::sys::open_unelevated;
use super::update::{self, Version};
use crate::winfs::winfsp_version;

const SOURCE_URL: &str = "https://github.com/poeggi/winfsp-ufs";
const FRAMES: usize = 8;
/// Timer ticks (about 200 ms each) before a check without an answer counts
/// as failed.
const MAX_TICKS: u32 = 120;

type Outcome = Option<Result<Version, String>>;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mark {
    Spinner(usize),
    Newer(Version),
    Current,
    Failed,
}

struct About {
    outcome: Arc<Mutex<Outcome>>,
    mark: Mark,
    footer: String,
    ticks: u32,
    size: i32,
    icon: HICON,
}

impl Drop for About {
    fn drop(&mut self) {
        // SAFETY: our icon, no longer shown.
        let _ = unsafe { DestroyIcon(self.icon) };
    }
}

fn content() -> String {
    let winfsp = match winfsp_version() {
        Ok(v) => format!("WinFsp {v} by Bill Zissimopoulos"),
        Err(_) => "WinFsp by Bill Zissimopoulos (not found)".to_string(),
    };
    format!(
        "Mounts FreeBSD UFS2 disks read-only.\n\n\
         Built using:\n\
         - rufs {}, from fuse-ufs by Benjamin Stuerz and contributors\n\
         - winfsp-rs {} by SnowflakePowered\n\
         - Rust {}\n\
         Runs on: {winfsp}\n\n\
         License: AGPL-3.0-or-later\n\
         Source: <a href=\"{SOURCE_URL}\">github.com/poeggi/winfsp-ufs</a>",
        env!("UFSMOUNT_RUFS"),
        env!("UFSMOUNT_WINFSP_RS"),
        env!("UFSMOUNT_RUST"),
    )
}

/// The footer for a finished check.
fn finished(outcome: Result<Version, String>) -> (Mark, String) {
    match outcome {
        Ok(latest) if latest > update::current() => {
            let (a, b, c) = latest;
            log::info!("update check: {a}.{b}.{c} is available");
            (
                Mark::Newer(latest),
                format!(
                    "ufsmount {a}.{b}.{c} is available. <a href=\"{}\">Download the setup</a> \
                     or see <a href=\"{}\">what's new</a>.",
                    update::setup_url(latest),
                    update::release_url(latest),
                ),
            )
        }
        Ok(_) => (Mark::Current, "ufsmount is up to date.".to_string()),
        Err(e) => {
            log::warn!("update check: {e}");
            (Mark::Failed, "Could not check for updates.".to_string())
        }
    }
}

/// Show the About box; it checks for updates while it is open.
pub fn show(owner: HWND) {
    let outcome = Arc::new(Mutex::new(None));
    let slot = outcome.clone();
    let check = std::thread::Builder::new().spawn(move || {
        let r = update::latest().map_err(|e| format!("{e:#}"));
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(r);
    });
    if let Err(e) = check {
        *outcome.lock().unwrap_or_else(PoisonError::into_inner) = Some(Err(e.to_string()));
    }
    let size = small_icon_size(owner);
    let mut about = About {
        outcome,
        mark: Mark::Spinner(0),
        footer: "Checking for updates...".to_string(),
        ticks: 0,
        size,
        icon: make_icon(Mark::Spinner(0), size),
    };
    let title = HSTRING::from(format!("ufsmount {}", env!("CARGO_PKG_VERSION")));
    let text = HSTRING::from(content());
    let footer = HSTRING::from(about.footer.as_str());
    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner,
        // SAFETY: the module handle of this exe.
        hInstance: unsafe { GetModuleHandleW(None) }.unwrap_or_default().into(),
        dwFlags: TDF_ENABLE_HYPERLINKS
            | TDF_USE_HICON_FOOTER
            | TDF_CALLBACK_TIMER
            | TDF_ALLOW_DIALOG_CANCELLATION,
        dwCommonButtons: TDCBF_OK_BUTTON,
        pszWindowTitle: w!("About ufsmount"),
        // The logo: icon resource 1.
        Anonymous1: TASKDIALOGCONFIG_0 {
            pszMainIcon: PCWSTR(std::ptr::without_provenance(1)),
        },
        pszMainInstruction: PCWSTR(title.as_ptr()),
        pszContent: PCWSTR(text.as_ptr()),
        Anonymous2: TASKDIALOGCONFIG_1 {
            hFooterIcon: about.icon,
        },
        pszFooter: PCWSTR(footer.as_ptr()),
        pfCallback: Some(callback),
        lpCallbackData: &raw mut about as isize,
        ..Default::default()
    };
    // SAFETY: the strings and `about` outlive the modal call.
    if let Err(e) = unsafe { TaskDialogIndirect(&config, None, None, None) } {
        log::error!("about box: {e}");
    }
}

unsafe extern "system" fn callback(
    hwnd: HWND,
    msg: TASKDIALOG_NOTIFICATIONS,
    _wparam: WPARAM,
    lparam: LPARAM,
    data: isize,
) -> HRESULT {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `data` is the About of `show`, alive while the dialog is.
        let about = unsafe { &mut *(data as *mut About) };
        about.notify(hwnd, msg, lparam);
    }));
    if r.is_err() {
        log::error!("about box: internal error");
    }
    S_OK
}

impl About {
    fn notify(&mut self, hwnd: HWND, msg: TASKDIALOG_NOTIFICATIONS, lparam: LPARAM) {
        match msg {
            TDN_CREATED => {
                // SAFETY: a plain call on our dialog.
                let _ = unsafe { SetForegroundWindow(hwnd) };
                self.set_icon(hwnd);
            }
            TDN_TIMER => {
                let Mark::Spinner(frame) = self.mark else {
                    return;
                };
                self.ticks += 1;
                let outcome = self
                    .outcome
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                let outcome = match outcome {
                    None if self.ticks > MAX_TICKS => Some(Err("no answer".to_string())),
                    o => o,
                };
                match outcome {
                    Some(o) => {
                        (self.mark, self.footer) = finished(o);
                        self.set_footer(hwnd);
                        self.set_icon(hwnd);
                    }
                    None => {
                        self.mark = Mark::Spinner((frame + 1) % FRAMES);
                        self.set_icon(hwnd);
                    }
                }
            }
            TDN_HYPERLINK_CLICKED => {
                // SAFETY: lParam is the href of the link, valid during the call.
                let href = unsafe { PCWSTR(lparam.0 as *const u16).to_string() };
                if let Ok(href) = href
                    && self.is_our_link(&href)
                    && let Err(e) = open_unelevated(&href)
                {
                    log::warn!("cannot open {href}: {e:#}");
                }
            }
            _ => {}
        }
    }

    /// Only the links this dialog shows are opened.
    fn is_our_link(&self, href: &str) -> bool {
        href == SOURCE_URL
            || matches!(self.mark, Mark::Newer(v)
                if href == update::setup_url(v) || href == update::release_url(v))
    }

    fn set_footer(&self, hwnd: HWND) {
        let text = HSTRING::from(self.footer.as_str());
        // SAFETY: the text lives through the call.
        unsafe {
            SendMessageW(
                hwnd,
                TDM_SET_ELEMENT_TEXT.0 as u32,
                Some(WPARAM(TDE_FOOTER.0 as usize)),
                Some(LPARAM(text.as_ptr() as isize)),
            );
        }
    }

    /// Draw the footer icon for the dialog's current DPI.
    fn set_icon(&mut self, hwnd: HWND) {
        self.size = small_icon_size(hwnd);
        let icon = make_icon(self.mark, self.size);
        // SAFETY: the dialog shows the new icon; the old one is ours to destroy.
        unsafe {
            SendMessageW(
                hwnd,
                TDM_UPDATE_ICON.0 as u32,
                Some(WPARAM(TDIE_ICON_FOOTER.0 as usize)),
                Some(LPARAM(icon.0 as isize)),
            );
            let _ = DestroyIcon(self.icon);
        }
        self.icon = icon;
    }
}

fn small_icon_size(hwnd: HWND) -> i32 {
    // SAFETY: plain calls.
    unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(hwnd).max(96)) }
}

/// Distance from `p` to the segment `a`-`b`.
fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx * dx + dy * dy;
    let t = if len == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len).clamp(0.0, 1.0)
    };
    (p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy)
}

type Segment = ((f32, f32), (f32, f32));

/// Color (RGB) and opacity of `mark` at `p`, in units of the icon size from its center.
fn sample(mark: Mark, p: (f32, f32)) -> Option<([f32; 3], f32)> {
    const BLUE: [f32; 3] = [0.0, 103.0, 192.0];
    const WHITE: [f32; 3] = [255.0; 3];
    let d = p.0.hypot(p.1);
    let (base, strokes): ([f32; 3], &[Segment]) = match mark {
        Mark::Spinner(frame) => {
            if (d - 0.34).abs() > 0.09 {
                return None;
            }
            let turn = std::f32::consts::TAU;
            let angle = p.1.atan2(p.0) - frame as f32 * turn / FRAMES as f32;
            let lit = angle.rem_euclid(turn) < turn * 0.3;
            return Some((BLUE, if lit { 1.0 } else { 0.25 }));
        }
        Mark::Newer(_) => (
            BLUE,
            &[
                ((0.0, 0.22), (0.0, -0.2)),
                ((-0.17, -0.04), (0.0, -0.21)),
                ((0.0, -0.21), (0.17, -0.04)),
            ],
        ),
        Mark::Current => (
            [22.0, 163.0, 74.0],
            &[
                ((-0.22, 0.02), (-0.06, 0.17)),
                ((-0.06, 0.17), (0.22, -0.14)),
            ],
        ),
        Mark::Failed => (
            [128.0; 3],
            &[((0.0, -0.22), (0.0, 0.05)), ((0.0, 0.2), (0.0, 0.2))],
        ),
    };
    if d > 0.46 {
        return None;
    }
    let on_stroke = strokes
        .iter()
        .any(|&(a, b)| segment_distance(p, a, b) <= 0.075);
    Some((if on_stroke { WHITE } else { base }, 1.0))
}

/// `mark` as BGRA pixels (straight alpha), 4 x 4 samples per pixel.
fn pixels(mark: Mark, size: usize) -> Vec<u32> {
    let s = size as f32;
    let mut out = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let (mut alpha, mut rgb) = (0.0f32, [0.0f32; 3]);
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = (x as f32 + (sx as f32 + 0.5) / 4.0) / s - 0.5;
                    let py = (y as f32 + (sy as f32 + 0.5) / 4.0) / s - 0.5;
                    if let Some((color, a)) = sample(mark, (px, py)) {
                        alpha += a;
                        for (sum, c) in rgb.iter_mut().zip(color) {
                            *sum += c * a;
                        }
                    }
                }
            }
            if alpha == 0.0 {
                out.push(0);
                continue;
            }
            let [r, g, b] = rgb.map(|c| (c / alpha).round() as u32);
            let a = (alpha / 16.0 * 255.0).round() as u32;
            out.push(a << 24 | r << 16 | g << 8 | b);
        }
    }
    out
}

fn make_icon(mark: Mark, size: i32) -> HICON {
    let px = pixels(mark, size as usize);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // Rows of a monochrome bitmap are padded to 16 bits; all zero: no mask.
    let mask_bits = vec![0u8; (size as usize).div_ceil(16) * 2 * size as usize];
    // SAFETY: the DIB holds size x size 32-bit pixels; both bitmaps are
    // deleted after CreateIconIndirect copied them.
    unsafe {
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return HICON::default();
        };
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits.cast::<u32>(), px.len());
        let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr().cast()));
        let info = ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footers() {
        let (a, b, c) = update::current();
        assert_eq!(finished(Ok((a, b, c))).0, Mark::Current);
        assert_eq!(finished(Ok((a, b, c + 1))).0, Mark::Newer((a, b, c + 1)));
        let newer = (a + 1, 0, 0);
        let (mark, footer) = finished(Ok(newer));
        assert_eq!(mark, Mark::Newer(newer));
        assert!(footer.contains(&update::setup_url(newer)));
        assert!(footer.contains(&update::release_url(newer)));
        assert_eq!(finished(Err("x".into())).0, Mark::Failed);
    }

    #[test]
    fn only_shown_links_open() {
        let mut about = About {
            outcome: Arc::default(),
            mark: Mark::Current,
            footer: String::new(),
            ticks: 0,
            size: 16,
            icon: HICON::default(),
        };
        let v = (9, 9, 9);
        assert!(about.is_our_link(SOURCE_URL));
        assert!(!about.is_our_link(&update::setup_url(v)));
        about.mark = Mark::Newer(v);
        assert!(about.is_our_link(&update::setup_url(v)));
        assert!(about.is_our_link(&update::release_url(v)));
        assert!(!about.is_our_link(&update::setup_url((9, 9, 8))));
        assert!(!about.is_our_link("https://example.com/"));
    }

    #[test]
    fn versions_come_from_the_build() {
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"))
            .unwrap()
            .replace("\r\n", "\n");
        let rufs = env!("UFSMOUNT_RUFS");
        assert!(lock.contains(&format!("name = \"rufs\"\nversion = \"{rufs}\"\n")));
        let (winfsp, api) = env!("UFSMOUNT_WINFSP_RS").split_once(" (WinFsp ").unwrap();
        let api = api.trim_end_matches(" API)");
        assert!(lock.contains(&format!(
            "name = \"winfsp\"\nversion = \"{winfsp}+winfsp-{api}\"\n"
        )));
        assert!(env!("UFSMOUNT_RUST").starts_with(|c: char| c.is_ascii_digit()));
        let text = content();
        for v in [rufs, env!("UFSMOUNT_WINFSP_RS"), env!("UFSMOUNT_RUST")] {
            assert!(text.contains(v), "{v}");
        }
    }

    #[test]
    fn marks_are_drawn() {
        for mark in [
            Mark::Spinner(0),
            Mark::Spinner(5),
            Mark::Newer((9, 9, 9)),
            Mark::Current,
            Mark::Failed,
        ] {
            let px = pixels(mark, 20);
            assert_eq!(px.len(), 400);
            assert_eq!(px[0] >> 24, 0, "{mark:?}: corners are clear");
            assert!(px.iter().any(|p| p >> 24 == 255), "{mark:?}: drawn");
        }
        assert_ne!(pixels(Mark::Spinner(0), 20), pixels(Mark::Spinner(1), 20));
    }
}
