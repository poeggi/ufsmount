// SPDX-License-Identifier: AGPL-3.0-or-later
//! Find UFS file systems on a disk: MBR slices, BSD disklabels, GPT, or a bare file system.
//!
//! Every candidate is checked for a UFS superblock. Table type fields (MBR type,
//! label fstype, GPT type GUID) are shown, never trusted.

use std::fmt;
use std::sync::Arc;

use rufs::{BlockReader, Ufs};

use crate::device::{Device, PartitionReader};

const SBLOCK_UFS2: u64 = 65536;
const SBLOCK_UFS1: u64 = 8192;
const MAGIC_OFFSET: u64 = 1372;
const FS_UFS2_MAGIC: u32 = 0x1954_0119;
const FS_UFS1_MAGIC: u32 = 0x0001_1954;

const DISKMAGIC: u32 = 0x8256_4557;
const MAXPARTITIONS: usize = 20;
const RAW_PART: usize = 2;

/// freebsd-ufs, 516e7cb6-6ecf-11d6-8ff8-00022d09712b, in on-disk (mixed-endian) order.
pub const GPT_FREEBSD_UFS: [u8; 16] = [
    0xb6, 0x7c, 0x6e, 0x51, 0xcf, 0x6e, 0xd6, 0x11, 0x8f, 0xf8, 0x00, 0x02, 0x2d, 0x09, 0x71, 0x2b,
];

/// What a candidate holds.
#[derive(Debug, Clone)]
pub enum FsKind {
    Ufs2(Box<UfsFacts>),
    /// UFS1 superblock found; not supported by rufs.
    Ufs1,
    None,
}

#[derive(Debug, Clone)]
pub struct UfsFacts {
    pub info: rufs::Info,
    pub big_endian: bool,
}

/// A place on the disk that may hold a file system.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// `whole`, `s1`, `s1a`, `a`, `p2`.
    pub id: String,
    /// Byte offset on the device.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
    /// Where it came from, for display.
    pub origin: String,
    pub fs: FsKind,
}

impl Candidate {
    pub fn ufs2(&self) -> Option<&UfsFacts> {
        match &self.fs {
            FsKind::Ufs2(facts) => Some(facts),
            _ => None,
        }
    }
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:<6} {:>14} {:>10}  {}",
            self.id,
            self.start,
            human_size(self.len),
            self.origin
        )?;
        match &self.fs {
            FsKind::Ufs2(facts) => write!(f, "\n       {}", describe_ufs(facts)),
            FsKind::Ufs1 => write!(f, "\n       UFS1 (not supported)"),
            FsKind::None => Ok(()),
        }
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Names of the `fs_flags` bits that matter to a reader.
pub fn describe_flags(flags: u32) -> Vec<&'static str> {
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

pub fn describe_ufs(facts: &UfsFacts) -> String {
    let info = &facts.info;
    let mut s = format!(
        "UFS2{}, {} KiB blocks, {} KiB fragments, {}",
        if facts.big_endian {
            " (big-endian)"
        } else {
            ""
        },
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

fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbrEntry {
    pub index: usize,
    pub kind: u8,
    pub start_lba: u64,
    pub sectors: u64,
}

/// Primary MBR entries that describe a slice. Entries starting at sector 0 are
/// skipped: FreeBSD's boot1 carries such a fake entry inside a bare UFS partition.
pub fn parse_mbr(sector0: &[u8]) -> Option<Vec<MbrEntry>> {
    if sector0.len() < 512 || sector0[510..512] != [0x55, 0xaa] {
        return None;
    }
    let entries = (0..4)
        .map(|i| {
            let e = &sector0[446 + 16 * i..462 + 16 * i];
            MbrEntry {
                index: i + 1,
                kind: e[4],
                start_lba: u64::from(le32(e, 8)),
                sectors: u64::from(le32(e, 12)),
            }
        })
        .filter(|e| e.kind != 0 && e.start_lba != 0 && e.sectors != 0)
        .collect();
    Some(entries)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelPart {
    pub letter: char,
    /// Offset in sectors, relative to the start of the slice.
    pub offset: u64,
    pub sectors: u64,
    pub fstype: u8,
}

/// A BSD disklabel. `sector` is the logical sector size of the device.
pub fn parse_label(buf: &[u8], sector: u64) -> Option<Vec<LabelPart>> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptEntry {
    pub index: usize,
    pub type_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
}

pub fn guid_string(g: &[u8; 16]) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{}",
        le32(g, 0),
        le16(g, 4),
        le16(g, 6),
        g[8],
        g[9],
        g[10..]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// A GPT header. Returns (entry LBA, number of entries, entry size, entry array CRC).
pub fn parse_gpt_header(buf: &[u8]) -> Option<(u64, u32, u32, u32)> {
    if buf.len() < 92 || &buf[0..8] != b"EFI PART" {
        return None;
    }
    let size = le32(buf, 12) as usize;
    if !(92..=buf.len()).contains(&size) {
        return None;
    }
    let mut hdr = buf[..size].to_vec();
    hdr[16..20].fill(0);
    if crc32fast::hash(&hdr) != le32(buf, 16) {
        log::warn!("GPT header CRC mismatch");
        return None;
    }
    let count = le32(buf, 80);
    let entry_size = le32(buf, 84);
    if count == 0
        || count > 4096
        || !(128..=4096).contains(&entry_size)
        || !entry_size.is_multiple_of(8)
    {
        log::warn!("GPT header with {count} entries of {entry_size} bytes ignored");
        return None;
    }
    Some((le64(buf, 72), count, entry_size, le32(buf, 88)))
}

pub fn parse_gpt_entries(buf: &[u8], entry_size: u32, crc: u32) -> Option<Vec<GptEntry>> {
    if crc32fast::hash(buf) != crc {
        log::warn!("GPT entry array CRC mismatch");
        return None;
    }
    let entries = buf
        .chunks_exact(entry_size as usize)
        .enumerate()
        .filter_map(|(i, e)| {
            let type_guid: [u8; 16] = e[0..16].try_into().unwrap();
            if type_guid == [0; 16] {
                return None;
            }
            Some(GptEntry {
                index: i + 1,
                type_guid,
                first_lba: le64(e, 32),
                last_lba: le64(e, 40),
            })
        })
        .collect();
    Some(entries)
}

fn read(dev: &Device, offset: u64, len: u64) -> Option<Vec<u8>> {
    if offset.checked_add(len)? > dev.len() {
        return None;
    }
    let mut buf = vec![0; len as usize];
    match dev.read(offset, &mut buf) {
        Ok(()) => Some(buf),
        Err(e) => {
            log::warn!("{}: cannot read {len} bytes at {offset}: {e}", dev.name());
            None
        }
    }
}

fn read_gpt(dev: &Device, sector: u64) -> Option<Vec<GptEntry>> {
    let last_lba = dev.len() / sector - 1;
    for lba in [1, last_lba] {
        let Some(hdr) = read(dev, lba * sector, sector) else {
            continue;
        };
        let Some((entry_lba, count, size, crc)) = parse_gpt_header(&hdr) else {
            continue;
        };
        let Some(array) = read(
            dev,
            entry_lba.checked_mul(sector)?,
            u64::from(count) * u64::from(size),
        ) else {
            continue;
        };
        if let Some(entries) = parse_gpt_entries(&array, size, crc) {
            if lba != 1 {
                log::warn!("{}: primary GPT is damaged, using the backup", dev.name());
            }
            return Some(entries);
        }
    }
    None
}

/// Table candidates without file system checks.
fn table_candidates(dev: &Device, sector: u64) -> Vec<Candidate> {
    let mut labels = Vec::new();
    let mut others = Vec::new();
    let in_device =
        |start: u64, len: u64| start.checked_add(len).is_some_and(|end| end <= dev.len());

    let add_label =
        |labels: &mut Vec<Candidate>, base: u64, base_len: u64, prefix: &str, origin: &str| {
            let Some(buf) = read(dev, base + sector, sector.max(512)) else {
                return;
            };
            let Some(parts) = parse_label(&buf, sector) else {
                return;
            };
            for p in parts {
                let (Some(start), Some(len)) = (
                    p.offset
                        .checked_mul(sector)
                        .and_then(|o| o.checked_add(base)),
                    p.sectors.checked_mul(sector),
                ) else {
                    continue;
                };
                if start + len > base + base_len || !in_device(start, len) {
                    log::warn!(
                        "{}: label partition {} lies outside its slice, ignored",
                        dev.name(),
                        p.letter
                    );
                    continue;
                }
                labels.push(Candidate {
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
        };

    if let Some(entries) = read_gpt(dev, sector) {
        for e in entries {
            let Some(len) = e
                .last_lba
                .checked_sub(e.first_lba)
                .and_then(|n| (n + 1).checked_mul(sector))
            else {
                continue;
            };
            let start = e.first_lba.saturating_mul(sector);
            if !in_device(start, len) {
                continue;
            }
            let kind = if e.type_guid == GPT_FREEBSD_UFS {
                "freebsd-ufs".to_string()
            } else {
                guid_string(&e.type_guid)
            };
            others.push(Candidate {
                id: format!("p{}", e.index),
                start,
                len,
                origin: format!("GPT partition {} ({kind})", e.index),
                fs: FsKind::None,
            });
        }
    } else if let Some(sector0) = read(dev, 0, sector.max(512))
        && let Some(entries) = parse_mbr(&sector0)
    {
        for e in entries {
            if e.kind == 0xee {
                continue;
            }
            let (Some(start), Some(len)) = (
                e.start_lba.checked_mul(sector),
                e.sectors.checked_mul(sector),
            ) else {
                continue;
            };
            if !in_device(start, len) {
                log::warn!(
                    "{}: MBR slice {} lies outside the disk, ignored",
                    dev.name(),
                    e.index
                );
                continue;
            }
            let id = format!("s{}", e.index);
            if e.kind == 0xa5 {
                add_label(
                    &mut labels,
                    start,
                    len,
                    &id,
                    &format!("slice {}, ", e.index),
                );
            }
            others.push(Candidate {
                id,
                start,
                len,
                origin: format!("MBR slice {} (type 0x{:02x})", e.index, e.kind),
                fs: FsKind::None,
            });
        }
    }
    // A label at the start of the disk, without slices ("dangerously dedicated").
    add_label(&mut labels, 0, dev.len(), "", "");

    labels.extend(others);
    labels.push(Candidate {
        id: "whole".into(),
        start: 0,
        len: dev.len(),
        origin: "whole disk".into(),
        fs: FsKind::None,
    });
    labels
}

fn read_magic(dev: &Device, offset: u64) -> Option<[u8; 4]> {
    read(dev, offset, 4).map(|b| b.try_into().unwrap())
}

/// Check one candidate for a UFS superblock.
pub fn probe_fs(dev: &Arc<Device>, start: u64, len: u64) -> FsKind {
    if len < SBLOCK_UFS2 + 8192 {
        return FsKind::None;
    }
    if let Some(magic) = read_magic(dev, start + SBLOCK_UFS2 + MAGIC_OFFSET) {
        let big_endian = u32::from_be_bytes(magic) == FS_UFS2_MAGIC;
        if big_endian || u32::from_le_bytes(magic) == FS_UFS2_MAGIC {
            let reader = PartitionReader::new(dev.clone(), start, len, 512);
            match Ufs::new(BlockReader::new(reader, 512, false)) {
                Ok(ufs) => {
                    let info = ufs.info();
                    if info.frags.saturating_mul(u64::from(info.fsize)) > len {
                        log::warn!(
                            "{}: UFS2 at byte {start} is larger than its partition, ignored",
                            dev.name()
                        );
                        return FsKind::None;
                    }
                    return FsKind::Ufs2(Box::new(UfsFacts { info, big_endian }));
                }
                Err(e) => {
                    log::warn!(
                        "{}: UFS2 superblock at byte {start} rejected: {e}",
                        dev.name()
                    );
                    return FsKind::None;
                }
            }
        }
    }
    if let Some(magic) = read_magic(dev, start + SBLOCK_UFS1 + MAGIC_OFFSET)
        && (u32::from_le_bytes(magic) == FS_UFS1_MAGIC
            || u32::from_be_bytes(magic) == FS_UFS1_MAGIC)
    {
        return FsKind::Ufs1;
    }
    FsKind::None
}

/// All candidates, with file system checks. UFS hits at the same start offset
/// are reported once, under the most specific id (label, then GPT or slice, then whole).
pub fn probe(dev: &Arc<Device>) -> Vec<Candidate> {
    let mut sizes = vec![dev.sector_size()];
    if dev.sector_size() == 512 {
        sizes.push(4096);
    }
    let mut result = Vec::new();
    for sector in sizes {
        let mut cands = table_candidates(dev, sector);
        let mut seen = Vec::new();
        for c in &mut cands {
            if seen.contains(&c.start) {
                continue;
            }
            c.fs = probe_fs(dev, c.start, c.len);
            if !matches!(c.fs, FsKind::None) {
                seen.push(c.start);
            }
        }
        let found_table = cands.len() > 1;
        result = cands;
        if found_table || result.iter().any(|c| c.ufs2().is_some()) {
            break;
        }
    }
    result
}

/// Pick the partition to use: the one named by `part`, or the only UFS2 one.
pub fn select<'a>(cands: &'a [Candidate], part: Option<&str>) -> anyhow::Result<&'a Candidate> {
    let ufs: Vec<&Candidate> = cands.iter().filter(|c| c.ufs2().is_some()).collect();
    let list = || {
        cands
            .iter()
            .map(|c| format!("  {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    match part {
        Some(id) => {
            let c = cands
                .iter()
                .find(|c| c.id.eq_ignore_ascii_case(id))
                .ok_or_else(|| anyhow::anyhow!("no partition \"{id}\". Found:\n{}", list()))?;
            match c.fs {
                FsKind::Ufs2(_) => Ok(c),
                FsKind::Ufs1 => anyhow::bail!("partition {id} holds UFS1, which is not supported"),
                FsKind::None => anyhow::bail!("partition {id} holds no UFS2 file system"),
            }
        }
        None => match ufs.as_slice() {
            [one] => Ok(one),
            [] => anyhow::bail!("no UFS2 file system found. Candidates:\n{}", list()),
            _ => anyhow::bail!(
                "several UFS2 file systems found, choose one with --part:\n{}",
                list()
            ),
        },
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn mbr(entries: &[(u8, u32, u32)]) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        for (i, &(kind, start, count)) in entries.iter().enumerate() {
            let e = &mut s[446 + 16 * i..462 + 16 * i];
            e[4] = kind;
            e[8..12].copy_from_slice(&start.to_le_bytes());
            e[12..16].copy_from_slice(&count.to_le_bytes());
        }
        s[510] = 0x55;
        s[511] = 0xaa;
        s
    }

    /// A disklabel sector. `parts` are (letter index, offset, size, fstype).
    pub fn label(npart: u16, parts: &[(usize, u32, u32, u8)], secsize: u32) -> Vec<u8> {
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
    fn mbr_skips_empty_and_boot1_entries() {
        let s = mbr(&[(0xa5, 63, 1000), (0xa5, 0, 50000), (0, 2000, 10)]);
        let e = parse_mbr(&s).unwrap();
        assert_eq!(
            e,
            vec![MbrEntry {
                index: 1,
                kind: 0xa5,
                start_lba: 63,
                sectors: 1000
            }]
        );
        assert!(parse_mbr(&[0u8; 512]).is_none());
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
    fn gpt_guid() {
        assert_eq!(
            guid_string(&GPT_FREEBSD_UFS),
            "516e7cb6-6ecf-11d6-8ff8-00022d09712b"
        );
    }

    pub fn gpt_header(entry_lba: u64, count: u32, entries: &[u8]) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[0..8].copy_from_slice(b"EFI PART");
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[72..80].copy_from_slice(&entry_lba.to_le_bytes());
        h[80..84].copy_from_slice(&count.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&crc32fast::hash(entries).to_le_bytes());
        let crc = crc32fast::hash(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    }

    #[test]
    fn gpt_parse_and_crc() {
        let mut entries = vec![0u8; 128 * 4];
        entries[128..144].copy_from_slice(&GPT_FREEBSD_UFS);
        entries[128 + 32..128 + 40].copy_from_slice(&40u64.to_le_bytes());
        entries[128 + 40..128 + 48].copy_from_slice(&1000u64.to_le_bytes());
        let h = gpt_header(2, 4, &entries);
        let (lba, count, size, crc) = parse_gpt_header(&h).unwrap();
        assert_eq!((lba, count, size), (2, 4, 128));
        let e = parse_gpt_entries(&entries, size, crc).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].index, e[0].first_lba, e[0].last_lba), (2, 40, 1000));

        let mut bad = h.clone();
        bad[80] ^= 1;
        assert!(parse_gpt_header(&bad).is_none());
        let mut bad_entries = entries.clone();
        bad_entries[200] ^= 1;
        assert!(parse_gpt_entries(&bad_entries, size, crc).is_none());
    }

    #[test]
    fn parsers_never_panic() {
        let mut seed = 0x1234_5678u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for len in [0usize, 1, 91, 92, 147, 148, 300, 511, 512, 4096] {
            for _ in 0..200 {
                let mut buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                if len >= 8 && next() % 2 == 0 {
                    buf[0..8].copy_from_slice(b"EFI PART");
                }
                if len >= 136 && next() % 2 == 0 {
                    buf[0..4].copy_from_slice(&DISKMAGIC.to_le_bytes());
                    buf[132..136].copy_from_slice(&DISKMAGIC.to_le_bytes());
                }
                let _ = parse_mbr(&buf);
                let _ = parse_label(&buf, 512);
                if let Some((_, _, size, crc)) = parse_gpt_header(&buf) {
                    let _ = parse_gpt_entries(&buf, size, crc);
                }
            }
        }
    }
}
