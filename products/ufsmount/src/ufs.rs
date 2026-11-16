// SPDX-License-Identifier: AGPL-3.0-or-later
//! UFS2 through rufs: finding it on a disk (BSD labels, the superblock) and reading it.
//!
//! Investigated against the FreeBSD, NetBSD, OpenBSD and DragonFly sources, so
//! presumably supported:
//! - FreeBSD: a label in an MBR slice (logical ones too) with offsets relative
//!   to the slice (c at 0) or absolute (4.x, c at the slice start); at the start
//!   of the disk ("dangerously dedicated"); in a GPT "freebsd" partition; in an
//!   APM "FreeBSD" entry (powerpc). GPT freebsd-ufs partitions and gmirror,
//!   glabel and gjournal providers hold UFS2 at their start.
//! - NetBSD (x86): a label in an MBR slice (0xa9) with absolute offsets, c the
//!   slice and d the disk; at the start of a disk without MBR. GPT netbsd-ffs
//!   partitions and macppc APM entries hold the file system directly.
//!   FFSv2, and FFSv2ea (NetBSD 10).
//! - OpenBSD: a label in an MBR slice (0xa6) or a GPT "openbsd" partition, with
//!   absolute offsets and c at 0; version 1 labels with 48-bit offsets; on
//!   4096-byte sectors at byte 512; big-endian in an APM "OPENBSD" entry (macppc).
//! - DragonFly: disklabel32 is read, but DragonFly makes only UFS1.
//!
//! Not read: disklabel64 (DragonFly, gpart BSD64), labels in sector 0 (alpha,
//! sparc, macppc without APM), OpenBSD's partitions past p and its labels in
//! GPT free space, Sun VTOC8 and PC98 tables, geli, gconcat, gstripe and
//! gvinum. UFS1 is found and refused.

use std::io;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use rufs::{BlockReader, InodeNum, InodeType};
use win_mountkit::device::{Device, PartitionReader};
use win_mountkit::fs::{Backend, Found, Fs, FsKind, Info, Kind, NodeId, Stat};
use win_mountkit::partition::{Candidate, Table, be16, be32, clamp, human_size, le16, le32, read};
use win_mountkit::volume::{describe, filetime};

const SBLOCK_UFS2: u64 = 65536;
const SBLOCK_UFS1: u64 = 8192;
const MAGIC_OFFSET: u64 = 1372;
const FS_UFS2_MAGIC: u32 = 0x1954_0119;
/// NetBSD's UFS2 with extended attributes (FFSv2ea).
const FS_UFS2EA_MAGIC: u32 = 0x1901_2038;
const FS_UFS1_MAGIC: u32 = 0x0001_1954;
const CG_MAGIC: u32 = 0x0009_0255;

const DISKMAGIC: u32 = 0x8256_4557;
/// FreeBSD allows 20, NetBSD and OpenBSD 16.
const MAXPARTITIONS: usize = 20;
const RAW_PART: usize = 2;

/// rufs reads a directory up to its claimed size, so that size is capped.
const DIR_MAX_BYTES: u64 = 1 << 30;

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

fn describe_ufs(info: &rufs::Info, big_endian: bool, ea: bool) -> String {
    let mut s = format!(
        "UFS2{}{}, {} KiB blocks, {} KiB fragments, {}",
        if big_endian { " (big-endian)" } else { "" },
        if ea { " (FFSv2ea)" } else { "" },
        info.bsize / 1024,
        info.fsize / 1024,
        human_size(info.frags * u64::from(info.fsize)),
    );
    if !info.label.is_empty() {
        s += &format!(", label \"{}\"", info.label.escape_ascii());
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
    /// Offset in sectors, as the label holds it.
    offset: u64,
    sectors: u64,
    fstype: u8,
}

#[derive(Debug, PartialEq, Eq)]
struct Label {
    /// The offset of c, the raw partition: the base of relative offsets.
    raw_offset: u64,
    /// OpenBSD's version 1 label, whose offsets count from the disk start.
    openbsd: bool,
    parts: Vec<LabelPart>,
}

/// A BSD disklabel, little-endian or big-endian (OpenBSD on big-endian
/// machines). `sector` is the logical sector size of the device.
fn parse_label(buf: &[u8], sector: u64) -> Option<Label> {
    if buf.len() < 148 {
        return None;
    }
    let le = le32(buf, 0) == DISKMAGIC && le32(buf, 132) == DISKMAGIC;
    if !le && (be32(buf, 0) != DISKMAGIC || be32(buf, 132) != DISKMAGIC) {
        return None;
    }
    let u16_at = |off| if le { le16(buf, off) } else { be16(buf, off) };
    let u32_at = |off| if le { le32(buf, off) } else { be32(buf, off) };
    // A label of another sector size belongs to another reading of the tables.
    let secsize = u64::from(u32_at(40));
    if secsize != sector {
        if secsize.is_power_of_two() && (512..=4096).contains(&secsize) {
            log::debug!("disklabel for {secsize}-byte sectors skipped");
            return None;
        }
        log::warn!("disklabel sector size {secsize} ignored");
    }
    let npart = usize::from(u16_at(138));
    if npart == 0 || npart > MAXPARTITIONS || buf.len() < 148 + 16 * npart {
        log::warn!("disklabel with {npart} partitions ignored");
        return None;
    }
    // XOR of all 16-bit words: zero in either byte order.
    let mut checksum = 0u16;
    for i in (0..148 + 16 * npart).step_by(2) {
        checksum ^= le16(buf, i);
    }
    if checksum != 0 {
        log::warn!("disklabel checksum mismatch (ignored)");
    }
    // OpenBSD's version 1 keeps 16 more bits of each offset and size where
    // FreeBSD keeps p_fsize; the version is in what FreeBSD leaves spare.
    let openbsd = u16_at(114) == 1;
    let entry = |i: usize| {
        let e = 148 + 16 * i;
        let (mut sectors, mut offset) = (u64::from(u32_at(e)), u64::from(u32_at(e + 4)));
        if openbsd {
            offset |= u64::from(u16_at(e + 8)) << 32;
            sectors |= u64::from(u16_at(e + 10)) << 32;
        }
        (sectors, offset, buf[e + 12])
    };
    let raw_offset = if npart > RAW_PART {
        entry(RAW_PART).1
    } else {
        0
    };
    let parts = (0..npart)
        .filter(|&i| i != RAW_PART)
        .filter_map(|i| {
            let (sectors, offset, fstype) = entry(i);
            (sectors != 0).then_some(LabelPart {
                letter: (b'a' + i as u8) as char,
                offset,
                sectors,
                fstype,
            })
        })
        .collect();
    Some(Label {
        raw_offset,
        openbsd,
        parts,
    })
}

/// A UFS superblock magic.
#[derive(Clone, Copy)]
enum Magic {
    Ufs2 { big_endian: bool, ea: bool },
    Ufs1,
}

/// The superblock magic of a file system starting at `start`, if any.
fn magic(dev: &Device, start: u64) -> Option<Magic> {
    let word = |off: u64| {
        start
            .checked_add(off + MAGIC_OFFSET)
            .and_then(|at| read(dev, at, 4))
            .map(|b| -> [u8; 4] { b.try_into().unwrap() })
    };
    if let Some(m) = word(SBLOCK_UFS2) {
        for (v, big_endian) in [
            (u32::from_le_bytes(m), false),
            (u32::from_be_bytes(m), true),
        ] {
            if v == FS_UFS2_MAGIC || v == FS_UFS2EA_MAGIC {
                let ea = v == FS_UFS2EA_MAGIC;
                return Some(Magic::Ufs2 { big_endian, ea });
            }
        }
    }
    let m = word(SBLOCK_UFS1)?;
    (u32::from_le_bytes(m) == FS_UFS1_MAGIC || u32::from_be_bytes(m) == FS_UFS1_MAGIC)
        .then_some(Magic::Ufs1)
}

/// Whether the UFS2 superblock of a file system at `start` is its primary
/// one: cylinder group 0 or the group's superblock copy follows it. A backup
/// superblock seen through a wrong offset has neither. A read error counts as yes.
fn is_primary(dev: &Device, start: u64, big_endian: bool) -> bool {
    let u32_at = |b: &[u8], off: usize| {
        let w: [u8; 4] = b[off..off + 4].try_into().unwrap();
        if big_endian {
            u32::from_be_bytes(w)
        } else {
            u32::from_le_bytes(w)
        }
    };
    let Some(sb) = start
        .checked_add(SBLOCK_UFS2)
        .and_then(|at| read(dev, at, 64))
    else {
        return true;
    };
    // A block number in the superblock at `field`, in fragments of fs_fsize.
    let block = |field: usize, len: u64| {
        u64::from(u32_at(&sb, field))
            .checked_mul(u64::from(u32_at(&sb, 52)))
            .and_then(|off| off.checked_add(start))
            .filter(|&at| at.checked_add(len).is_some_and(|end| end <= dev.len()))
    };
    // fs_cblkno: cg_magic and cg_cgx of group 0.
    let cg0 = block(12, 16).is_some_and(|at| {
        read(dev, at, 16).is_none_or(|cg| u32_at(&cg, 4) == CG_MAGIC && u32_at(&cg, 12) == 0)
    });
    // fs_sblkno: the magic of the group's superblock copy.
    let copy = block(8, MAGIC_OFFSET + 4).is_some_and(|at| {
        read(dev, at + MAGIC_OFFSET, 4)
            .is_none_or(|m| [FS_UFS2_MAGIC, FS_UFS2EA_MAGIC].contains(&u32_at(&m, 0)))
    });
    cg0 || copy
}

/// Whether a UFS file system, of any kind and byte order, starts at `start`.
fn has_superblock(dev: &Device, start: u64) -> bool {
    match magic(dev, start) {
        Some(Magic::Ufs2 { big_endian, .. }) => is_primary(dev, start, big_endian),
        Some(Magic::Ufs1) => true,
        None => false,
    }
}

/// The partitions of a BSD label in the second sector of `base`.
fn label_candidates(
    dev: &Device,
    sector: u64,
    base: u64,
    prefix: &str,
    origin: &str,
) -> Vec<Candidate> {
    // OpenBSD counts that sector in 512 bytes on larger sectors too.
    let mut at = vec![sector];
    if sector != 512 {
        at.push(512);
    }
    let Some(label) = at.into_iter().find_map(|off| {
        read(dev, base.checked_add(off)?, 512).and_then(|b| parse_label(&b, sector))
    }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in label.parts {
        // FreeBSD counts offsets from the slice, through c's offset. OpenBSD,
        // and NetBSD on some boards, count them from the disk start with c at
        // 0. Inside a slice with c at 0 both are possible: a superblock decides,
        // else the label's kind.
        let relative = p
            .offset
            .checked_sub(label.raw_offset)
            .and_then(|o| o.checked_mul(sector))
            .and_then(|o| o.checked_add(base));
        let absolute = if label.raw_offset == 0 && base > 0 {
            p.offset.checked_mul(sector)
        } else {
            None
        };
        let start = match (relative, absolute) {
            (Some(r), Some(a)) if r != a => {
                let (first, second) = if label.openbsd { (a, r) } else { (r, a) };
                if has_superblock(dev, first) || !has_superblock(dev, second) {
                    first
                } else {
                    second
                }
            }
            (Some(r), _) => r,
            (None, Some(a)) => a,
            (None, None) => continue,
        };
        let Some(len) = p.sectors.checked_mul(sector) else {
            continue;
        };
        let what = format!("label partition {prefix}{}", p.letter);
        let Some(len) = clamp(dev, sector, start, len, &what) else {
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
            table: Table::Inner,
            fs: FsKind::None,
        });
    }
    out
}

/// The id from the superblock; it stays the same on any port.
fn found(info: &rufs::Info, big_endian: bool, ea: bool) -> Found {
    Found {
        id: format!("{:08x}-{:08x}", info.id[0], info.id[1]),
        size: info.frags * u64::from(info.fsize),
        block_size: info.fsize,
        volume: 0,
        on_request: false,
        summary: describe_ufs(info, big_endian, ea),
    }
}

/// Check one candidate for a UFS superblock.
fn probe_fs(dev: &Arc<Device>, start: u64, len: u64) -> FsKind {
    if len < SBLOCK_UFS2 + 8192 {
        return FsKind::None;
    }
    let (big_endian, ea) = match magic(dev, start) {
        Some(Magic::Ufs2 { big_endian, ea }) => (big_endian, ea),
        Some(Magic::Ufs1) => return FsKind::Unsupported("UFS1".into()),
        None => return FsKind::None,
    };
    if !is_primary(dev, start, big_endian) {
        log::info!(
            "{}: backup UFS2 superblock at byte {start} ignored",
            dev.name()
        );
        return FsKind::None;
    }
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
    FsKind::Found(Box::new(found(&info, big_endian, ea)))
}

pub struct Ufs;

impl Backend for Ufs {
    /// BSD labels first: in MBR slices, GPT partitions and APM entries, and at
    /// the start of the disk without slices ("dangerously dedicated").
    fn candidates(&self, dev: &Device, sector: u64, table: Vec<Candidate>) -> Vec<Candidate> {
        let mut out = Vec::new();
        for c in &table {
            let origin = match c.table {
                Table::Mbr(_) => "slice",
                Table::Gpt(_) => "GPT partition",
                Table::Apm(_) => "APM partition",
                Table::Whole | Table::Inner => continue,
            };
            let origin = format!("{origin} {}, ", &c.id[1..]);
            out.extend(label_candidates(dev, sector, c.start, &c.id, &origin));
        }
        out.extend(label_candidates(dev, sector, 0, "", ""));
        out.extend(table);
        out
    }

    fn probe(&self, dev: &Arc<Device>, cand: &Candidate) -> Vec<FsKind> {
        vec![probe_fs(dev, cand.start, cand.len)]
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
            fs: "UFS",
            label: i.label.clone(),
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

    fn part(letter: char, offset: u64, sectors: u64, fstype: u8) -> LabelPart {
        LabelPart {
            letter,
            offset,
            sectors,
            fstype,
        }
    }

    #[test]
    fn label_relative_and_absolute() {
        // c at 0, a at 16 with fstype 0.
        let l = label(8, &[(0, 16, 976773089, 0), (2, 0, 976773105, 0)], 512);
        let l = parse_label(&l, 512).unwrap();
        assert_eq!(l.raw_offset, 0);
        assert!(!l.openbsd);
        assert_eq!(l.parts, [part('a', 16, 976773089, 0)]);

        // Older labels: absolute offsets, c starts at the slice start.
        let l = label(8, &[(0, 63, 100, 7), (1, 163, 50, 1), (2, 63, 150, 0)], 512);
        let l = parse_label(&l, 512).unwrap();
        assert_eq!(l.raw_offset, 63);
        assert_eq!(l.parts, [part('a', 63, 100, 7), part('b', 163, 50, 1)]);
    }

    #[test]
    fn openbsd_version_1_offsets() {
        let mut l = label(16, &[(0, 64, 100, 7), (2, 0, 1000, 0)], 512);
        // p_offseth and p_sizeh of a: the same bytes are p_fsize on FreeBSD.
        l[148 + 8..148 + 10].copy_from_slice(&1u16.to_le_bytes());
        l[148 + 10..148 + 12].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(
            parse_label(&l, 512).unwrap().parts[0],
            part('a', 64, 100, 7)
        );
        l[114..116].copy_from_slice(&1u16.to_le_bytes());
        let parsed = parse_label(&l, 512).unwrap();
        assert!(parsed.openbsd);
        assert_eq!(
            parsed.parts[0],
            part('a', (1 << 32) + 64, (2 << 32) + 100, 7)
        );
    }

    #[test]
    fn big_endian_label() {
        let mut s = vec![0u8; 512];
        s[0..4].copy_from_slice(&DISKMAGIC.to_be_bytes());
        s[132..136].copy_from_slice(&DISKMAGIC.to_be_bytes());
        s[40..44].copy_from_slice(&512u32.to_be_bytes());
        s[138..140].copy_from_slice(&16u16.to_be_bytes());
        s[114..116].copy_from_slice(&1u16.to_be_bytes());
        s[148..152].copy_from_slice(&100u32.to_be_bytes());
        s[152..156].copy_from_slice(&200u32.to_be_bytes());
        s[160] = 7;
        let l = parse_label(&s, 512).unwrap();
        assert!(l.openbsd);
        assert_eq!(l.parts, [part('a', 200, 100, 7)]);
    }

    #[test]
    fn label_of_another_sector_size() {
        let parts = [(0, 16, 100, 7), (2, 0, 200, 0)];
        assert!(parse_label(&label(8, &parts, 4096), 512).is_none());
        assert!(parse_label(&label(8, &parts, 4096), 4096).is_some());
        assert!(
            parse_label(&label(8, &parts, 0), 512).is_some(),
            "0 is ignored"
        );
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
                if len >= 136 {
                    let magic = match next() % 3 {
                        0 => DISKMAGIC.to_le_bytes(),
                        1 => DISKMAGIC.to_be_bytes(),
                        _ => [0; 4],
                    };
                    buf[0..4].copy_from_slice(&magic);
                    buf[132..136].copy_from_slice(&magic);
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
