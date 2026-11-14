// SPDX-License-Identifier: AGPL-3.0-or-later
//! `ls` and `cp` without WinFsp.

use std::ffi::OsString;
use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rufs::{InodeNum, InodeType};

use crate::volume::{Attr, Node, Volume};

const EPOCH_1601: u64 = 116_444_736_000_000_000;

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

fn path_utf16(path: &str) -> Vec<u16> {
    path.encode_utf16().collect()
}

pub fn ls(vol: &Volume, path: &str) -> anyhow::Result<()> {
    let node = vol
        .lookup(&path_utf16(path))
        .map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    if node.kind != InodeType::Directory {
        let attr = vol.attr(&node)?;
        println!("{}", line(&attr, path, None));
        return Ok(());
    }
    let table = vol.dir(node.inr)?;
    for e in &table.entries {
        let child = Node {
            inr: e.inr,
            kind: e.kind,
            parent: node.inr,
        };
        let name = String::from_utf16_lossy(&e.name);
        let attr = vol.attr_or_default(&child, &e.name);
        let target = if e.kind == InodeType::Symlink {
            vol.readlink(e.inr).ok()
        } else {
            None
        };
        println!("{}", line(&attr, &name, target.as_deref()));
    }
    Ok(())
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
        s += &format!(" -> {}", crate::names::display(t));
    }
    s
}

#[derive(Default)]
pub struct Stats {
    pub files: u64,
    pub bytes: u64,
    pub dirs: u64,
    pub skipped: u64,
    pub failed: u64,
}

/// Copy a file or a tree. Unreadable files are logged, removed and counted.
pub fn cp(vol: &Volume, from: &str, to: &Path) -> anyhow::Result<Stats> {
    let node = vol
        .lookup(&path_utf16(from))
        .map_err(|e| anyhow::anyhow!("{from}: {e}"))?;
    let mut dest = to.to_path_buf();
    if dest.is_dir() {
        let name = from
            .rsplit(['/', '\\'])
            .find(|c| !c.is_empty())
            .unwrap_or("root");
        dest.push(name);
    }
    let mut stats = Stats::default();
    copy_node(vol, &node, &dest, &mut stats);
    Ok(stats)
}

fn copy_node(vol: &Volume, node: &Node, dest: &Path, stats: &mut Stats) {
    match node.kind {
        InodeType::Directory => {
            if let Err(e) = fs::create_dir_all(dest) {
                log::error!("{}: {e}", dest.display());
                stats.failed += 1;
                return;
            }
            stats.dirs += 1;
            let table = match vol.dir(node.inr) {
                Ok(t) => t,
                Err(e) => {
                    log::error!("{}: cannot list: {e}", dest.display());
                    stats.failed += 1;
                    return;
                }
            };
            for e in &table.entries {
                let child = Node {
                    inr: e.inr,
                    kind: e.kind,
                    parent: node.inr,
                };
                let mut path: PathBuf = dest.to_path_buf();
                path.push(OsString::from_wide(&e.name));
                copy_node(vol, &child, &path, stats);
            }
            set_times(vol, node, dest);
        }
        InodeType::RegularFile => match copy_file(vol, node, dest) {
            Ok(n) => {
                stats.files += 1;
                stats.bytes += n;
            }
            Err(e) => {
                log::error!("{}: {e}", dest.display());
                let _ = fs::remove_file(dest);
                stats.failed += 1;
            }
        },
        InodeType::Symlink => {
            let target = vol
                .readlink(node.inr)
                .map(|t| crate::names::display(&t))
                .unwrap_or_default();
            log::info!("{}: symlink to {target}, skipped", dest.display());
            stats.skipped += 1;
        }
        _ => stats.skipped += 1,
    }
}

fn copy_file(vol: &Volume, node: &Node, dest: &Path) -> anyhow::Result<u64> {
    let attr = vol.attr(node)?;
    let mut out = File::create(dest)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    while off < attr.size {
        let want = (attr.size - off).min(buf.len() as u64) as usize;
        let n = vol.read(node.inr, off, &mut buf[..want])?;
        if n == 0 {
            anyhow::bail!("file ends early at byte {off} of {}", attr.size);
        }
        out.write_all(&buf[..n])?;
        off += n as u64;
    }
    out.set_times(file_times(&attr))?;
    Ok(off)
}

fn file_times(attr: &Attr) -> FileTimes {
    FileTimes::new()
        .set_modified(systemtime(attr.write))
        .set_accessed(systemtime(attr.access))
        .set_created(systemtime(attr.creation))
}

fn set_times(vol: &Volume, node: &Node, dest: &Path) {
    let Ok(attr) = vol.attr(node) else { return };
    let dir = fs::OpenOptions::new()
        .write(true)
        .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS, needed to open a directory
        .open(dest);
    if let Ok(dir) = dir {
        let _ = dir.set_times(file_times(&attr));
    }
}

pub fn root() -> Node {
    Node {
        inr: InodeNum::ROOT,
        kind: InodeType::Directory,
        parent: InodeNum::ROOT,
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
}
