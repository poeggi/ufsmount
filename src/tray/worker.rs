// SPDX-License-Identifier: AGPL-3.0-or-later
//! The tray's worker thread: all disk access, probing, mounting and unmounting.
//! The UI thread only sends commands and shows snapshots.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, HANDLE};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE, GetLogicalDrives};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_USERS, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DBT_DEVTYP_HANDLE, DEV_BROADCAST_HANDLE, DEVICE_NOTIFY_WINDOW_HANDLE, HDEVNOTIFY,
    RegisterDeviceNotificationW, UnregisterDeviceNotification,
};
use windows::core::{HSTRING, PWSTR};

use super::settings::{self, FsId};
use super::task;
use crate::device::{Device, Source};
use crate::names::Charset;
use crate::partition::{self, Candidate, human_size};
use crate::security::SecurityDescriptor;
use crate::volume::Volume;
use crate::winfs::Mount;

const MAX_MOUNTS: usize = 8;
const MAX_DISK: u32 = 32;
const DEBOUNCE: Duration = Duration::from_millis(1500);
const WATCH_EVERY: Duration = Duration::from_secs(2);
/// A disk that gave read errors while probing (not ready yet) is probed again.
const RETRY_AFTER: Duration = Duration::from_secs(5);
const RETRIES: u8 = 3;
/// Notes queued while the UI cannot show them; the oldest are dropped.
const MAX_NOTES: usize = 16;

pub enum Cmd {
    /// Probe disks that are not mounted. `manual` also forgets user unmounts.
    Rescan { manual: bool },
    /// A disk arrived; rescan after a short delay.
    Arrived,
    /// A disk went away; check the mounts now.
    Removed,
    /// The user unmounted this mount.
    Unmount(u64),
    /// Mount this file system from the "Not mounted" list on this letter.
    Mount(u64, char),
    /// Move this mount to this letter.
    Move(u64, char),
    /// Mount file systems as they are found, or list them as not mounted.
    AutoMount(bool),
    /// The watcher found the disk of this mount gone.
    Gone(u64),
    /// Start the tray at sign-in, or not.
    SignIn(bool),
    /// Windows asks to remove the disk with this handle: unmount and close
    /// it; the sender is told when done.
    Eject(usize, Sender<()>),
    /// Unmount everything and end; the sender is told when done.
    Quit(Sender<()>),
}

#[derive(Clone)]
pub struct MountView {
    pub id: u64,
    pub letter: String,
    pub label: String,
}

/// A file system that was found but is not mounted.
#[derive(Clone)]
pub struct IdleView {
    pub id: u64,
    pub label: String,
    /// The letter "Mount on" offers.
    pub letter: Option<char>,
}

/// A message for the user: title, text, warning.
pub type Note = (String, String, bool);

#[derive(Clone, Default)]
pub struct Snapshot {
    pub mounts: Vec<MountView>,
    pub idle: Vec<IdleView>,
    /// The drive letters mounts may use, in order, and whether each is free.
    pub letters: Vec<(char, bool)>,
    pub auto_mount: bool,
    pub notes: Vec<Note>,
    /// Shown in the tooltip when no drive is mounted.
    pub status: String,
    /// Whether the task starts the tray at sign-in; None when not known.
    pub sign_in: Option<bool>,
}

/// State shared with the UI thread. `notify` wakes the UI.
pub struct Shared {
    pub snap: Mutex<Snapshot>,
    pub notify: Box<dyn Fn() + Send + Sync>,
    /// The tray window (HWND), which gets the removal requests of mounted disks.
    pub window: usize,
}

/// One file system, identified across scans by disk, offset and UFS id.
type Key = (u32, u64, FsId);

/// A file system that was found but is not mounted. Its disk is not kept open.
struct Idle {
    id: u64,
    key: Key,
    disk: Option<u32>,
    label: String,
}

struct Active {
    id: u64,
    key: Key,
    disk: Option<u32>,
    letter: String,
    label: String,
    mount: Mount,
    dev: Arc<Device>,
    stop: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
    /// The registration for removal requests of the disk (HDEVNOTIFY), or 0.
    removal: usize,
}

impl Active {
    fn stop(mut self) {
        if self.removal != 0 {
            // SAFETY: our registration, ended once.
            let _ =
                unsafe { UnregisterDeviceNotification(HDEVNOTIFY(self.removal as *mut c_void)) };
        }
        self.stop.store(true, Ordering::Relaxed);
        self.mount.stop();
        if let Some(w) = self.watcher.take() {
            let _ = w.join();
        }
        log::info!("unmounted {} from {}", self.label, self.letter);
    }
}

/// Ask for the removal requests of `dev` (Eject, Safely Remove) at the tray
/// window: an open disk handle otherwise blocks them. The registration, or 0.
fn watch_removal(window: usize, dev: &Device) -> usize {
    let Some(handle) = dev.disk_handle() else {
        return 0;
    };
    let filter = DEV_BROADCAST_HANDLE {
        dbch_size: size_of::<DEV_BROADCAST_HANDLE>() as u32,
        dbch_devicetype: DBT_DEVTYP_HANDLE.0,
        dbch_handle: HANDLE(handle as *mut c_void),
        ..Default::default()
    };
    // SAFETY: the filter lives through the call; Active::stop ends the registration.
    match unsafe {
        RegisterDeviceNotificationW(
            HANDLE(window as *mut c_void),
            (&raw const filter).cast::<c_void>(),
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
    } {
        Ok(h) => h.0 as usize,
        Err(e) => {
            log::warn!("{}: no removal requests: {e}", dev.name());
            0
        }
    }
}

/// Drive letters to try, in order: U:, then T: down to D:. Skips letters in
/// `used` (bit 0 = A:) and in `network`.
pub fn letter_order(used: u32, network: &[char]) -> Vec<char> {
    std::iter::once('U')
        .chain(('D'..='T').rev())
        .filter(|&c| used & (1 << (c as u32 - 'A' as u32)) == 0 && !network.contains(&c))
        .collect()
}

/// The letters an automatic mount tries: the one chosen before if it is
/// free, then the first free ones, `n` in all.
fn preferred(chosen: Option<char>, free: &[char], n: usize) -> Vec<char> {
    chosen
        .filter(|c| free.contains(c))
        .into_iter()
        .chain(free.iter().copied().filter(|&c| Some(c) != chosen))
        .take(n)
        .collect()
}

/// Letters of the session user's persistent network mappings
/// (`HKEY_USERS\<sid>\Network`). The elevated tray runs in another logon
/// session: GetLogicalDrives does not show that user's mapped drives.
fn network_letters(user_sid: Option<&str>) -> Vec<char> {
    let mut out = Vec::new();
    let Some(sid) = user_sid else { return out };
    let path = HSTRING::from(format!("{sid}\\Network"));
    let mut key = HKEY::default();
    // SAFETY: the key is closed below; the buffer is passed with its length.
    unsafe {
        if RegOpenKeyExW(HKEY_USERS, &path, Some(0), KEY_READ, &mut key) != ERROR_SUCCESS {
            return out;
        }
        for i in 0.. {
            let mut buf = [0u16; 64];
            let mut len = buf.len() as u32;
            match RegEnumKeyExW(
                key,
                i,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            ) {
                ERROR_SUCCESS => {}
                // A name that long is no drive letter.
                ERROR_MORE_DATA => continue,
                _ => break,
            }
            let name = String::from_utf16_lossy(&buf[..len as usize]);
            if let [c] = name.as_bytes() {
                out.push(c.to_ascii_uppercase() as char);
            }
        }
        let _ = RegCloseKey(key);
    }
    out
}

/// Whether the disk has removable media (card readers, most USB sticks).
/// None if the disk cannot be asked. The query does not spin the disk up.
fn removable_media(n: u32) -> Option<bool> {
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .open(Source::PhysicalDrive(n).path())
        .ok()?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut buf = [0u8; 1024];
    let mut returned = 0u32;
    // SAFETY: both buffers are passed with their sizes and live through the call.
    unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&raw const query).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr().cast()),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
    }
    .ok()?;
    // Read as a byte: the driver fills a BOOLEAN, which need not be 0 or 1.
    let at = std::mem::offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia);
    (returned as usize > at).then(|| buf[at] != 0)
}

struct Worker {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    sd: SecurityDescriptor,
    source: Option<PathBuf>,
    actives: Vec<Active>,
    idle: Vec<Idle>,
    next_id: u64,
    /// File systems the user unmounted: not mounted automatically again.
    unmounted: HashSet<Key>,
    /// Disks probed without finding UFS, and whether their media is removable.
    no_ufs: HashMap<u32, bool>,
    auto_mount: bool,
    /// The letter the user chose for each file system.
    letters: HashMap<FsId, char>,
    notes: Vec<Note>,
    status: String,
    last_warning: Option<String>,
    cap_warned: bool,
    retry: bool,
    retries_left: u8,
    user_sid: Option<String>,
    sign_in: Option<bool>,
}

impl Worker {
    fn new(
        tx: Sender<Cmd>,
        shared: Arc<Shared>,
        sd: SecurityDescriptor,
        source: Option<PathBuf>,
    ) -> Worker {
        Worker {
            tx,
            shared,
            sd,
            source,
            actives: Vec::new(),
            idle: Vec::new(),
            next_id: 1,
            unmounted: HashSet::new(),
            no_ufs: HashMap::new(),
            auto_mount: true,
            letters: HashMap::new(),
            notes: Vec::new(),
            status: String::new(),
            last_warning: None,
            cap_warned: false,
            retry: false,
            retries_left: RETRIES,
            user_sid: None,
            sign_in: None,
        }
    }

    fn note(&mut self, title: &str, text: String, warn: bool) {
        if warn {
            log::warn!("{title}: {text}");
            self.last_warning = Some(format!("{title}: {text}"));
        }
        self.notes.push((title.to_string(), text, warn));
    }

    fn update_status(&mut self) {
        self.status = match (&self.last_warning, self.actives.is_empty()) {
            (Some(w), _) => w.clone(),
            (None, true) => "no UFS disk mounted".to_string(),
            (None, false) => String::new(),
        };
    }

    fn free_letters(&self) -> Vec<char> {
        letter_order(
            self.used_letters(),
            &network_letters(self.user_sid.as_deref()),
        )
    }

    fn publish(&mut self) {
        let mounts = self
            .actives
            .iter()
            .map(|a| MountView {
                id: a.id,
                letter: a.letter.clone(),
                label: a.label.clone(),
            })
            .collect();
        let free = self.free_letters();
        let idle = self
            .idle
            .iter()
            .map(|i| IdleView {
                id: i.id,
                label: i.label.clone(),
                letter: preferred(self.letters.get(&i.key.2).copied(), &free, 1)
                    .first()
                    .copied(),
            })
            .collect();
        let letters = letter_order(0, &[])
            .into_iter()
            .map(|l| (l, free.contains(&l)))
            .collect();
        {
            let mut snap = self.shared.snap.lock().unwrap_or_else(|e| e.into_inner());
            snap.mounts = mounts;
            snap.idle = idle;
            snap.letters = letters;
            snap.auto_mount = self.auto_mount;
            snap.notes.append(&mut self.notes);
            let extra = snap.notes.len().saturating_sub(MAX_NOTES);
            snap.notes.drain(..extra);
            snap.status = self.status.clone();
            snap.sign_in = self.sign_in;
        }
        (self.shared.notify)();
    }

    fn used_letters(&self) -> u32 {
        // SAFETY: no arguments.
        let mut used = unsafe { GetLogicalDrives() };
        for a in &self.actives {
            if let Some(c) = a.letter.chars().next() {
                used |= 1 << (c as u32 - 'A' as u32);
            }
        }
        used
    }

    /// Whether `letter` is free for a mount; else a note says why not.
    fn letter_free(&mut self, letter: char) -> bool {
        let free = self.free_letters().contains(&letter);
        if !free {
            self.note("Cannot mount", format!("{letter}: is in use"), true);
        }
        free
    }

    fn add_idle(&mut self, key: Key, disk: Option<u32>, label: String) {
        if !self.idle.iter().any(|i| i.key == key) {
            let id = self.next_id;
            self.next_id += 1;
            self.idle.push(Idle {
                id,
                key,
                disk,
                label,
            });
        }
    }

    /// Mount one UFS2 file system on `want`, or automatically on its chosen
    /// letter or a free one. Not mounted: listed as idle.
    fn mount(
        &mut self,
        disk: Option<u32>,
        dev: &Arc<Device>,
        cand: &Candidate,
        want: Option<char>,
    ) {
        let Some(facts) = cand.ufs2() else { return };
        let key = (disk.unwrap_or(u32::MAX), cand.start, facts.info.id);
        if self.actives.iter().any(|a| a.key == key) {
            return;
        }
        let name = match disk {
            Some(n) => format!("PhysicalDrive{n}"),
            None => dev.name().to_string(),
        };
        let label = format!(
            "{name} {}  {}",
            cand.id,
            human_size(facts.info.frags * u64::from(facts.info.fsize))
        );
        if want.is_none() && (!self.auto_mount || self.unmounted.contains(&key)) {
            self.add_idle(key, disk, label);
            return;
        }
        if self.actives.len() >= MAX_MOUNTS {
            self.add_idle(key, disk, label.clone());
            if want.is_some() || !std::mem::replace(&mut self.cap_warned, true) {
                let text = format!("{label} is not mounted (limit {MAX_MOUNTS})");
                self.note("Too many file systems", text, true);
            }
            return;
        }
        let vol = match Volume::open(dev.clone(), cand, Charset::Utf8) {
            Ok(v) => Arc::new(v),
            Err(e) => {
                self.add_idle(key, disk, label.clone());
                self.note("Cannot open", format!("{label}: {e:#}"), true);
                return;
            }
        };
        let tries = match want {
            Some(l) => vec![l],
            None => preferred(self.letters.get(&key.2).copied(), &self.free_letters(), 3),
        };
        let mut last_err = None;
        for letter in tries {
            let target = format!("{letter}:");
            match Mount::start(vol.clone(), self.sd.clone(), None, Some(&target), true) {
                Ok(mount) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    let stop = Arc::new(AtomicBool::new(false));
                    let watcher = spawn_watcher(id, dev.clone(), stop.clone(), self.tx.clone());
                    log::info!("mounted {label} on {target}");
                    self.note("Mounted", format!("{target}  {label}"), false);
                    self.idle.retain(|i| i.key != key);
                    self.unmounted.remove(&key);
                    if want.is_some() && self.letters.insert(key.2, letter) != Some(letter) {
                        settings::save_letters(&self.letters);
                    }
                    self.actives.push(Active {
                        id,
                        key,
                        disk,
                        letter: target,
                        label,
                        mount,
                        dev: dev.clone(),
                        stop,
                        watcher: Some(watcher),
                        removal: watch_removal(self.shared.window, dev),
                    });
                    return;
                }
                Err(e) => {
                    log::warn!("{target}: {e:#}");
                    last_err = Some(e);
                }
            }
        }
        let why = last_err.map_or("no free drive letter".to_string(), |e| format!("{e:#}"));
        self.add_idle(key, disk, label.clone());
        self.note("Cannot mount", format!("{label}: {why}"), true);
    }

    /// Mount a file system found before: its disk is probed again, and only
    /// the same file system (offset and UFS id) is mounted.
    fn remount(
        &mut self,
        key: Key,
        disk: Option<u32>,
        dev: Option<Arc<Device>>,
        want: Option<char>,
    ) {
        let dev = match dev {
            Some(d) => d,
            None => {
                let source = match (disk, &self.source) {
                    (Some(n), _) => Source::PhysicalDrive(n),
                    (None, Some(path)) => Source::Image(path.clone()),
                    (None, None) => return,
                };
                match Device::open(&source, None) {
                    Ok(d) => Arc::new(d),
                    Err(e) => {
                        self.idle.retain(|i| i.key != key);
                        self.note("Cannot mount", format!("{e:#}"), true);
                        return;
                    }
                }
            }
        };
        let same = |c: &Candidate| c.start == key.1 && c.ufs2().is_some_and(|f| f.info.id == key.2);
        match partition::probe(&dev).into_iter().find(same) {
            Some(c) => self.mount(disk, &dev, &c, want),
            None => {
                self.idle.retain(|i| i.key != key);
                let text = format!("{}: the file system is gone. Use Rescan.", dev.name());
                self.note("Not found", text, true);
            }
        }
    }

    fn scan_disk(&mut self, n: u32) {
        // The probe lists this disk's file systems again.
        self.idle.retain(|i| i.disk != Some(n));
        let dev = match Device::open(&Source::PhysicalDrive(n), None) {
            Ok(d) => Arc::new(d),
            Err(e) => {
                log::debug!("PhysicalDrive{n}: {e:#}");
                return;
            }
        };
        let cands = match catch_unwind(AssertUnwindSafe(|| partition::probe(&dev))) {
            Ok(c) => c,
            Err(_) => {
                self.note("Probe failed", format!("PhysicalDrive{n}"), true);
                return;
            }
        };
        let ufs: Vec<Candidate> = cands.into_iter().filter(|c| c.ufs2().is_some()).collect();
        if ufs.is_empty() {
            // Read errors (a disk still spinning up) are no proof of "no UFS".
            if dev.take_last_error().is_none() && !dev.is_gone() {
                self.no_ufs.insert(n, removable_media(n).unwrap_or(true));
            } else {
                log::info!("PhysicalDrive{n}: read errors while probing");
                self.retry = true;
            }
            return;
        }
        for c in &ufs {
            if catch_unwind(AssertUnwindSafe(|| self.mount(Some(n), &dev, c, None))).is_err() {
                self.note("Mount failed", format!("PhysicalDrive{n} {}", c.id), true);
            }
        }
    }

    /// Probe the disks. An automatic scan skips disks that have a mount, are
    /// listed as not mounted or are known to hold no UFS; a manual one probes
    /// all (mounts and the list are deduplicated).
    fn scan(&mut self, manual: bool) {
        if let Some(path) = self.source.clone() {
            if self.actives.is_empty() {
                self.scan_image(path);
            }
            return;
        }
        for n in 0..MAX_DISK {
            if !manual
                && (self.no_ufs.contains_key(&n)
                    || self.actives.iter().any(|a| a.disk == Some(n))
                    || (self.idle.iter().any(|i| i.disk == Some(n))
                        && removable_media(n) == Some(false)))
            {
                continue;
            }
            self.scan_disk(n);
        }
    }

    /// After a disk went away: forget what is noted about disk numbers that
    /// are gone, so a disk that comes back on that number is probed and
    /// mounted again.
    fn forget_gone_disks(&mut self) {
        let present = |n: u32| removable_media(n).is_some();
        self.no_ufs.retain(|&n, _| present(n));
        self.unmounted.retain(|k| k.0 == u32::MAX || present(k.0));
        self.idle.retain(|i| i.disk.is_none_or(present));
    }

    fn scan_image(&mut self, path: PathBuf) {
        let dev = match Device::open(&Source::Image(path.clone()), None) {
            Ok(d) => Arc::new(d),
            Err(e) => {
                self.note("Cannot open", format!("{}: {e:#}", path.display()), true);
                return;
            }
        };
        let cands = partition::probe(&dev);
        for c in cands.iter().filter(|c| c.ufs2().is_some()) {
            self.mount(None, &dev, c, None);
        }
    }

    fn take(&mut self, id: u64) -> Option<Active> {
        let i = self.actives.iter().position(|a| a.id == id)?;
        Some(self.actives.remove(i))
    }

    fn check_alive(&mut self) {
        let gone: Vec<u64> = self
            .actives
            .iter()
            .filter(|a| !a.dev.is_alive())
            .map(|a| a.id)
            .collect();
        for id in gone {
            self.gone(id);
        }
    }

    fn gone(&mut self, id: u64) {
        if let Some(a) = self.take(id) {
            let text = format!("{}  {}", a.letter, a.label);
            if let Some(n) = a.disk {
                self.no_ufs.remove(&n);
            }
            a.stop();
            self.note("Disk removed", text, true);
        }
    }

    /// Run `f`. The panic hook logs a panic; here it becomes a note and the
    /// worker goes on.
    fn guard(&mut self, f: impl FnOnce(&mut Worker)) {
        if catch_unwind(AssertUnwindSafe(|| f(self))).is_err() {
            self.note("Internal error", "Details are in tray.log.".into(), true);
        }
    }

    /// Handle one command. Returns the sender of `Cmd::Quit`.
    fn handle(&mut self, cmd: Cmd, rescan_at: &mut Option<Instant>) -> Option<Sender<()>> {
        match cmd {
            Cmd::Rescan { manual } => {
                let before = self.actives.len() + self.idle.len();
                if manual {
                    self.unmounted.clear();
                    self.no_ufs.clear();
                    self.idle.clear();
                    self.last_warning = None;
                    self.cap_warned = false;
                    self.retries_left = RETRIES;
                }
                self.scan(manual);
                let found = self.actives.len() + self.idle.len();
                if manual && found <= before && self.notes.is_empty() {
                    self.note("ufsmount", "No new UFS file system found.".into(), false);
                }
            }
            Cmd::Arrived => {
                // A card reader keeps its disk number when the card is swapped.
                // Fixed disks are not probed again: that would wake a sleeping disk.
                self.no_ufs.retain(|_, removable| !*removable);
                self.retries_left = RETRIES;
                *rescan_at = Some(Instant::now() + DEBOUNCE);
            }
            Cmd::Removed => {
                self.check_alive();
                self.forget_gone_disks();
            }
            Cmd::Unmount(id) => {
                if let Some(a) = self.take(id) {
                    self.unmounted.insert(a.key);
                    self.add_idle(a.key, a.disk, a.label.clone());
                    a.stop();
                }
            }
            Cmd::Mount(id, letter) => {
                let found = self
                    .idle
                    .iter()
                    .find(|i| i.id == id)
                    .map(|i| (i.key, i.disk));
                if let Some((key, disk)) = found
                    && self.letter_free(letter)
                {
                    self.remount(key, disk, None, Some(letter));
                }
            }
            Cmd::Move(id, letter) => {
                // Checked first: a stale menu may offer a letter taken since.
                if self.letter_free(letter)
                    && let Some(a) = self.take(id)
                {
                    let (key, disk, dev) = (a.key, a.disk, a.dev.clone());
                    a.stop();
                    self.remount(key, disk, Some(dev), Some(letter));
                }
            }
            Cmd::AutoMount(on) => {
                self.auto_mount = on;
                settings::set_auto_mount(on);
                if on {
                    let waiting: Vec<(Key, Option<u32>)> = self
                        .idle
                        .iter()
                        .filter(|i| !self.unmounted.contains(&i.key))
                        .map(|i| (i.key, i.disk))
                        .collect();
                    for (key, disk) in waiting {
                        self.remount(key, disk, None, None);
                    }
                }
            }
            Cmd::Gone(id) => self.gone(id),
            Cmd::Eject(handle, done) => {
                // A pending scan would open the disk again before Windows removes it.
                *rescan_at = None;
                let (eject, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.actives)
                    .into_iter()
                    .partition(|a| a.dev.disk_handle() == Some(handle));
                self.actives = keep;
                // The next scan of the disk lists its file systems again.
                let disks: Vec<Option<u32>> = eject.iter().map(|a| a.disk).collect();
                self.idle.retain(|i| !disks.contains(&i.disk));
                for a in eject {
                    log::info!("{}: removal requested", a.dev.name());
                    a.stop();
                }
                let _ = done.send(());
            }
            Cmd::SignIn(on) => match task::install(on) {
                Ok(()) => self.sign_in = task::starts_at_sign_in(),
                Err(e) => self.note("Start at sign-in", format!("{e:#}"), true),
            },
            Cmd::Quit(done) => return Some(done),
        }
        None
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        self.sign_in = task::starts_at_sign_in();
        self.auto_mount = settings::auto_mount();
        self.letters = settings::letters();
        self.guard(|w| w.scan(false));
        if self.actives.is_empty() && self.idle.is_empty() && self.notes.is_empty() {
            self.note(
                "ufsmount",
                "No UFS disk found. Plug one in or use Rescan.".into(),
                false,
            );
        }
        self.update_status();
        self.publish();
        let mut rescan_at: Option<Instant> = None;
        loop {
            if std::mem::take(&mut self.retry) && self.retries_left > 0 {
                self.retries_left -= 1;
                rescan_at = Some(Instant::now() + RETRY_AFTER);
            }
            // The worker holds a sender itself, so the channel never closes;
            // it ends on Cmd::Quit only.
            let cmd = match rescan_at {
                Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(c) => c,
                    Err(RecvTimeoutError::Timeout) => {
                        rescan_at = None;
                        Cmd::Rescan { manual: false }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(c) => c,
                    Err(_) => return,
                },
            };
            let mut quit = None;
            self.guard(|w| quit = w.handle(cmd, &mut rescan_at));
            if let Some(done) = quit {
                for a in std::mem::take(&mut self.actives) {
                    let _ = catch_unwind(AssertUnwindSafe(|| a.stop()));
                }
                let _ = done.send(());
                return;
            }
            self.update_status();
            self.publish();
        }
    }
}

fn spawn_watcher(
    id: u64,
    dev: Arc<Device>,
    stop: Arc<AtomicBool>,
    tx: Sender<Cmd>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let step = Duration::from_millis(200);
        loop {
            let mut waited = Duration::ZERO;
            while waited < WATCH_EVERY {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(step);
                waited += step;
            }
            if !dev.is_alive() {
                let _ = tx.send(Cmd::Gone(id));
                return;
            }
        }
    })
}

/// Start the worker. It scans at once. `source` mounts an image instead of disks.
pub fn spawn(
    source: Option<PathBuf>,
    sd: SecurityDescriptor,
    shared: Arc<Shared>,
) -> (Sender<Cmd>, JoinHandle<()>) {
    let (tx, rx) = channel();
    let own_tx = tx.clone();
    // The worker owns the mounts; it is built on its own thread and never leaves it.
    let handle = std::thread::spawn(move || {
        let mut w = Worker::new(own_tx, shared, sd, source);
        w.user_sid = task::session_user_sid().ok();
        w.run(rx)
    });
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker() -> Worker {
        let shared = Arc::new(Shared {
            snap: Mutex::new(Snapshot::default()),
            notify: Box::new(|| {}),
            window: 0,
        });
        let sd = SecurityDescriptor::read_only_for("S-1-5-18").unwrap();
        Worker::new(channel().0, shared, sd, None)
    }

    #[test]
    fn arrival_forgets_removable_disks_without_ufs() {
        let mut w = worker();
        w.no_ufs.insert(3, true);
        w.no_ufs.insert(4, false);
        let mut at = None;
        assert!(w.handle(Cmd::Arrived, &mut at).is_none());
        assert!(!w.no_ufs.contains_key(&3));
        assert!(w.no_ufs.contains_key(&4));
        assert!(at.is_some());
    }

    #[test]
    fn removal_forgets_gone_disks() {
        let mut w = worker();
        w.no_ufs.insert(999, false);
        assert!(w.handle(Cmd::Removed, &mut None).is_none());
        assert!(w.no_ufs.is_empty());
    }

    #[test]
    fn preferred_letters() {
        let free = ['U', 'T', 'S', 'R'];
        assert_eq!(preferred(None, &free, 3), ['U', 'T', 'S']);
        assert_eq!(preferred(Some('S'), &free, 3), ['S', 'U', 'T']);
        assert_eq!(
            preferred(Some('G'), &free, 3),
            ['U', 'T', 'S'],
            "chosen but used"
        );
        assert_eq!(preferred(Some('R'), &free, 1), ['R']);
        assert!(preferred(Some('R'), &[], 1).is_empty());
    }

    #[test]
    fn idle_file_systems_are_listed() {
        let mut w = worker();
        let id = [7, 7];
        w.add_idle((999, 65536, id), Some(999), "disk 999".into());
        w.add_idle((999, 65536, id), Some(999), "disk 999".into());
        assert_eq!(w.idle.len(), 1, "listed once");
        let chosen = w.free_letters()[1];
        w.letters.insert(id, chosen);
        w.publish();
        {
            let snap = w.shared.snap.lock().unwrap();
            assert_eq!(snap.idle.len(), 1);
            assert_eq!(
                snap.idle[0].letter,
                Some(chosen),
                "the letter chosen before"
            );
            assert!(snap.auto_mount);
            assert_eq!(snap.letters.first(), Some(&('U', snap.letters[0].1)));
        }
        // A disk that is gone leaves the list.
        assert!(w.handle(Cmd::Removed, &mut None).is_none());
        assert!(w.idle.is_empty());
    }

    #[test]
    fn arrival_keeps_the_list() {
        let mut w = worker();
        w.add_idle((999, 0, [1, 1]), Some(999), "a".into());
        let id = w.idle[0].id;
        assert!(w.handle(Cmd::Arrived, &mut None).is_none());
        assert_eq!(w.idle.len(), 1);
        assert_eq!(w.idle[0].id, id, "the menu's ids stay valid");
    }

    #[test]
    fn letters_in_use_or_invalid_are_refused() {
        let mut w = worker();
        w.add_idle((999, 0, [1, 1]), Some(999), "a".into());
        let id = w.idle[0].id;
        for letter in ['C', 'Z', '@'] {
            assert!(w.handle(Cmd::Mount(id, letter), &mut None).is_none());
            assert!(w.handle(Cmd::Move(1234, letter), &mut None).is_none());
        }
        assert_eq!(w.idle.len(), 1, "nothing tried");
        assert_eq!(w.notes.len(), 6);
        assert!(w.notes.iter().all(|n| n.0 == "Cannot mount" && n.2));
    }

    #[test]
    fn eject_answers_without_a_mount() {
        let mut w = worker();
        let (done, answer) = channel();
        let mut at = Some(Instant::now());
        assert!(w.handle(Cmd::Eject(42, done), &mut at).is_none());
        assert!(answer.try_recv().is_ok(), "Windows is told at once");
        assert!(at.is_none(), "no scan while the disk goes");
    }

    #[test]
    fn removable_media_query() {
        assert_eq!(removable_media(999), None);
        // Needs no admin rights: the handle has no access rights.
        assert!(removable_media(0).is_some());
    }

    #[test]
    fn queued_notes_are_capped() {
        let mut w = worker();
        for i in 0..MAX_NOTES + 4 {
            w.note("t", i.to_string(), false);
            w.publish();
        }
        let snap = w.shared.snap.lock().unwrap();
        assert_eq!(snap.notes.len(), MAX_NOTES);
        assert_eq!(snap.notes[0].1, "4");
        assert_eq!(snap.notes[MAX_NOTES - 1].1, (MAX_NOTES + 3).to_string());
    }

    #[test]
    fn panic_becomes_a_note() {
        let mut w = worker();
        w.guard(|_| panic!("test panic"));
        assert_eq!(w.notes.len(), 1);
        assert!(w.notes[0].2);
        let mut ran = false;
        w.guard(|_| ran = true);
        assert!(ran);
        assert_eq!(w.notes.len(), 1);
    }

    #[test]
    fn network_letters_by_sid() {
        assert!(network_letters(None).is_empty());
        assert!(network_letters(Some("S-1-0-0")).is_empty());
        let sid = crate::security::current_user_sid().unwrap();
        assert!(network_letters(Some(&sid)).iter().all(char::is_ascii));
    }

    #[test]
    fn quit_returns_its_sender() {
        let mut w = worker();
        let (done, done_rx) = channel();
        let quit = w.handle(Cmd::Quit(done), &mut None).unwrap();
        quit.send(()).unwrap();
        assert!(done_rx.try_recv().is_ok());
    }

    #[test]
    fn letters() {
        let order = letter_order(0, &[]);
        assert_eq!(order[..3], ['U', 'T', 'S']);
        assert_eq!(*order.last().unwrap(), 'D');
        assert!(!order.contains(&'C'));
        let u = 1 << ('U' as u32 - 'A' as u32);
        assert_eq!(letter_order(u, &['T'])[0], 'S');
    }
}
