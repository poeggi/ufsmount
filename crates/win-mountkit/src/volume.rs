// SPDX-License-Identifier: AGPL-3.0-or-later
//! An open file system: path lookup, directory tables, attributes.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::device::{Device, DeviceError, device_error};
use crate::fs::{Fs, Info, Kind, NodeId};
use crate::names::{self, Charset};
use crate::partition::Candidate;
use crate::product::product;

/// Directory entries kept in the cache, over all directories.
const DIR_CACHE_ENTRIES: usize = 200_000;
/// Entries and name bytes read from one directory at most. Real directories
/// stay far below; a damaged one could claim more than memory holds.
/// A table takes about 350 bytes per entry.
const DIR_MAX_ENTRIES: usize = 1_000_000;
const DIR_MAX_NAME_BYTES: usize = 256 << 20;
/// Symlinks followed while resolving one path, in total (MAXSYMLINKS on FreeBSD).
const MAX_LINKS: u32 = 32;
/// Symlinks whose kind is remembered; the cache is cleared when full.
const LINK_CACHE_ENTRIES: usize = 65_536;

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

/// errno values file system code reports, by name. On Windows `io::Error`
/// would print them as unrelated Win32 messages.
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
        return format!("{} error {name}", product().fs);
    }
    e.to_string()
}

/// One directory entry, with its Windows name.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: Vec<u16>,
    pub raw: Vec<u8>,
    pub id: NodeId,
    pub kind: Kind,
}

/// All entries of one directory, without `.` and `..`.
#[derive(Debug)]
pub struct DirTable {
    pub entries: Vec<Entry>,
    pub parent: NodeId,
    /// Entries left out: an empty name, or a Windows name that clashes with another one.
    pub hidden: usize,
    exact: HashMap<Vec<u16>, usize>,
    folded: HashMap<String, (usize, u32)>,
    raw: HashMap<Vec<u8>, usize>,
}

impl DirTable {
    fn build(dir: NodeId, items: Vec<(Vec<u8>, NodeId, Kind)>, charset: Charset) -> DirTable {
        let mut table = DirTable {
            entries: Vec::with_capacity(items.len()),
            parent: dir,
            hidden: 0,
            exact: HashMap::with_capacity(items.len()),
            folded: HashMap::new(),
            raw: HashMap::with_capacity(items.len()),
        };
        for (raw, id, kind) in items {
            if raw == b"." {
                continue;
            }
            if raw == b".." {
                table.parent = id;
                continue;
            }
            if kind == Kind::Other {
                log::debug!(
                    "dir {dir}: hiding the special file \"{}\"",
                    names::display(&raw)
                );
                continue;
            }
            if raw.is_empty() {
                log::warn!("dir {dir}: an entry for inode {id} has an empty name, hidden");
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
                id,
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
    dirs: HashMap<NodeId, (Arc<DirTable>, u64)>,
    entries: usize,
    clock: u64,
}

/// File attributes as Windows wants them.
#[derive(Debug, Clone, Default)]
pub struct Attr {
    pub id: NodeId,
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
    pub id: NodeId,
    pub kind: Kind,
    pub parent: NodeId,
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
    fs: Mutex<Box<dyn Fs>>,
    dirs: Mutex<DirCache>,
    /// Whether a symlink, by its directory and id, resolves to a directory.
    /// A hard-linked symlink can resolve differently in each directory.
    links: Mutex<HashMap<(NodeId, NodeId), bool>>,
    dev: Arc<Device>,
    root: NodeId,
    pub info: Info,
    pub charset: Charset,
}

impl Volume {
    /// Open the file system of a probed candidate.
    pub fn open(dev: Arc<Device>, cand: &Candidate, charset: Charset) -> anyhow::Result<Volume> {
        let found = cand.found().ok_or_else(|| {
            let fs = product().fs_variant;
            anyhow::anyhow!("{} holds no {fs} file system", cand.id)
        })?;
        let fs = product().backend.open(dev.clone(), cand, found)?;
        // WinFsp holds 15 characters and a NUL.
        let name = fs.info().fs;
        anyhow::ensure!(
            (1..16).contains(&name.len()) && name.is_ascii(),
            "the file system name {name:?} does not fit WinFsp"
        );
        Ok(Volume::new(dev, fs, charset))
    }

    fn new(dev: Arc<Device>, fs: Box<dyn Fs>, charset: Charset) -> Volume {
        Volume {
            info: fs.info(),
            root: fs.root(),
            fs: Mutex::new(fs),
            dirs: Mutex::new(DirCache {
                dirs: HashMap::new(),
                entries: 0,
                clock: 0,
            }),
            links: Mutex::new(HashMap::new()),
            dev,
            charset,
        }
    }

    pub fn device(&self) -> &Arc<Device> {
        &self.dev
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    fn root_node(&self) -> Node {
        Node {
            id: self.root,
            kind: Kind::Dir,
            parent: self.root,
        }
    }

    /// Run a file system call. A device error that the file system code
    /// replaced is recovered here.
    fn with_fs<T>(&self, f: impl FnOnce(&mut dyn Fs) -> io::Result<T>) -> Result<T> {
        let mut fs: MutexGuard<'_, _> = self.fs.lock().unwrap_or_else(PoisonError::into_inner);
        self.dev.take_last_error();
        f(fs.as_mut()).map_err(|e| {
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
    pub fn dir(&self, id: NodeId) -> Result<Arc<DirTable>> {
        {
            let mut cache = self.dirs.lock().unwrap_or_else(PoisonError::into_inner);
            cache.clock += 1;
            let clock = cache.clock;
            if let Some((table, used)) = cache.dirs.get_mut(&id) {
                *used = clock;
                return Ok(table.clone());
            }
        }
        let items = self.with_fs(|fs| {
            let mut items = Vec::new();
            let mut name_bytes = 0;
            let mut too_big = false;
            fs.read_dir(id, &mut |name, id, kind| {
                name_bytes += name.len();
                too_big = items.len() == DIR_MAX_ENTRIES || name_bytes > DIR_MAX_NAME_BYTES;
                if !too_big {
                    items.push((name.to_vec(), id, kind));
                }
                too_big
            })?;
            Ok((!too_big).then_some(items))
        })?
        .ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "directory {id} holds more than {DIR_MAX_ENTRIES} entries or {} MiB of names",
                    DIR_MAX_NAME_BYTES >> 20
                ),
            ))
        })?;
        let table = Arc::new(DirTable::build(id, items, self.charset));

        let mut cache = self.dirs.lock().unwrap_or_else(PoisonError::into_inner);
        cache.clock += 1;
        let clock = cache.clock;
        // Every table costs at least one entry, so empty directories are bounded too.
        let cost = |t: &DirTable| t.entries.len() + 1;
        cache.entries += cost(&table);
        if let Some((old, _)) = cache.dirs.insert(id, (table.clone(), clock)) {
            cache.entries -= cost(&old);
        }
        if cache.entries > DIR_CACHE_ENTRIES {
            // Evict the oldest tables down to 90 %, sorting once.
            let mut ages: Vec<(u64, NodeId)> = cache
                .dirs
                .iter()
                .filter(|(k, _)| **k != id)
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
        let mut node = self.root_node();
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            if node.kind != Kind::Dir {
                return Err(Error::PathNotFound);
            }
            let table = self.dir(node.id).map_err(|e| match e {
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
                id: entry.id,
                kind: entry.kind,
                parent: node.id,
            };
        }
        Ok(node)
    }

    /// Attributes of a node. Unreadable nodes are an error here; listings
    /// use [`Volume::attr_or_default`].
    pub fn attr(&self, node: &Node) -> Result<Attr> {
        let s = self.with_fs(|fs| fs.stat(node.id))?;
        let file = s.kind == Kind::File;
        let mut attr = Attr {
            id: node.id,
            is_dir: s.kind == Kind::Dir,
            is_symlink: s.kind == Kind::Symlink,
            link_to_dir: false,
            size: if file { s.size } else { 0 },
            alloc: if file { s.alloc } else { 0 },
            nlink: s.nlink,
            creation: s.creation,
            access: s.access,
            write: s.write,
            change: s.change,
        };
        if attr.is_symlink {
            attr.link_to_dir = self.link_to_dir(node);
        }
        Ok(attr)
    }

    /// Whether symlink `node` resolves to a directory. Remembered: a target
    /// kept in a data block costs a seek. An error is not remembered.
    fn link_to_dir(&self, node: &Node) -> bool {
        let key = (node.parent, node.id);
        let cached = self
            .links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .copied();
        if let Some(to_dir) = cached {
            return to_dir;
        }
        let resolved = self
            .readlink(node.id)
            .and_then(|target| self.resolve_target(node.parent, &target));
        let Ok(kind) = resolved else {
            return false;
        };
        let to_dir = kind == Some(Kind::Dir);
        let mut links = self.links.lock().unwrap_or_else(PoisonError::into_inner);
        if links.len() >= LINK_CACHE_ENTRIES {
            links.clear();
        }
        links.insert(key, to_dir);
        to_dir
    }

    /// Attributes for a listing: an unreadable entry is shown with zero size
    /// and times, so one bad inode does not fail the whole directory.
    pub fn attr_or_default(&self, node: &Node, name: &[u16]) -> Attr {
        self.attr(node).unwrap_or_else(|e| {
            log::warn!(
                "inode {} (\"{}\"): {e}",
                node.id,
                String::from_utf16_lossy(name)
            );
            Attr {
                id: node.id,
                is_dir: node.kind == Kind::Dir,
                is_symlink: node.kind == Kind::Symlink,
                ..Attr::default()
            }
        })
    }

    pub fn read(&self, id: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.with_fs(|fs| fs.read(id, offset, buf))
    }

    pub fn readlink(&self, id: NodeId) -> Result<Vec<u8>> {
        self.with_fs(|fs| fs.read_link(id))
    }

    /// A symlink target with the last mount point removed, so `/storage/x`
    /// on a file system last mounted on `/storage` becomes `/x`.
    fn strip_mount_point<'a>(&self, target: &'a [u8]) -> &'a [u8] {
        let mnt = &self.info.mount_point;
        if mnt.len() > 1
            && let Some(rest) = target.strip_prefix(mnt.as_slice())
            && (rest.is_empty() || rest[0] == b'/')
        {
            return if rest.is_empty() { b"/" } else { rest };
        }
        target
    }

    /// Follow a symlink target from directory `dir`; the kind it ends at.
    fn resolve_target(&self, dir: NodeId, target: &[u8]) -> Result<Option<Kind>> {
        let mut budget = MAX_LINKS;
        Ok(self
            .resolve_node(dir, target, &mut budget)?
            .map(|(_, kind)| kind))
    }

    /// Resolve a path typed by the user: names as on the disk, `/` or `\`
    /// separators, relative to the root. `.`, `..` and symlinks are followed,
    /// as a shell would.
    pub fn resolve_path(&self, path: &str) -> Result<Node> {
        let path = path.replace('\\', "/");
        let mut budget = MAX_LINKS;
        let (id, kind) = self
            .resolve_node(
                self.root,
                path.trim_start_matches('/').as_bytes(),
                &mut budget,
            )?
            .ok_or(Error::NotFound)?;
        Ok(Node {
            id,
            kind,
            parent: self.root,
        })
    }

    /// Follow `target` from `dir`; None if it ends nowhere. `budget` caps the
    /// symlinks followed in total, over all nesting levels.
    fn resolve_node(
        &self,
        mut dir: NodeId,
        target: &[u8],
        budget: &mut u32,
    ) -> Result<Option<(NodeId, Kind)>> {
        let mut target = target;
        if target.first() == Some(&b'/') {
            target = self.strip_mount_point(target);
            dir = self.root;
        }
        let comps: Vec<&[u8]> = target
            .split(|&c| c == b'/')
            .filter(|c| !c.is_empty() && *c != b".")
            .collect();
        for (i, comp) in comps.iter().enumerate() {
            if *comp == b".." {
                dir = self.dir(dir)?.parent;
                continue;
            }
            let table = self.dir(dir)?;
            let Some(entry) = table.find_raw(comp) else {
                return Ok(None);
            };
            let (id, kind) = if entry.kind == Kind::Symlink {
                let Some(left) = budget.checked_sub(1) else {
                    return Ok(None);
                };
                *budget = left;
                let link = self.readlink(entry.id)?;
                match self.resolve_node(dir, &link, budget)? {
                    Some(found) => found,
                    None => return Ok(None),
                }
            } else {
                (entry.id, entry.kind)
            };
            if i + 1 == comps.len() {
                return Ok(Some((id, kind)));
            }
            if kind != Kind::Dir {
                return Ok(None);
            }
            dir = id;
        }
        Ok(Some((dir, Kind::Dir)))
    }

    /// A symlink target as a Windows reparse target: `\` separators, names
    /// mapped, absolute targets relative to the volume root (`\...`).
    pub fn link_target_windows(&self, target: &[u8]) -> Vec<u16> {
        let absolute = target.first() == Some(&b'/');
        let target = if absolute {
            self.strip_mount_point(target)
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::device::testing::FakeDisk;
    use crate::fs::Stat;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn table_rules() {
        let root = 2;
        let items = vec![
            (b".".to_vec(), root, Kind::Dir),
            (b"..".to_vec(), root, Kind::Dir),
            (b"Makefile".to_vec(), root, Kind::File),
            (b"makefile".to_vec(), root, Kind::File),
            (b"README".to_vec(), root, Kind::File),
            (b"a:".to_vec(), root, Kind::File),
            ("a\u{f03a}".as_bytes().to_vec(), root, Kind::File),
            (b"tty".to_vec(), root, Kind::Other),
            (b"pipe".to_vec(), root, Kind::Other),
            (Vec::new(), root, Kind::Dir),
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

    #[test]
    fn errno_names() {
        let e = io::Error::from_raw_os_error(2);
        assert_eq!(describe(&e), "DEMO error ENOENT");
    }

    /// Root 2 holds directory "sub" (3) and symlink "link" (4) to `target`;
    /// "sub" holds a hard link of the same symlink. Reading "sub" fails
    /// `sub_errors` times.
    struct Links {
        reads: Arc<AtomicUsize>,
        target: &'static [u8],
        sub_errors: usize,
    }

    fn volume(fs: Links) -> Volume {
        let dev = FakeDisk::new(vec![0; 4096], 512).into_device();
        Volume::new(dev, Box::new(fs), Charset::Utf8)
    }

    impl Fs for Links {
        fn info(&self) -> Info {
            Info {
                fs: "TEST",
                label: Vec::new(),
                serial: 0,
                block_size: 512,
                total: 0,
                free: 0,
                mount_point: Vec::new(),
            }
        }
        fn root(&self) -> NodeId {
            2
        }
        fn stat(&mut self, id: NodeId) -> io::Result<Stat> {
            let kind = if id == 4 { Kind::Symlink } else { Kind::Dir };
            Ok(Stat {
                kind,
                size: 3,
                alloc: 0,
                nlink: 2,
                creation: 0,
                access: 0,
                write: 0,
                change: 0,
            })
        }
        fn read_dir(
            &mut self,
            dir: NodeId,
            f: &mut dyn FnMut(&[u8], NodeId, Kind) -> bool,
        ) -> io::Result<()> {
            if dir == 3 && self.sub_errors > 0 {
                self.sub_errors -= 1;
                return Err(io::Error::from_raw_os_error(5));
            }
            let mut items = vec![(&b".."[..], 2, Kind::Dir), (b"link", 4, Kind::Symlink)];
            if dir == 2 {
                items.push((b"sub", 3, Kind::Dir));
            }
            for (name, id, kind) in items {
                if f(name, id, kind) {
                    break;
                }
            }
            Ok(())
        }
        fn read(&mut self, _: NodeId, _: u64, _: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn read_link(&mut self, _: NodeId) -> io::Result<Vec<u8>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.target.to_vec())
        }
    }

    #[test]
    fn symlink_kind_is_remembered_per_directory() {
        let reads = Arc::new(AtomicUsize::new(0));
        let vol = volume(Links {
            reads: reads.clone(),
            target: b"sub",
            sub_errors: 0,
        });
        let in_root = vol.lookup(&w("link")).unwrap();
        let in_sub = vol.lookup(&w("sub/link")).unwrap();
        for _ in 0..2 {
            assert!(vol.attr(&in_root).unwrap().link_to_dir);
        }
        assert_eq!(
            reads.load(Ordering::Relaxed),
            1,
            "read once, then remembered"
        );
        for _ in 0..2 {
            assert!(
                !vol.attr(&in_sub).unwrap().link_to_dir,
                "sub/sub does not exist"
            );
        }
        assert_eq!(reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn symlink_kind_error_is_not_remembered() {
        let vol = volume(Links {
            reads: Arc::default(),
            target: b"sub/..",
            sub_errors: 1,
        });
        let link = vol.lookup(&w("link")).unwrap();
        assert!(!vol.attr(&link).unwrap().link_to_dir, "reading sub failed");
        assert!(vol.attr(&link).unwrap().link_to_dir);
    }
}
