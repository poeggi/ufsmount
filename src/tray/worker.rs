// SPDX-License-Identifier: AGPL-3.0-or-later
//! The tray's worker thread: all disk access, probing, mounting and unmounting.
//! The UI thread only sends commands and shows snapshots.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Storage::FileSystem::GetLogicalDrives;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
};
use windows::core::{PWSTR, w};

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

pub enum Cmd {
    /// Probe disks that are not mounted. `manual` also forgets user unmounts.
    Rescan { manual: bool },
    /// A disk arrived; rescan after a short delay.
    Arrived,
    /// A disk went away; check the mounts now.
    Removed,
    /// The user unmounted this mount.
    Unmount(u64),
    /// The watcher found the disk of this mount gone.
    Gone(u64),
    /// Unmount everything and end; the sender is told when done.
    Quit(Sender<()>),
}

#[derive(Clone)]
pub struct MountView {
    pub id: u64,
    pub letter: String,
    pub label: String,
}

/// A message for the user: title, text, warning.
pub type Note = (String, String, bool);

#[derive(Clone, Default)]
pub struct Snapshot {
    pub mounts: Vec<MountView>,
    pub notes: Vec<Note>,
    /// Shown in the tooltip when no drive is mounted.
    pub status: String,
}

/// State shared with the UI thread. `notify` wakes the UI.
pub struct Shared {
    pub snap: Mutex<Snapshot>,
    pub notify: Box<dyn Fn() + Send + Sync>,
}

/// One file system, identified across scans by disk, offset and UFS id.
type Key = (u32, u64, [u32; 2]);

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
}

impl Active {
    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.mount.stop();
        if let Some(w) = self.watcher.take() {
            let _ = w.join();
        }
        log::info!("unmounted {} from {}", self.label, self.letter);
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

/// Letters of the user's network mappings (`HKCU\Network`).
fn network_letters() -> Vec<char> {
    let mut out = Vec::new();
    let mut key = HKEY::default();
    // SAFETY: the key is closed below; the buffer is passed with its length.
    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Network"),
            Some(0),
            KEY_READ,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return out;
        }
        for i in 0.. {
            let mut buf = [0u16; 64];
            let mut len = buf.len() as u32;
            if RegEnumKeyExW(
                key,
                i,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            ) != ERROR_SUCCESS
            {
                break;
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

struct Worker {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    sd: SecurityDescriptor,
    source: Option<PathBuf>,
    actives: Vec<Active>,
    next_id: u64,
    unmounted: HashSet<Key>,
    no_ufs: HashSet<u32>,
    notes: Vec<Note>,
    status: String,
    last_warning: Option<String>,
    cap_warned: bool,
    retry: bool,
    retries_left: u8,
}

impl Worker {
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
        {
            let mut snap = self.shared.snap.lock().unwrap_or_else(|e| e.into_inner());
            snap.mounts = mounts;
            snap.notes.append(&mut self.notes);
            snap.status = self.status.clone();
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

    /// Mount one UFS2 file system on the first free letter that works.
    fn mount(&mut self, disk: Option<u32>, dev: &Arc<Device>, cand: &Candidate) {
        let Some(facts) = cand.ufs2() else { return };
        let key = (disk.unwrap_or(u32::MAX), cand.start, facts.info.id);
        if self.unmounted.contains(&key) || self.actives.iter().any(|a| a.key == key) {
            return;
        }
        if self.actives.len() >= MAX_MOUNTS {
            if std::mem::replace(&mut self.cap_warned, true) {
                return;
            }
            self.note(
                "Too many file systems",
                format!(
                    "{} {} is not mounted (limit {MAX_MOUNTS})",
                    dev.name(),
                    cand.id
                ),
                true,
            );
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
        let vol = match Volume::open(dev.clone(), cand, Charset::Utf8) {
            Ok(v) => Arc::new(v),
            Err(e) => {
                self.note("Cannot open", format!("{label}: {e:#}"), true);
                return;
            }
        };
        let network = network_letters();
        let mut last_err = None;
        for letter in letter_order(self.used_letters(), &network)
            .into_iter()
            .take(3)
        {
            let target = format!("{letter}:");
            match Mount::start(vol.clone(), self.sd.clone(), None, Some(&target), true) {
                Ok(mount) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    let stop = Arc::new(AtomicBool::new(false));
                    let watcher = spawn_watcher(id, dev.clone(), stop.clone(), self.tx.clone());
                    log::info!("mounted {label} on {target}");
                    self.note("Mounted", format!("{target}  {label}"), false);
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
        self.note("Cannot mount", format!("{label}: {why}"), true);
    }

    fn scan_disk(&mut self, n: u32) {
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
                self.no_ufs.insert(n);
            } else {
                log::info!("PhysicalDrive{n}: read errors while probing");
                self.retry = true;
            }
            return;
        }
        for c in &ufs {
            if catch_unwind(AssertUnwindSafe(|| self.mount(Some(n), &dev, c))).is_err() {
                self.note("Mount failed", format!("PhysicalDrive{n} {}", c.id), true);
            }
        }
    }

    /// Probe the disks. An automatic scan skips disks that have a mount or are
    /// known to hold no UFS; a manual one probes all (mounts are deduplicated).
    fn scan(&mut self, manual: bool) {
        if let Some(path) = self.source.clone() {
            if self.actives.is_empty() {
                self.scan_image(path);
            }
            return;
        }
        for n in 0..MAX_DISK {
            if !manual
                && (self.no_ufs.contains(&n) || self.actives.iter().any(|a| a.disk == Some(n)))
            {
                continue;
            }
            self.scan_disk(n);
        }
    }

    /// After a disk went away: forget what was noted about disk numbers that are
    /// gone, so the disk is probed (and mounted) again when it comes back.
    fn forget_gone_disks(&mut self) {
        let present = |n: u32| Device::open(&Source::PhysicalDrive(n), None).is_ok();
        self.no_ufs.retain(|&n| present(n));
        self.unmounted.retain(|k| k.0 == u32::MAX || present(k.0));
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
            self.mount(None, &dev, c);
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

    fn run(mut self, rx: Receiver<Cmd>) {
        self.scan(false);
        if self.actives.is_empty() && self.notes.is_empty() {
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
            match cmd {
                Cmd::Rescan { manual } => {
                    if manual {
                        self.unmounted.clear();
                        self.no_ufs.clear();
                        self.last_warning = None;
                        self.cap_warned = false;
                        self.retries_left = RETRIES;
                    }
                    let before = self.actives.len();
                    self.scan(manual);
                    if manual && self.actives.len() == before && self.notes.is_empty() {
                        self.note("ufsmount", "No new UFS file system found.".into(), false);
                    }
                }
                Cmd::Arrived => {
                    self.retries_left = RETRIES;
                    rescan_at = Some(Instant::now() + DEBOUNCE);
                }
                Cmd::Removed => {
                    self.check_alive();
                    self.forget_gone_disks();
                }
                Cmd::Unmount(id) => {
                    if let Some(a) = self.take(id) {
                        self.unmounted.insert(a.key);
                        a.stop();
                    }
                }
                Cmd::Gone(id) => self.gone(id),
                Cmd::Quit(done) => {
                    for a in std::mem::take(&mut self.actives) {
                        a.stop();
                    }
                    let _ = done.send(());
                    return;
                }
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
        Worker {
            tx: own_tx,
            shared,
            sd,
            source,
            actives: Vec::new(),
            next_id: 1,
            unmounted: HashSet::new(),
            no_ufs: HashSet::new(),
            notes: Vec::new(),
            status: String::new(),
            last_warning: None,
            cap_warned: false,
            retry: false,
            retries_left: RETRIES,
        }
        .run(rx)
    });
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use super::letter_order;

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
