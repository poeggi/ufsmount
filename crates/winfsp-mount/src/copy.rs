// SPDX-License-Identifier: AGPL-3.0-or-later
//! `ls` and `cp` without WinFsp.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, FileTimes};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::fs::{Kind, NodeId};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::FSCTL_SET_SPARSE;

use crate::names;
use crate::volume::{Attr, Node, Volume};

const EPOCH_1601: u64 = 116_444_736_000_000_000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
/// Directory levels `cp` descends at most. FreeBSD paths are at most 1024
/// bytes (PATH_MAX), so a real tree is not deeper.
const MAX_DEPTH: usize = 512;
/// Entries `cp` holds at once: each level keeps the rest of its directory
/// while it copies the one below.
const MAX_HELD: usize = 4_000_000;

pub fn systemtime(filetime: u64) -> SystemTime {
    if filetime >= EPOCH_1601 {
        UNIX_EPOCH + Duration::from_nanos((filetime - EPOCH_1601).saturating_mul(100))
    } else {
        UNIX_EPOCH - Duration::from_nanos((EPOCH_1601 - filetime).saturating_mul(100))
    }
}

/// `YYYY-MM-DD HH:MM` in UTC.
pub fn format_time(filetime: u64) -> String {
    let secs = (filetime as i64 - EPOCH_1601 as i64).div_euclid(10_000_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

fn line(attr: &Attr, name: &str, target: Option<&[u8]>) -> String {
    let kind = if attr.is_dir {
        'd'
    } else if attr.is_symlink {
        'l'
    } else {
        '-'
    };
    let mut s = format!(
        "{kind} {:>14} {}  {name}",
        attr.size,
        format_time(attr.write)
    );
    if let Some(t) = target {
        s += &format!(" -> {}", names::display(t));
    }
    s
}

/// Print a directory listing, or one line for a file.
pub fn ls(vol: &Volume, path: &str) -> anyhow::Result<()> {
    let node = vol
        .resolve_path(path)
        .map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let mut out = BufWriter::new(io::stdout().lock());
    let result = (|| -> io::Result<()> {
        if node.kind != Kind::Dir {
            let attr = vol.attr(&node).map_err(io::Error::other)?;
            writeln!(out, "{}", line(&attr, path, None))?;
            return out.flush();
        }
        let table = vol.dir(node.id).map_err(io::Error::other)?;
        for e in &table.entries {
            let child = Node {
                id: e.id,
                kind: e.kind,
                parent: node.id,
            };
            let attr = vol.attr_or_default(&child, &e.name);
            let target = if e.kind == Kind::Symlink {
                vol.readlink(e.id).ok()
            } else {
                None
            };
            writeln!(
                out,
                "{}",
                line(&attr, &names::display(&e.raw), target.as_deref())
            )?;
        }
        if table.hidden > 0 {
            log::warn!(
                "{} entries not shown: their names are empty or clash with other entries",
                table.hidden
            );
        }
        out.flush()
    })();
    match result {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        r => Ok(r?),
    }
}

#[derive(Default)]
pub struct Stats {
    pub files: u64,
    pub bytes: u64,
    pub dirs: u64,
    pub skipped: u64,
    pub failed: u64,
    /// The disk went away during the copy.
    pub aborted: bool,
    held: usize,
}

/// `\\?\` form of an absolute path: Win32 name parsing (DOS device names
/// such as `nul`, trailing dots) does not apply to the names below it.
fn verbatim(path: &Path) -> PathBuf {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let s = String::from_utf16_lossy(&wide);
    if s.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    let prefixed: Vec<u16> = match s.strip_prefix(r"\\") {
        Some(_) => r"\\?\UNC\"
            .encode_utf16()
            .chain(wide[2..].iter().copied())
            .collect(),
        None => r"\\?\".encode_utf16().chain(wide).collect(),
    };
    PathBuf::from(OsString::from_wide(&prefixed))
}

/// Copy a file or a tree. Unreadable files are logged, removed and counted.
pub fn cp(vol: &Volume, from: &str, to: &Path) -> anyhow::Result<Stats> {
    let node = vol
        .resolve_path(from)
        .map_err(|e| anyhow::anyhow!("{from}: {e}"))?;
    // Only the parent goes through Win32 path parsing: it would turn a last
    // component such as `nul` into the device \\.\nul.
    let mut dest = match (to.parent(), to.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() {
                std::env::current_dir()?
            } else {
                std::path::absolute(parent)?
            };
            verbatim(&parent).join(name)
        }
        _ => verbatim(&std::path::absolute(to)?),
    };
    if dest.is_dir() {
        let name = from
            .rsplit(['/', '\\'])
            .find(|c| !c.is_empty() && *c != "." && *c != "..")
            .unwrap_or("root");
        dest.push(OsString::from_wide(&names::to_windows(
            name.as_bytes(),
            vol.charset,
        )));
    }
    let mut stats = Stats::default();
    // One stack frame per directory level: MAX_DEPTH levels need more than
    // the main thread's 1 MiB in a debug build.
    std::thread::scope(|s| -> anyhow::Result<()> {
        let worker = std::thread::Builder::new()
            .stack_size(16 << 20)
            .spawn_scoped(s, || {
                copy_node(vol, &node, &dest, 0, &mut HashSet::new(), &mut stats)
            })?;
        if let Err(panic) = worker.join() {
            std::panic::resume_unwind(panic);
        }
        Ok(())
    })?;
    Ok(stats)
}

/// Whether a mapped name joins a path as exactly one new component.
fn is_plain_name(name: &[u16]) -> bool {
    let dot = u16::from(b'.');
    !name.is_empty()
        && name != [dot]
        && name != [dot, dot]
        && !name
            .iter()
            .any(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
}

/// `dirs` holds the directories copied so far. One met twice is a loop on a
/// damaged disk, or a directory hard link: it is copied once.
fn copy_node(
    vol: &Volume,
    node: &Node,
    dest: &Path,
    depth: usize,
    dirs: &mut HashSet<NodeId>,
    stats: &mut Stats,
) {
    if vol.device().is_gone() {
        stats.aborted = true;
        return;
    }
    match node.kind {
        Kind::Dir => {
            if !dirs.insert(node.id) {
                log::error!(
                    "{}: directory inode {} is linked more than once, not copied",
                    dest.display(),
                    node.id
                );
                stats.failed += 1;
                return;
            }
            if depth >= MAX_DEPTH {
                log::error!(
                    "{}: more than {MAX_DEPTH} directory levels, not copied",
                    dest.display()
                );
                stats.failed += 1;
                return;
            }
            if let Err(e) = fs::create_dir_all(dest) {
                log::error!("{}: {e}", dest.display());
                stats.failed += 1;
                return;
            }
            stats.dirs += 1;
            let Some(children) = children(vol, node, dest, stats) else {
                return;
            };
            let n = children.len();
            if stats.held + n > MAX_HELD {
                log::error!(
                    "{}: more than {MAX_HELD} entries on this path, not copied",
                    dest.display()
                );
                stats.failed += 1;
                return;
            }
            stats.held += n;
            for (name, child) in children {
                if stats.aborted {
                    return;
                }
                copy_node(vol, &child, &dest.join(name), depth + 1, dirs, stats);
            }
            stats.held -= n;
            set_dir_times(vol, node, dest);
        }
        Kind::File => match copy_file(vol, node, dest) {
            Ok(n) => {
                stats.files += 1;
                stats.bytes += n;
            }
            Err(e) => {
                log::error!("{}: {e}", dest.display());
                let _ = fs::remove_file(dest);
                stats.failed += 1;
                stats.aborted |= vol.device().is_gone();
            }
        },
        Kind::Symlink => {
            let target = vol
                .readlink(node.id)
                .map(|t| names::display(&t))
                .unwrap_or_default();
            log::warn!("{}: symlink to {target}, skipped", dest.display());
            stats.skipped += 1;
        }
        _ => stats.skipped += 1,
    }
}

/// The entries of a directory to copy, with their Windows names. Entries that
/// cannot be copied are logged and counted. Smaller than the directory table,
/// which is not kept while the levels below are copied.
fn children(
    vol: &Volume,
    node: &Node,
    dest: &Path,
    stats: &mut Stats,
) -> Option<Vec<(OsString, Node)>> {
    let table = match vol.dir(node.id) {
        Ok(t) => t,
        Err(e) => {
            log::error!("{}: cannot list: {e}", dest.display());
            stats.failed += 1;
            return None;
        }
    };
    if table.hidden > 0 {
        log::error!(
            "{}: {} entries not copied: their names are empty or clash with other entries",
            dest.display(),
            table.hidden
        );
        stats.failed += table.hidden as u64;
    }
    // Windows folders are case-insensitive: Makefile and makefile would collide.
    let mut seen = HashSet::new();
    let mut children = Vec::with_capacity(table.entries.len());
    for e in &table.entries {
        if !is_plain_name(&e.name) {
            log::error!(
                "{}: \"{}\" is not a valid file name, not copied",
                dest.display(),
                names::display(&e.raw)
            );
            stats.failed += 1;
            continue;
        }
        if !seen.insert(names::fold(&e.name)) {
            log::error!(
                "{}: \"{}\" differs from another entry only in case, not copied",
                dest.display(),
                names::display(&e.raw)
            );
            stats.failed += 1;
            continue;
        }
        let child = Node {
            id: e.id,
            kind: e.kind,
            parent: node.id,
        };
        children.push((OsString::from_wide(&e.name), child));
    }
    Some(children)
}

/// Mark a file sparse, so holes skipped below take no space. Best effort.
fn set_sparse(file: &File) {
    let mut returned = 0u32;
    // SAFETY: FSCTL_SET_SPARSE with no buffers on an open file handle.
    let _ = unsafe {
        DeviceIoControl(
            HANDLE(file.as_raw_handle()),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    };
}

fn copy_file(vol: &Volume, node: &Node, dest: &Path) -> anyhow::Result<u64> {
    let attr = vol.attr(node)?;
    let mut out = File::create(dest)?;
    set_sparse(&out);
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    while off < attr.size {
        let want = (attr.size - off).min(buf.len() as u64) as usize;
        let n = vol.read(node.id, off, &mut buf[..want])?;
        if n == 0 {
            anyhow::bail!("file ends early at byte {off} of {}", attr.size);
        }
        if buf[..n].iter().all(|&b| b == 0) {
            out.seek(SeekFrom::Current(n as i64))?;
        } else {
            out.write_all(&buf[..n])?;
        }
        off += n as u64;
    }
    out.set_len(attr.size)?;
    if let Err(e) = out.set_times(file_times(&attr)) {
        log::warn!("{}: times not set: {e}", dest.display());
    }
    Ok(off)
}

/// The times to set; times the destination cannot hold (0) are left out.
fn file_times(attr: &Attr) -> FileTimes {
    let mut t = FileTimes::new();
    if attr.write > 0 {
        t = t.set_modified(systemtime(attr.write));
    }
    if attr.access > 0 {
        t = t.set_accessed(systemtime(attr.access));
    }
    if attr.creation > 0 {
        t = t.set_created(systemtime(attr.creation));
    }
    t
}

fn set_dir_times(vol: &Volume, node: &Node, dest: &Path) {
    let Ok(attr) = vol.attr(node) else { return };
    let dir = fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dest);
    match dir {
        Ok(dir) => {
            if let Err(e) = dir.set_times(file_times(&attr)) {
                log::warn!("{}: times not set: {e}", dest.display());
            }
        }
        Err(e) => log::warn!("{}: times not set: {e}", dest.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(format_time(EPOCH_1601), "1970-01-01 00:00");
        // 2026-11-14 12:34:00 UTC = 1794659640 s
        let ft = EPOCH_1601 + 1_794_659_640 * 10_000_000;
        assert_eq!(format_time(ft), "2026-11-14 12:34");
        assert_eq!(
            systemtime(ft),
            UNIX_EPOCH + Duration::from_secs(1_794_659_640)
        );
    }

    #[test]
    fn verbatim_paths() {
        let v = |s: &str| verbatim(Path::new(s)).to_string_lossy().into_owned();
        assert_eq!(v(r"C:\out"), r"\\?\C:\out");
        assert_eq!(v(r"\\server\share\x"), r"\\?\UNC\server\share\x");
        assert_eq!(v(r"\\?\C:\out"), r"\\?\C:\out");
    }

    #[test]
    fn plain_names() {
        let ok = |s: &str| is_plain_name(&s.encode_utf16().collect::<Vec<_>>());
        assert!(ok("file.txt"));
        assert!(ok("..."));
        assert!(ok(".hidden"));
        for bad in ["", ".", "..", "../x", "a/b", r"a\b", r"\x", "/x"] {
            assert!(!ok(bad), "{bad:?}");
        }
    }
}
