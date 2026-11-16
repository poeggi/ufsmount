// SPDX-License-Identifier: AGPL-3.0-or-later
//! UFS2 through rufs: finding it on a disk (BSD labels, the superblock) and reading it.

use std::io;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use rufs::{BlockReader, InodeNum, InodeType};
use winfsp_mount::device::{Device, PartitionReader};
use winfsp_mount::fs::{Backend, Found, Fs, FsKind, Info, Kind, NodeId, Stat};
use winfsp_mount::partition::{Candidate, clamp, human_size, le16, le32, read};
use winfsp_mount::volume::{describe, filetime};

const SBLOCK_UFS2: u64 = 65536;
const SBLOCK_UFS1: u64 = 8192;
const MAGIC_OFFSET: u64 = 1372;
const FS_UFS2_MAGIC: u32 = 0x1954_0119;
const FS_UFS1_MAGIC: u32 = 0x0001_1954;

const DISKMAGIC: u32 = 0x8256_4557;
const MAXPARTITIONS: usize = 20;
const RAW_PART: usize = 2;

/// rufs reads a directory up to its claimed size, so that size is capped.
const DIR_MAX_BYTES: u64 = 1 << 30;

/// freebsd-ufs, 516e7cb6-6ecf-11d6-8ff8-00022d09712b, in on-disk (mixed-endian) order.
pub const GPT_FREEBSD_UFS: [u8; 16] = [
    0xb6, 0x7c, 0x6e, 0x51, 0xcf, 0x6e, 0xd6, 0x11, 0x8f, 0xf8, 0x00, 0x02, 0x2d, 0x09, 0x71, 0x2b,
];

/// Names of the `fs_flags` bits that matter to a reader.
fn describe_flags(flags: u32) -> Vec<&'static str> {
    const NAMES: [(u32, &str); 4] = [
        (0x01, "unclean"),
        (0x02, "soft updates"),
        (0x04, "needs fsck"),
        (0x08, "SU+J journal"),
    ];
    NAMES
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

fn describe_ufs(info: &rufs::Info, big_endian: bool) -> String {
    let mut s = format!(
        "UFS2{}, {} KiB blocks, {} KiB fragments, {}",
        if big_endian { " (big-endian)" } else { "" },
        info.bsize / 1024,
        info.fsize / 1024,
        human_size(info.frags * u64::from(info.fsize)),
    );
    if !info.volname.is_empty() {
        s += &format!(", label \"{}\"", info.volname.escape_ascii());
    }
    if !info.fsmnt.is_empty() {
        s += &format!(", last mounted on {}", info.fsmnt.escape_ascii());
    }
    s += if info.clean { ", clean" } else { ", NOT clean" };
    for flag in describe_flags(info.flags) {
        s += ", ";
        s += flag;
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LabelPart {
    letter: char,
    /// Offset in sectors, relative to the start of the slice.
    offset: u64,
    sectors: u64,
    fstype: u8,
}

/// A BSD disklabel. `sector` is the logical sector size of the device.
fn parse_label(buf: &[u8], sector: u64) -> Option<Vec<LabelPart>> {
    if buf.len() < 148 || le32(buf, 0) != DISKMAGIC || le32(buf, 132) != DISKMAGIC {
        return None;
    }
    let secsize = u64::from(le32(buf, 40));
    if secsize != sector {
        log::warn!("disklabel sector size {secsize} differs from the device's {sector}");
    }
    let npart = usize::from(le16(buf, 138));
    if npart == 0 || npart > MAXPARTITIONS || buf.len() < 148 + 16 * npart {
        log::warn!("disklabel with {npart} partitions ignored");
        return None;
    }
    let mut checksum = 0u16;
    for i in (0..148 + 16 * npart).step_by(2) {
        checksum ^= le16(buf, i);
    }
    if checksum != 0 {
        log::warn!("disklabel checksum mismatch (ignored)");
    }
    let entry = |i: usize| {
        let e = &buf[148 + 16 * i..164 + 16 * i];
        (u64::from(le32(e, 0)), u64::from(le32(e, 4)), e[12])
    };
    // Offsets are absolute on older labels; the raw partition c gives the base.
    let base = if npart > RAW_PART {
        entry(RAW_PART).1
    } else {
        0
    };
    let parts = (0..npart)
        .filter(|&i| i != RAW_PART)
        .filter_map(|i| {
            let (sectors, offset, fstype) = entry(i);
            if sectors == 0 || offset < base {
                return None;
            }
            Some(LabelPart {
                letter: (b'a' + i as u8) as char,
                offset: offset - base,
                sectors,
                fstype,
            })
        })
        .collect();
    Some(parts)
}

/// Partitions of a BSD label at sector 1 of `base`.
fn label_candidates(
    dev: &Device,
    sector: u64,
    base: u64,
    base_len: u64,
    prefix: &str,
    origin: &str,
) -> Vec<Candidate> {
    let Some(buf) = read(dev, base + sector, sector.max(512)) else {
        return Vec::new();
    };
    let Some(parts) = parse_label(&buf, sector) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in parts {
        let (Some(rel), Some(len)) = (p.offset.checked_mul(sector), p.sectors.checked_mul(sector))
        else {
            continue;
        };
        let Some(mut start) = rel.checked_add(base) else {
            continue;
        };
        // Old labels count offsets from the disk start even when c is at 0.
        if start.saturating_add(len) > base.saturating_add(base_len) && base > 0 && rel >= base {
            start = rel;
        }
        let what = format!("label partition {prefix}{}", p.letter);
        let Some(len) = clamp(dev, start, len, &what) else {
            continue;
        };
        out.push(Candidate {
            id: format!("{prefix}{}", p.letter),
            start,
            len,
            origin: format!(
                "{origin}BSD label partition {} (fstype {})",
                p.letter, p.fstype
            ),
            fs: FsKind::None,
        });
    }
    out
}

fn read_magic(dev: &Device, offset: u64) -> Option<[u8; 4]> {
    read(dev, offset, 4).map(|b| b.try_into().unwrap())
}

/// The id from the superblock; it stays the same on any port.
fn found(info: &rufs::Info, big_endian: bool) -> Found {
    Found {
        id: format!("{:08x}-{:08x}", info.id[0], info.id[1]),
        size: info.frags * u64::from(info.fsize),
        block_size: info.fsize,
        summary: describe_ufs(info, big_endian),
    }
}

/// Check one candidate for a UFS superblock.
fn probe_fs(dev: &Arc<Device>, start: u64, len: u64) -> FsKind {
    if len < SBLOCK_UFS2 + 8192 {
        return FsKind::None;
    }
    if let Some(magic) = read_magic(dev, start + SBLOCK_UFS2 + MAGIC_OFFSET) {
        let big_endian = u32::from_be_bytes(magic) == FS_UFS2_MAGIC;
        if big_endian || u32::from_le_bytes(magic) == FS_UFS2_MAGIC {
            let reader = PartitionReader::new(dev.clone(), start, len, 512);
            let info = match rufs::Ufs::new(BlockReader::new(reader, 512, false)) {
                Ok(ufs) => ufs.info(),
                Err(e) => {
                    log::warn!(
                        "{}: UFS2 superblock at byte {start} rejected: {e}",
                        dev.name()
                    );
                    return FsKind::None;
                }
            };
            if !(4096..=65536).contains(&info.bsize) || !(512..=info.bsize).contains(&info.fsize) {
                log::warn!(
                    "{}: UFS2 at byte {start} has an unusable block size {}/{}",
                    dev.name(),
                    info.bsize,
                    info.fsize
                );
                return FsKind::None;
            }
            let fs_len = info.frags.saturating_mul(u64::from(info.fsize));
            if fs_len > dev.len() - start {
                log::warn!(
                    "{}: UFS2 at byte {start} is larger than the disk, ignored",
                    dev.name()
                );
                return FsKind::None;
            }
            return FsKind::Found(Box::new(found(&info, big_endian)));
        }
    }
    if let Some(magic) = read_magic(dev, start + SBLOCK_UFS1 + MAGIC_OFFSET)
        && (u32::from_le_bytes(magic) == FS_UFS1_MAGIC
            || u32::from_be_bytes(magic) == FS_UFS1_MAGIC)
    {
        return FsKind::Unsupported("UFS1".into());
    }
    FsKind::None
}

pub struct Ufs;

impl Backend for Ufs {
    /// BSD labels first: in each MBR slice, and at the start of the disk
    /// without slices ("dangerously dedicated").
    fn candidates(&self, dev: &Device, sector: u64, table: Vec<Candidate>) -> Vec<Candidate> {
        let mut out = Vec::new();
        for c in table.iter().filter(|c| c.id.starts_with('s')) {
            let origin = format!("slice {}, ", &c.id[1..]);
            out.extend(label_candidates(
                dev, sector, c.start, c.len, &c.id, &origin,
            ));
        }
        out.extend(label_candidates(dev, sector, 0, dev.len(), "", ""));
        out.extend(table);
        out
    }

    fn probe(&self, dev: &Arc<Device>, start: u64, len: u64) -> FsKind {
        probe_fs(dev, start, len)
    }

    fn open(
        &self,
        dev: Arc<Device>,
        cand: &Candidate,
        found: &Found,
    ) -> anyhow::Result<Box<dyn Fs>> {
        let fsize = found.block_size;
        let reader = PartitionReader::new(dev, cand.start, cand.len, u64::from(fsize));
        let ufs = rufs::Ufs::new(BlockReader::new(reader, fsize as usize, false))
            .map_err(|e| anyhow::anyhow!("cannot open UFS2 on {}: {}", cand.id, describe(&e)))?;
        let info = ufs.info();
        if !info.clean {
            let flags = describe_flags(info.flags).join(", ");
            log::warn!(
                "the file system was not unmounted cleanly ({flags}). It is read as is; recently written files may be incomplete."
            );
        }
        Ok(Box::new(UfsFs { ufs, info }))
    }
}

struct UfsFs {
    ufs: rufs::Ufs<PartitionReader>,
    info: rufs::Info,
}

fn inode(id: NodeId) -> io::Result<InodeNum> {
    let n = u32::try_from(id).map_err(|_| io::Error::from_raw_os_error(22))?;
    // SAFETY: any number is safe to pass: rufs checks the range when it reads
    // the inode and fails with EIO.
    Ok(unsafe { InodeNum::new(n) })
}

fn kind(t: InodeType) -> Kind {
    match t {
        InodeType::RegularFile => Kind::File,
        InodeType::Directory => Kind::Dir,
        InodeType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    }
}

impl Fs for UfsFs {
    fn info(&self) -> Info {
        let i = &self.info;
        let fsize = u64::from(i.fsize);
        Info {
            label: i.volname.clone(),
            serial: i.id[0] ^ i.id[1],
            block_size: i.fsize,
            total: i.blocks.saturating_mul(fsize),
            free: i.bfree.saturating_mul(fsize),
            mount_point: i.fsmnt.clone(),
        }
    }

    fn root(&self) -> NodeId {
        InodeNum::ROOT.get64()
    }

    fn stat(&mut self, id: NodeId) -> io::Result<Stat> {
        let a = self.ufs.inode_attr(inode(id)?)?;
        let write = filetime(a.mtime);
        Ok(Stat {
            kind: kind(a.kind),
            size: a.size,
            alloc: a.blocks.saturating_mul(512),
            nlink: u32::from(a.nlink),
            // No birth time on the disk: show the write time.
            creation: if a.btime == UNIX_EPOCH {
                write
            } else {
                filetime(a.btime)
            },
            access: filetime(a.atime),
            write,
            change: filetime(a.ctime),
        })
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        f: &mut dyn FnMut(&[u8], NodeId, Kind) -> bool,
    ) -> io::Result<()> {
        let inr = inode(dir)?;
        let size = self.ufs.inode_attr(inr)?.size;
        if size > DIR_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("directory {dir}: size {size} cannot be real"),
            ));
        }
        self.ufs.dir_iter_bytes(inr, |name, inr, t| {
            f(name, inr.get64(), kind(t)).then_some(())
        })?;
        Ok(())
    }

    fn read(&mut self, id: NodeId, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.ufs.inode_read(inode(id)?, offset, buf)
    }

    fn read_link(&mut self, id: NodeId) -> io::Result<Vec<u8>> {
        self.ufs.symlink_read(inode(id)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disklabel sector. `parts` are (letter index, offset, size, fstype).
    fn label(npart: u16, parts: &[(usize, u32, u32, u8)], secsize: u32) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[0..4].copy_from_slice(&DISKMAGIC.to_le_bytes());
        s[132..136].copy_from_slice(&DISKMAGIC.to_le_bytes());
        s[40..44].copy_from_slice(&secsize.to_le_bytes());
        s[138..140].copy_from_slice(&npart.to_le_bytes());
        for &(i, offset, size, fstype) in parts {
            let e = &mut s[148 + 16 * i..164 + 16 * i];
            e[0..4].copy_from_slice(&size.to_le_bytes());
            e[4..8].copy_from_slice(&offset.to_le_bytes());
            e[12] = fstype;
        }
        let mut sum = 0u16;
        for i in (0..148 + 16 * npart as usize).step_by(2) {
            sum ^= le16(&s, i);
        }
        s[136..138].copy_from_slice(&sum.to_le_bytes());
        s
    }

    #[test]
    fn label_relative_and_absolute() {
        // The target disk: c at 0, a at 16, fstype 0.
        let l = label(8, &[(0, 16, 976773089, 0), (2, 0, 976773105, 0)], 512);
        let p = parse_label(&l, 512).unwrap();
        assert_eq!(
            p,
            vec![LabelPart {
                letter: 'a',
                offset: 16,
                sectors: 976773089,
                fstype: 0
            }]
        );

        // Older labels: absolute offsets, c starts at the slice start.
        let l = label(8, &[(0, 63, 100, 7), (1, 163, 50, 1), (2, 63, 150, 0)], 512);
        let p = parse_label(&l, 512).unwrap();
        assert_eq!(p[0].offset, 0);
        assert_eq!(p[1].offset, 100);
    }

    #[test]
    fn label_bounds() {
        assert!(parse_label(&label(21, &[], 512), 512).is_none());
        assert!(parse_label(&[0u8; 512], 512).is_none());
    }

    #[test]
    fn label_parser_never_panics() {
        let mut seed = 0x1234_5678u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for len in [0usize, 1, 135, 136, 147, 148, 300, 511, 512, 4096] {
            for _ in 0..200 {
                let mut buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                if len >= 136 && next() % 2 == 0 {
                    buf[0..4].copy_from_slice(&DISKMAGIC.to_le_bytes());
                    buf[132..136].copy_from_slice(&DISKMAGIC.to_le_bytes());
                }
                let _ = parse_label(&buf, 512);
            }
        }
    }

    #[test]
    fn ids_round_trip() {
        let root = inode(InodeNum::ROOT.get64()).unwrap();
        assert_eq!(root, InodeNum::ROOT);
        assert!(inode(u64::from(u32::MAX) + 1).is_err());
    }
}
