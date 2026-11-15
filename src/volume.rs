// SPDX-License-Identifier: AGPL-3.0-or-later
//! A mounted UFS2 file system: path lookup, directory tables, attributes.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rufs::{BlockReader, InodeNum, InodeType, Ufs};

use crate::device::{Device, DeviceError, PartitionReader, device_error};
use crate::names::{self, Charset};
use crate::partition::{Candidate, describe_flags};

/// Directory entries kept in the cache, over all directories.
const DIR_CACHE_ENTRIES: usize = 200_000;
/// Entries and name bytes read from one directory at most. Real directories
/// stay far below; a damaged one could claim more than memory holds.
/// A table takes about 350 bytes per entry.
const DIR_MAX_ENTRIES: usize = 1_000_000;
const DIR_MAX_NAME_BYTES: usize = 256 << 20;
/// Symlinks followed while resolving one path, in total (MAXSYMLINKS on FreeBSD).
const MAX_LINKS: u32 = 32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    #[error("path not found")]
    PathNotFound,
    #[error("not a directory")]
    NotADirectory,
    #[error("{}", describe(.0))]
    Io(io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// errno values rufs reports, by name. On Windows `io::Error` would print
/// them as unrelated Win32 messages.
fn errno_name(code: i32) -> Option<&'static str> {
    Some(match code {
        2 => "ENOENT",
        5 => "EIO",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        30 => "EROFS",
        120 => "ENODATA",
        _ => return None,
    })
}

pub fn describe(e: &io::Error) -> String {
    if let Some(d) = device_error(e) {
        return d.to_string();
    }
    if e.get_ref().is_none()
        && let Some(name) = e.raw_os_error().and_then(errno_name)
    {
        return format!("UFS error {name}");
    }
    e.to_string()
}

/// One directory entry, with its Windows name.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: Vec<u16>,
    pub raw: Vec<u8>,
    pub inr: InodeNum,
    pub kind: InodeType,
}

/// All entries of one directory, without `.` and `..`.
#[derive(Debug)]
pub struct DirTable {
    pub entries: Vec<Entry>,
    pub parent: InodeNum,
    /// Entries left out: an empty name, or a Windows name that clashes with another one.
    pub hidden: usize,
    exact: HashMap<Vec<u16>, usize>,
    folded: HashMap<String, (usize, u32)>,
    raw: HashMap<Vec<u8>, usize>,
}

impl DirTable {
    fn build(
        dir: InodeNum,
        items: Vec<(Vec<u8>, InodeNum, InodeType)>,
        charset: Charset,
    ) -> DirTable {
        let mut table = DirTable {
            entries: Vec::with_capacity(items.len()),
            parent: dir,
            hidden: 0,
            exact: HashMap::with_capacity(items.len()),
            folded: HashMap::new(),
            raw: HashMap::with_capacity(items.len()),
        };
        for (raw, inr, kind) in items {
            if raw == b"." {
                continue;
            }
            if raw == b".." {
                table.parent = inr;
                continue;
            }
            if !matches!(
                kind,
                InodeType::RegularFile | InodeType::Directory | InodeType::Symlink
            ) {
                log::debug!("dir {dir}: hiding {kind:?} \"{}\"", names::display(&raw));
                continue;
            }
            if raw.is_empty() {
                log::warn!("dir {dir}: an entry for inode {inr} has an empty name, hidden");
                table.hidden += 1;
                continue;
            }
            let name = names::to_windows(&raw, charset);
            if table.exact.contains_key(&name) {
                log::warn!(
                    "dir {dir}: \"{}\" has the same Windows name as another entry, hidden",
                    names::display(&raw)
                );
                table.hidden += 1;
                continue;
            }
            let idx = table.entries.len();
            table.exact.insert(name.clone(), idx);
            table
                .folded
                .entry(names::fold(&name))
                .and_modify(|(_, n)| *n += 1)
                .or_insert((idx, 1));
            table.raw.insert(raw.clone(), idx);
            table.entries.push(Entry {
                name,
                raw,
                inr,
                kind,
            });
        }
        table
    }

    /// Exact match, else a case-insensitive match if it is unique.
    pub fn find(&self, name: &[u16]) -> Option<&Entry> {
        if let Some(&i) = self.exact.get(name) {
            return Some(&self.entries[i]);
        }
        match self.folded.get(&names::fold(name)) {
            Some(&(i, 1)) => Some(&self.entries[i]),
            _ => None,
        }
    }

    pub fn find_raw(&self, name: &[u8]) -> Option<&Entry> {
        self.raw.get(name).map(|&i| &self.entries[i])
    }
}

struct DirCache {
    dirs: HashMap<u32, (Arc<DirTable>, u64)>,
    entries: usize,
    clock: u64,
}

/// File attributes as Windows wants them.
#[derive(Debug, Clone, Default)]
pub struct Attr {
    pub inr: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// For symlinks: the target resolves to a directory.
    pub link_to_dir: bool,
    pub size: u64,
    pub alloc: u64,
    pub nlink: u32,
    /// FILETIME values: 100 ns since 1601.
    pub creation: u64,
    pub access: u64,
    pub write: u64,
    pub change: u64,
}

/// The result of a path lookup.
#[derive(Debug, Clone, Copy)]
pub struct Node {
    pub inr: InodeNum,
    pub kind: InodeType,
    pub parent: InodeNum,
}

pub fn filetime(t: SystemTime) -> u64 {
    const EPOCH_1601: i128 = 116_444_736_000_000_000;
    let ticks = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => EPOCH_1601 + (d.as_nanos() / 100) as i128,
        Err(e) => EPOCH_1601 - (e.duration().as_nanos() / 100) as i128,
    };
    ticks.clamp(0, i64::MAX as i128) as u64
}

pub struct Volume {
    ufs: Mutex<Ufs<PartitionReader>>,
    dirs: Mutex<DirCache>,
    dev: Arc<Device>,
    pub info: rufs::Info,
    pub charset: Charset,
}

impl Volume {
    /// Open the UFS2 file system of a probed candidate.
    pub fn open(dev: Arc<Device>, cand: &Candidate, charset: Charset) -> anyhow::Result<Volume> {
        let facts = cand
            .ufs2()
            .ok_or_else(|| anyhow::anyhow!("{} holds no UFS2 file system", cand.id))?;
        let fsize = u64::from(facts.info.fsize);
        let reader = PartitionReader::new(dev.clone(), cand.start, cand.len, fsize);
        let ufs = Ufs::new(BlockReader::new(reader, fsize as usize, false))
            .map_err(|e| anyhow::anyhow!("cannot open UFS2 on {}: {}", cand.id, describe(&e)))?;
        let info = ufs.info();
        if !info.clean {
            let flags = describe_flags(info.flags).join(", ");
            log::warn!(
                "the file system was not unmounted cleanly ({flags}). It is read as is; recently written files may be incomplete."
            );
        }
        Ok(Volume {
            ufs: Mutex::new(ufs),
            dirs: Mutex::new(DirCache {
                dirs: HashMap::new(),
                entries: 0,
                clock: 0,
            }),
            dev,
            info,
            charset,
        })
    }

    pub fn device(&self) -> &Arc<Device> {
        &self.dev
    }

    /// Run a rufs call. A device error that rufs replaced is recovered here.
    fn with_ufs<T>(&self, f: impl FnOnce(&mut Ufs<PartitionReader>) -> io::Result<T>) -> Result<T> {
        let mut ufs: MutexGuard<'_, _> = self.ufs.lock().unwrap_or_else(PoisonError::into_inner);
        self.dev.take_last_error();
        f(&mut ufs).map_err(|e| {
            // Only an EIO or an errno-less error can stand for a device failure.
            let replaceable = matches!(e.raw_os_error(), None | Some(5));
            let e = match (device_error(&e), self.dev.take_last_error()) {
                (None, Some(d)) if replaceable => io::Error::other::<DeviceError>(d),
                _ => e,
            };
            Error::Io(e)
        })
    }

    /// The entries of a directory, from the cache or read now.
    pub fn dir(&self, inr: InodeNum) -> Result<Arc<DirTable>> {
        {
            let mut cache = self.dirs.lock().unwrap_or_else(PoisonError::into_inner);
            cache.clock += 1;
            let clock = cache.clock;
            if let Some((table, used)) = cache.dirs.get_mut(&inr.get()) {
                *used = clock;
                return Ok(table.clone());
            }
        }
        let items = self.with_ufs(|ufs| {
            let mut items = Vec::new();
            let mut name_bytes = 0;
            let too_big = ufs.dir_iter_bytes(inr, |name, inr, kind| {
                name_bytes += name.len();
                if items.len() == DIR_MAX_ENTRIES || name_bytes > DIR_MAX_NAME_BYTES {
                    return Some(());
                }
                items.push((name.to_vec(), inr, kind));
                None
            })?;
            Ok(too_big.is_none().then_some(items))
        })?
        .ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "directory {inr} holds more than {DIR_MAX_ENTRIES} entries or {} MiB of names",
                    DIR_MAX_NAME_BYTES >> 20
                ),
            ))
        })?;
        let table = Arc::new(DirTable::build(inr, items, self.charset));

        let mut cache = self.dirs.lock().unwrap_or_else(PoisonError::into_inner);
        cache.clock += 1;
        let clock = cache.clock;
        // Every table costs at least one entry, so empty directories are bounded too.
        let cost = |t: &DirTable| t.entries.len() + 1;
        cache.entries += cost(&table);
        if let Some((old, _)) = cache.dirs.insert(inr.get(), (table.clone(), clock)) {
            cache.entries -= cost(&old);
        }
        if cache.entries > DIR_CACHE_ENTRIES {
            // Evict the oldest tables down to 90 %, sorting once.
            let mut ages: Vec<(u64, u32)> = cache
                .dirs
                .iter()
                .filter(|(k, _)| **k != inr.get())
                .map(|(k, (_, used))| (*used, *k))
                .collect();
            ages.sort_unstable();
            for (_, k) in ages {
                if cache.entries <= DIR_CACHE_ENTRIES / 10 * 9 {
                    break;
                }
                if let Some((old, _)) = cache.dirs.remove(&k) {
                    cache.entries -= cost(&old);
                }
            }
        }
        Ok(table)
    }

    /// Look up a path such as `\dir\file` (`/` works too). Symlinks inside
    /// the path are not followed; Windows resolves them through reparse points.
    pub fn lookup(&self, path: &[u16]) -> Result<Node> {
        let comps: Vec<&[u16]> = path
            .split(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
            .filter(|c| !c.is_empty())
            .collect();
        let mut node = Node {
            inr: InodeNum::ROOT,
            kind: InodeType::Directory,
            parent: InodeNum::ROOT,
        };
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            if node.kind != InodeType::Directory {
                return Err(Error::PathNotFound);
            }
            let table = self.dir(node.inr).map_err(|e| match e {
                Error::Io(_) => e,
                _ => Error::PathNotFound,
            })?;
            let Some(entry) = table.find(comp) else {
                return Err(if last {
                    Error::NotFound
                } else {
                    Error::PathNotFound
                });
            };
            node = Node {
                inr: entry.inr,
                kind: entry.kind,
                parent: node.inr,
            };
        }
        Ok(node)
    }

    /// Attributes of a node. Unreadable inodes are an error here; listings
    /// use [`Volume::attr_or_default`].
    pub fn attr(&self, node: &Node) -> Result<Attr> {
        let a = self.with_ufs(|ufs| ufs.inode_attr(node.inr))?;
        let is_dir = a.kind == InodeType::Directory;
        let is_symlink = a.kind == InodeType::Symlink;
        let birth = filetime(a.btime);
        let write = filetime(a.mtime);
        let mut attr = Attr {
            inr: node.inr.get64(),
            is_dir,
            is_symlink,
            link_to_dir: false,
            size: if a.kind == InodeType::RegularFile {
                a.size
            } else {
                0
            },
            alloc: if a.kind == InodeType::RegularFile {
                a.blocks.saturating_mul(512)
            } else {
                0
            },
            nlink: u32::from(a.nlink),
            creation: if a.btime == UNIX_EPOCH { write } else { birth },
            access: filetime(a.atime),
            write,
            change: filetime(a.ctime),
        };
        if is_symlink {
            attr.link_to_dir = self
                .readlink(node.inr)
                .ok()
                .and_then(|target| self.resolve_target(node.parent, &target))
                .is_some_and(|kind| kind == InodeType::Directory);
        }
        Ok(attr)
    }

    /// Attributes for a listing: an unreadable entry is shown with zero size
    /// and times, so one bad inode does not fail the whole directory.
    pub fn attr_or_default(&self, node: &Node, name: &[u16]) -> Attr {
        self.attr(node).unwrap_or_else(|e| {
            log::warn!(
                "inode {} (\"{}\"): {e}",
                node.inr,
                String::from_utf16_lossy(name)
            );
            Attr {
                inr: node.inr.get64(),
                is_dir: node.kind == InodeType::Directory,
                is_symlink: node.kind == InodeType::Symlink,
                ..Attr::default()
            }
        })
    }

    pub fn read(&self, inr: InodeNum, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.with_ufs(|ufs| ufs.inode_read(inr, offset, buf))
    }

    pub fn readlink(&self, inr: InodeNum) -> Result<Vec<u8>> {
        self.with_ufs(|ufs| ufs.symlink_read(inr))
    }

    /// A symlink target with the last mount point removed, so `/storage/x`
    /// on a file system last mounted on `/storage` becomes `/x`.
    fn strip_fsmnt<'a>(&self, target: &'a [u8]) -> &'a [u8] {
        let mnt = &self.info.fsmnt;
        if mnt.len() > 1
            && let Some(rest) = target.strip_prefix(mnt.as_slice())
            && (rest.is_empty() || rest[0] == b'/')
        {
            return if rest.is_empty() { b"/" } else { rest };
        }
        target
    }

    /// Follow a symlink target from directory `dir`; the kind it ends at.
    fn resolve_target(&self, dir: InodeNum, target: &[u8]) -> Option<InodeType> {
        let mut budget = MAX_LINKS;
        self.resolve_inode(dir, target, &mut budget)
            .map(|(_, kind)| kind)
    }

    /// Resolve a path typed by the user: UFS names, `/` or `\` separators,
    /// relative to the root. `.`, `..` and symlinks are followed, as a shell would.
    pub fn resolve_path(&self, path: &str) -> Result<Node> {
        let path = path.replace('\\', "/");
        let mut budget = MAX_LINKS;
        let (inr, kind) = self
            .resolve_inode(
                InodeNum::ROOT,
                path.trim_start_matches('/').as_bytes(),
                &mut budget,
            )
            .ok_or(Error::NotFound)?;
        Ok(Node {
            inr,
            kind,
            parent: InodeNum::ROOT,
        })
    }

    /// Follow `target` from `dir`. `budget` caps the symlinks followed in
    /// total, over all nesting levels.
    fn resolve_inode(
        &self,
        mut dir: InodeNum,
        target: &[u8],
        budget: &mut u32,
    ) -> Option<(InodeNum, InodeType)> {
        let mut target = target;
        if target.first() == Some(&b'/') {
            target = self.strip_fsmnt(target);
            dir = InodeNum::ROOT;
        }
        let comps: Vec<&[u8]> = target
            .split(|&c| c == b'/')
            .filter(|c| !c.is_empty() && *c != b".")
            .collect();
        for (i, comp) in comps.iter().enumerate() {
            if *comp == b".." {
                dir = self.dir(dir).ok()?.parent;
                continue;
            }
            let table = self.dir(dir).ok()?;
            let entry = table.find_raw(comp)?;
            let (inr, kind) = if entry.kind == InodeType::Symlink {
                *budget = budget.checked_sub(1)?;
                let link = self.readlink(entry.inr).ok()?;
                self.resolve_inode(dir, &link, budget)?
            } else {
                (entry.inr, entry.kind)
            };
            if i + 1 == comps.len() {
                return Some((inr, kind));
            }
            if kind != InodeType::Directory {
                return None;
            }
            dir = inr;
        }
        Some((dir, InodeType::Directory))
    }

    /// A symlink target as a Windows reparse target: `\` separators, names
    /// mapped, absolute targets relative to the volume root (`\...`).
    pub fn link_target_windows(&self, target: &[u8]) -> Vec<u16> {
        let absolute = target.first() == Some(&b'/');
        let target = if absolute {
            self.strip_fsmnt(target)
        } else {
            target
        };
        let mut out = Vec::new();
        if absolute {
            out.push(u16::from(b'\\'));
        }
        for comp in target
            .split(|&c| c == b'/')
            .filter(|c| !c.is_empty() && *c != b".")
        {
            if !out.is_empty() && out.last() != Some(&u16::from(b'\\')) {
                out.push(u16::from(b'\\'));
            }
            if comp == b"." || comp == b".." {
                out.extend(comp.iter().map(|&b| u16::from(b)));
            } else {
                out.extend(names::to_windows(comp, self.charset));
            }
        }
        if out.is_empty() {
            out.push(u16::from(b'.'));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn table_rules() {
        let root = InodeNum::ROOT;
        let items = vec![
            (b".".to_vec(), root, InodeType::Directory),
            (b"..".to_vec(), root, InodeType::Directory),
            (b"Makefile".to_vec(), root, InodeType::RegularFile),
            (b"makefile".to_vec(), root, InodeType::RegularFile),
            (b"README".to_vec(), root, InodeType::RegularFile),
            (b"a:".to_vec(), root, InodeType::RegularFile),
            (
                "a\u{f03a}".as_bytes().to_vec(),
                root,
                InodeType::RegularFile,
            ),
            (b"tty".to_vec(), root, InodeType::CharDevice),
            (b"pipe".to_vec(), root, InodeType::NamedPipe),
            (Vec::new(), root, InodeType::Directory),
        ];
        let t = DirTable::build(root, items, Charset::Utf8);
        let names: Vec<String> = t
            .entries
            .iter()
            .map(|e| String::from_utf16(&e.name).unwrap())
            .collect();
        assert_eq!(names, ["Makefile", "makefile", "README", "a\u{f03a}"]);
        assert_eq!(
            t.hidden, 2,
            "the empty name, and the literal U+F03A name that clashes with the mapped ':'"
        );
        assert!(t.find(&w("Makefile")).is_some());
        assert!(t.find(&w("MAKEFILE")).is_none(), "ambiguous fold must miss");
        assert_eq!(t.find(&w("readme")).unwrap().raw, b"README");
        assert!(t.find_raw(b"a:").is_some());
        assert!(t.find(&w("tty")).is_none());
    }

    #[test]
    fn filetime_edges() {
        assert_eq!(filetime(UNIX_EPOCH), 116_444_736_000_000_000);
        let before = UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert_eq!(filetime(before), 116_444_736_000_000_000 - 10_000_000);
    }
}
