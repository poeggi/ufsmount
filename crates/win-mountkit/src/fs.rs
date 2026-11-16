// SPDX-License-Identifier: AGPL-3.0-or-later
//! What a product's file system code provides: finding its file system on a
//! disk and reading it. Lookup, names, limits and WinFsp are shared.

use std::io;
use std::sync::Arc;

use crate::device::Device;
use crate::partition::Candidate;

/// A file or directory, numbered by the file system. Ids come only from
/// [`Fs::root`] and [`Fs::read_dir`].
pub type NodeId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    /// Devices, fifos, sockets: hidden.
    Other,
}

/// Attributes as the file system keeps them. Times are FILETIME values
/// (100 ns since 1601).
#[derive(Debug, Clone)]
pub struct Stat {
    pub kind: Kind,
    pub size: u64,
    /// Bytes allocated on the disk.
    pub alloc: u64,
    pub nlink: u32,
    pub creation: u64,
    pub access: u64,
    pub write: u64,
    pub change: u64,
}

#[derive(Debug, Clone)]
pub struct Info {
    /// The file system's name for Windows: Explorer shows it, and it is the
    /// label of a volume without one. 1 to 15 ASCII characters.
    pub fs: &'static str,
    /// The volume label, empty for none.
    pub label: Vec<u8>,
    pub serial: u32,
    /// The allocation unit in bytes.
    pub block_size: u32,
    pub total: u64,
    pub free: u64,
    /// Where it was last mounted: absolute symlink targets below it point
    /// into this file system.
    pub mount_point: Vec<u8>,
}

/// An open file system. Errors may carry errno values; a device error that
/// the code replaced is recovered by the caller.
pub trait Fs: Send {
    fn info(&self) -> Info;
    fn root(&self) -> NodeId;
    fn stat(&mut self, id: NodeId) -> io::Result<Stat>;
    /// Call `f` for each entry; `f` returns true to stop. `.` and `..` may be
    /// among them; `..` names the parent.
    fn read_dir(
        &mut self,
        dir: NodeId,
        f: &mut dyn FnMut(&[u8], NodeId, Kind) -> bool,
    ) -> io::Result<()>;
    fn read(&mut self, id: NodeId, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    fn read_link(&mut self, id: NodeId) -> io::Result<Vec<u8>>;
}

/// A file system found by [`Backend::probe`].
#[derive(Debug, Clone)]
pub struct Found {
    /// Stays the same on any port and disk number, such as an id from the superblock.
    pub id: String,
    /// Its size in bytes.
    pub size: u64,
    /// For the backend's `open`.
    pub block_size: u32,
    /// Which file system of a container, for the backend's `open`; 0 for a plain one.
    pub volume: u32,
    /// One line for `probe`.
    pub summary: String,
}

/// What a place on the disk holds.
#[derive(Debug, Clone)]
pub enum FsKind {
    Found(Box<Found>),
    /// A file system of the family that cannot be read, such as an old version.
    Unsupported(String),
    None,
}

pub trait Backend: Sync {
    /// The places to check, most specific first: the same start offset is
    /// checked once. `table` holds the GPT partitions (`p<n>`), MBR slices
    /// (`s<n>`, logical ones from `s5`), Apple Partition Map entries (`m<n>`)
    /// and the whole disk (`whole`); [`Candidate::table`] tells which.
    fn candidates(&self, _dev: &Device, _sector: u64, table: Vec<Candidate>) -> Vec<Candidate> {
        table
    }

    /// What `cand` holds: usually nothing or one file system; a container may
    /// hold several. The file systems must fit on the disk.
    fn probe(&self, dev: &Arc<Device>, cand: &Candidate) -> Vec<FsKind>;

    /// Open what `probe` found at `cand`.
    fn open(
        &self,
        dev: Arc<Device>,
        cand: &Candidate,
        found: &Found,
    ) -> anyhow::Result<Box<dyn Fs>>;
}
