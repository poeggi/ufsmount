// SPDX-License-Identifier: AGPL-3.0-or-later
//! Find file systems on a disk: GPT partitions, MBR slices, Apple Partition
//! Map entries, the whole disk, and what the product's code adds (such as BSD
//! labels).
//!
//! Every candidate is checked by the product's code. Table type fields (MBR
//! type, GPT type GUID, APM type) are shown, never trusted.
//!
//! Checked against the FreeBSD, NetBSD, OpenBSD, Linux and Apple sources, so
//! presumably supported:
//! - MBR primary slices, and logical slices of an extended partition (types
//!   0x05, 0x0f, 0x85), numbered from s5 as FreeBSD does.
//! - GPT, from the primary header or the backup at the last LBA. A hybrid MBR
//!   (Boot Camp) lists the MBR slices too.
//! - Apple Partition Map with 512- or 2048-byte blocks. A map of larger blocks
//!   with an entry at byte 512 counts 512-byte blocks, as Apple does.
//! - Logical sectors of 512 and 4096 bytes.
//!
//! Not read: Apple's old "TS" maps, TiVo maps, Sun VTOC8, PC98.

use std::fmt;
use std::sync::Arc;

use crate::device::Device;
use crate::fs::{Backend, Found, FsKind};
use crate::product::product;

/// Logical slices followed in an extended partition at most.
const MAX_LOGICAL: usize = 128;
/// Apple Partition Map entries read at most, as libblkid does.
const MAX_APM: u64 = 256;
/// GPT partitions read at most.
const MAX_GPT: usize = 256;
/// Places checked for a file system at most, against crafted tables.
const MAX_CANDIDATES: usize = 1024;

/// Where a candidate comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Table {
    Whole,
    /// An MBR slice, primary or logical, and its type.
    Mbr(u8),
    /// A GPT partition and its type GUID in on-disk order.
    Gpt([u8; 16]),
    /// An Apple Partition Map entry and its type.
    Apm(String),
    /// Found by the product's code inside another candidate, such as a BSD label partition.
    Inner,
}

/// A place on the disk that may hold a file system.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// `whole`, `s1`, `s5`, `s1a`, `a`, `p2`, `p2a`, `m3`, `p2v1`.
    pub id: String,
    /// Byte offset on the device.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
    /// Where it came from, for display.
    pub origin: String,
    pub table: Table,
    pub fs: FsKind,
}

impl Candidate {
    pub fn found(&self) -> Option<&Found> {
        match &self.fs {
            FsKind::Found(found) => Some(found),
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
            FsKind::Found(found) => write!(f, "\n       {}", found.summary),
            FsKind::Unsupported(what) => write!(f, "\n       {what} (not supported)"),
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

pub fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}

pub fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

pub fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes(b[off..off + 2].try_into().unwrap())
}

pub fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbrEntry {
    pub index: usize,
    pub kind: u8,
    pub start_lba: u64,
    pub sectors: u64,
}

/// Entries of an MBR or EBR that describe a slice. Entries starting at sector 0
/// are skipped: FreeBSD's boot1 carries such a fake entry inside a bare partition.
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

fn is_extended(kind: u8) -> bool {
    matches!(kind, 0x05 | 0x0f | 0x85)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApmEntry {
    /// Entries in the map, as the entry says.
    pub map_entries: u32,
    /// Start and length in map blocks.
    pub start: u64,
    pub blocks: u64,
    pub name: String,
    pub kind: String,
}

/// An APM name or type field, up to the first NUL; other than printable ASCII shows as `?`.
fn apm_text(b: &[u8]) -> String {
    b.iter()
        .take_while(|&&c| c != 0)
        .map(|&c| {
            if c.is_ascii_graphic() || c == b' ' {
                c as char
            } else {
                '?'
            }
        })
        .collect()
}

/// An Apple Partition Map entry ("PM", big-endian).
pub fn parse_apm_entry(b: &[u8]) -> Option<ApmEntry> {
    if b.len() < 80 || be16(b, 0) != 0x504d {
        return None;
    }
    Some(ApmEntry {
        map_entries: be32(b, 4),
        start: u64::from(be32(b, 8)),
        blocks: u64::from(be32(b, 12)),
        name: apm_text(&b[16..48]),
        kind: apm_text(&b[48..80]),
    })
}

/// `len` bytes at `offset`, or None (logged) if they cannot be read.
pub fn read(dev: &Device, offset: u64, len: u64) -> Option<Vec<u8>> {
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

/// The GPT entries, from the primary header or, if allowed, the backup at the last LBA.
fn read_gpt(dev: &Device, sector: u64, try_backup: bool) -> Option<Vec<GptEntry>> {
    let sectors = dev.len() / sector;
    if sectors < 3 {
        return None;
    }
    let lbas: &[u64] = if try_backup { &[1, sectors - 1] } else { &[1] };
    for &lba in lbas {
        let Some(hdr) = read(dev, lba * sector, sector) else {
            continue;
        };
        let Some((entry_lba, count, size, crc)) = parse_gpt_header(&hdr) else {
            continue;
        };
        let Some(offset) = entry_lba.checked_mul(sector) else {
            continue;
        };
        let Some(array) = read(dev, offset, u64::from(count) * u64::from(size)) else {
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

/// A table range cut to the device: `None` if it starts outside, else its usable length.
/// Some USB bridges report a few sectors less than the disk was partitioned with.
/// A pass with another sector size than the device's logs at debug level.
pub fn clamp(dev: &Device, sector: u64, start: u64, len: u64, what: &str) -> Option<u64> {
    let level = if sector == dev.sector_size() {
        log::Level::Warn
    } else {
        log::Level::Debug
    };
    if start >= dev.len() {
        log::log!(
            level,
            "{}: {what} starts past the end of the disk, ignored",
            dev.name()
        );
        return None;
    }
    let avail = dev.len() - start;
    if len > avail {
        log::log!(
            level,
            "{}: {what} runs past the end of the disk, cut to the disk size",
            dev.name()
        );
    }
    Some(len.min(avail))
}

/// The logical slices of the extended partition at LBA `ext`, as s5, s6, ...
/// Each EBR holds a slice relative to itself and a link to the next EBR
/// relative to the extended partition. Links need not go forward.
fn logical_slices(dev: &Device, sector: u64, ext: u64, ext_sectors: u64, out: &mut Vec<Candidate>) {
    let end = ext.saturating_add(ext_sectors);
    let mut ebr = ext;
    let mut visited = vec![ebr];
    let mut index = 5;
    for _ in 0..MAX_LOGICAL {
        let Some(entries) = ebr
            .checked_mul(sector)
            .and_then(|at| read(dev, at, 512))
            .and_then(|s| parse_mbr(&s))
        else {
            return;
        };
        if let Some(e) = entries.iter().find(|e| !is_extended(e.kind)) {
            let what = format!("MBR slice {index}");
            if let (Some(start), Some(len)) = (
                ebr.checked_add(e.start_lba)
                    .and_then(|lba| lba.checked_mul(sector)),
                e.sectors.checked_mul(sector),
            ) && let Some(len) = clamp(dev, sector, start, len, &what)
            {
                out.push(Candidate {
                    id: format!("s{index}"),
                    start,
                    len,
                    origin: format!("MBR logical slice {index} (type 0x{:02x})", e.kind),
                    table: Table::Mbr(e.kind),
                    fs: FsKind::None,
                });
            }
            index += 1;
        }
        let next = entries
            .iter()
            .find(|e| is_extended(e.kind))
            .and_then(|e| ext.checked_add(e.start_lba));
        match next {
            Some(next) if next < end && !visited.contains(&next) => {
                visited.push(next);
                ebr = next;
            }
            _ => return,
        }
    }
}

/// The Apple Partition Map entries, if block 0 holds a driver descriptor ("ER").
/// Free space and the map itself are left out.
fn apm_candidates(dev: &Device, sector: u64) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Some(ddm) = read(dev, 0, 512) else {
        return out;
    };
    if be16(&ddm, 0) != 0x4552 {
        return out;
    }
    let block = u64::from(be16(&ddm, 2));
    let entry = |at: u64| read(dev, at, 512).and_then(|b| parse_apm_entry(&b));
    let unit = if entry(512).is_some() {
        512
    } else if block.is_power_of_two() && (1024..=4096).contains(&block) && entry(block).is_some() {
        block
    } else {
        return out;
    };
    let Some(first) = entry(unit) else {
        return out;
    };
    for n in 1..=u64::from(first.map_entries).min(MAX_APM) {
        let Some(e) = entry(n * unit) else {
            break;
        };
        let skip = ["Apple_Free", "Apple_partition_map"]
            .iter()
            .any(|t| e.kind.eq_ignore_ascii_case(t));
        if skip || e.start == 0 || e.blocks == 0 {
            continue;
        }
        let (Some(start), Some(len)) = (e.start.checked_mul(unit), e.blocks.checked_mul(unit))
        else {
            continue;
        };
        let Some(len) = clamp(dev, sector, start, len, &format!("APM partition {n}")) else {
            continue;
        };
        out.push(Candidate {
            id: format!("m{n}"),
            start,
            len,
            origin: format!("APM partition {n} ({}, \"{}\")", e.kind, e.name),
            table: Table::Apm(e.kind),
            fs: FsKind::None,
        });
    }
    out
}

/// Table candidates without file system checks: GPT partitions, MBR slices,
/// APM entries, then the whole disk.
fn table_candidates(dev: &Device, sector: u64) -> Vec<Candidate> {
    let mut out = Vec::new();

    let mbr = read(dev, 0, 512)
        .and_then(|s| parse_mbr(&s))
        .unwrap_or_default();
    let protective = mbr.iter().any(|e| e.kind == 0xee);
    let slices: Vec<&MbrEntry> = mbr.iter().filter(|e| e.kind != 0xee).collect();

    // A stale GPT next to a real MBR is harmless: every candidate is checked.
    if let Some(entries) = read_gpt(dev, sector, protective || slices.is_empty()) {
        if entries.len() > MAX_GPT {
            log::warn!(
                "{}: {} GPT partitions, only the first {MAX_GPT} are read",
                dev.name(),
                entries.len()
            );
        }
        for e in entries.into_iter().take(MAX_GPT) {
            let (Some(start), Some(len)) = (
                e.first_lba.checked_mul(sector),
                e.last_lba
                    .checked_sub(e.first_lba)
                    .and_then(|n| n.checked_add(1))
                    .and_then(|n| n.checked_mul(sector)),
            ) else {
                continue;
            };
            let Some(len) = clamp(
                dev,
                sector,
                start,
                len,
                &format!("GPT partition {}", e.index),
            ) else {
                continue;
            };
            let guid = guid_string(&e.type_guid);
            let kind = product()
                .gpt_types
                .iter()
                .find(|(g, _)| g.eq_ignore_ascii_case(&guid))
                .map_or(guid.clone(), |(_, name)| name.to_string());
            out.push(Candidate {
                id: format!("p{}", e.index),
                start,
                len,
                origin: format!("GPT partition {} ({kind})", e.index),
                table: Table::Gpt(e.type_guid),
                fs: FsKind::None,
            });
        }
    }
    let mut logical = Vec::new();
    for e in slices {
        let (Some(start), Some(len)) = (
            e.start_lba.checked_mul(sector),
            e.sectors.checked_mul(sector),
        ) else {
            continue;
        };
        let Some(len) = clamp(dev, sector, start, len, &format!("MBR slice {}", e.index)) else {
            continue;
        };
        if is_extended(e.kind) && logical.is_empty() {
            logical_slices(dev, sector, e.start_lba, e.sectors, &mut logical);
        }
        out.push(Candidate {
            id: format!("s{}", e.index),
            start,
            len,
            origin: format!("MBR slice {} (type 0x{:02x})", e.index, e.kind),
            table: Table::Mbr(e.kind),
            fs: FsKind::None,
        });
    }
    out.extend(logical);
    // ISO hybrids carry an MBR, a GPT and an APM together.
    out.extend(apm_candidates(dev, sector));
    out.push(Candidate {
        id: "whole".into(),
        start: 0,
        len: dev.len(),
        origin: "whole disk".into(),
        table: Table::Whole,
        fs: FsKind::None,
    });
    out
}

/// A file system larger than its partition entry is used at its own size.
fn fit(dev: &Device, c: &mut Candidate) {
    if let FsKind::Found(found) = &c.fs
        && found.size > c.len
    {
        log::warn!(
            "{}: the file system in {} is larger than its partition entry; using its own size",
            dev.name(),
            c.id
        );
        c.len = found.size;
    }
}

/// All candidates, with file system checks. Hits at the same start offset
/// are reported once, under the most specific id. Several file systems in one
/// place (a container) get the ids `<id>v1`, `<id>v2`, ...
pub fn probe(dev: &Arc<Device>) -> Vec<Candidate> {
    probe_with(dev, product().backend)
}

fn probe_with(dev: &Arc<Device>, backend: &dyn Backend) -> Vec<Candidate> {
    // A disk can report another sector size than it was partitioned with,
    // for example behind a USB bridge or in a different enclosure.
    let mut sizes = vec![dev.sector_size()];
    for s in [512, 4096] {
        if !sizes.contains(&s) {
            sizes.push(s);
        }
    }
    // The first listing that shows something, if no size finds a file system.
    let mut fallback: Option<(bool, Vec<Candidate>)> = None;
    for sector in sizes {
        let mut out = Vec::new();
        let mut seen = Vec::new();
        let mut cands = backend.candidates(dev, sector, table_candidates(dev, sector));
        if cands.len() > MAX_CANDIDATES {
            log::warn!(
                "{}: {} places to look at, only the first {MAX_CANDIDATES} are",
                dev.name(),
                cands.len()
            );
            cands.truncate(MAX_CANDIDATES);
        }
        for c in cands {
            if seen.contains(&c.start) {
                out.push(c);
                continue;
            }
            let found: Vec<FsKind> = backend
                .probe(dev, &c)
                .into_iter()
                .filter(|k| !matches!(k, FsKind::None))
                .collect();
            if found.is_empty() {
                out.push(c);
                continue;
            }
            seen.push(c.start);
            let several = found.len() > 1;
            for (i, fs) in found.into_iter().enumerate() {
                let mut v = c.clone();
                if several {
                    v.id = format!("{}v{}", c.id, i + 1);
                }
                v.fs = fs;
                fit(dev, &mut v);
                out.push(v);
            }
        }
        if out.iter().any(|c| c.found().is_some()) {
            if sector != dev.sector_size() {
                log::warn!(
                    "{}: the partition tables use {sector}-byte sectors, the disk reports {}",
                    dev.name(),
                    dev.sector_size()
                );
            }
            return out;
        }
        let unsupported = out.iter().any(|c| matches!(c.fs, FsKind::Unsupported(_)));
        if fallback.as_ref().is_none_or(|(u, _)| unsupported && !u) {
            fallback = Some((unsupported, out));
        }
    }
    fallback.map(|(_, out)| out).unwrap_or_default()
}

/// Pick the partition to use: the one named by `part`, or the only file system found.
pub fn select<'a>(cands: &'a [Candidate], part: Option<&str>) -> anyhow::Result<&'a Candidate> {
    let fs = product().fs_variant;
    let found: Vec<&Candidate> = cands.iter().filter(|c| c.found().is_some()).collect();
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
            match &c.fs {
                FsKind::Found(_) => Ok(c),
                FsKind::Unsupported(what) => {
                    anyhow::bail!("partition {id} holds {what}, which is not supported")
                }
                FsKind::None => anyhow::bail!("partition {id} holds no {fs} file system"),
            }
        }
        None => match found.as_slice() {
            [one] => Ok(one),
            [] => anyhow::bail!("no {fs} file system found. Candidates:\n{}", list()),
            _ => anyhow::bail!(
                "several {fs} file systems found, choose one with --part:\n{}",
                list()
            ),
        },
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::device::testing::FakeDisk;
    use crate::fs::Fs;

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

    const GUID: [u8; 16] = [
        0xb6, 0x7c, 0x6e, 0x51, 0xcf, 0x6e, 0xd6, 0x11, 0x8f, 0xf8, 0x00, 0x02, 0x2d, 0x09, 0x71,
        0x2b,
    ];

    #[test]
    fn gpt_guid() {
        assert_eq!(guid_string(&GUID), "516e7cb6-6ecf-11d6-8ff8-00022d09712b");
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
        entries[128..144].copy_from_slice(&GUID);
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
        for len in [0usize, 1, 79, 80, 91, 92, 147, 148, 300, 511, 512, 4096] {
            for _ in 0..200 {
                let mut buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                if len >= 8 && next() % 2 == 0 {
                    buf[0..8].copy_from_slice(b"EFI PART");
                }
                if len >= 2 && next() % 2 == 0 {
                    buf[0..2].copy_from_slice(b"PM");
                }
                let _ = parse_mbr(&buf);
                let _ = parse_apm_entry(&buf);
                // Sizes as parse_gpt_header allows them, with a matching CRC.
                let size = 128 + 8 * (next() % 497);
                let _ = parse_gpt_entries(&buf, size, crc32fast::hash(&buf));
                if let Some((_, _, size, crc)) = parse_gpt_header(&buf) {
                    let _ = parse_gpt_entries(&buf, size, crc);
                }
            }
        }
    }

    fn put(disk: &mut [u8], at: usize, data: &[u8]) {
        disk[at..at + data.len()].copy_from_slice(data);
    }

    /// Reports `n` file systems at each given start offset.
    struct At(Vec<(u64, u32)>);

    impl Backend for At {
        fn probe(&self, _: &Arc<Device>, c: &Candidate) -> Vec<FsKind> {
            let n = self
                .0
                .iter()
                .find(|(s, _)| *s == c.start)
                .map_or(0, |e| e.1);
            (0..n)
                .map(|volume| {
                    FsKind::Found(Box::new(Found {
                        id: format!("{}-{volume}", c.start),
                        size: 512,
                        block_size: 512,
                        volume,
                        summary: String::new(),
                    }))
                })
                .collect()
        }

        fn open(&self, _: Arc<Device>, _: &Candidate, _: &Found) -> anyhow::Result<Box<dyn Fs>> {
            anyhow::bail!("not here")
        }
    }

    fn ids(cands: &[Candidate]) -> Vec<(String, u64)> {
        cands
            .iter()
            .filter(|c| c.found().is_some())
            .map(|c| (c.id.clone(), c.start / 512))
            .collect()
    }

    #[test]
    fn logical_slices_follow_the_chain() {
        // Extended partition at 100. EBRs at 100, 450, 300: links count from
        // the extended partition and may go backwards. The last one links to
        // 450 again, which ends the chain. EBR 300 has its link first.
        let mut disk = vec![0u8; 700 * 512];
        put(&mut disk, 0, &mbr(&[(0xa5, 10, 50), (0x0f, 100, 500)]));
        put(
            &mut disk,
            100 * 512,
            &mbr(&[(0xa5, 10, 50), (0x05, 350, 100)]),
        );
        put(
            &mut disk,
            450 * 512,
            &mbr(&[(0x83, 20, 30), (0x05, 200, 100)]),
        );
        put(
            &mut disk,
            300 * 512,
            &mbr(&[(0x05, 350, 100), (0xa5, 5, 20)]),
        );
        let dev = FakeDisk::new(disk, 512).into_device();
        let at = At(vec![(110 * 512, 1), (470 * 512, 1), (305 * 512, 1)]);
        let cands = probe_with(&dev, &at);
        assert_eq!(
            ids(&cands),
            [
                ("s5".to_string(), 110),
                ("s6".to_string(), 470),
                ("s7".to_string(), 305)
            ]
        );
        assert_eq!(
            cands.iter().find(|c| c.id == "s6").unwrap().table,
            Table::Mbr(0x83)
        );
    }

    /// Reports an unsupported file system at one start offset.
    struct Old(u64);

    impl Backend for Old {
        fn probe(&self, _: &Arc<Device>, c: &Candidate) -> Vec<FsKind> {
            if c.start == self.0 {
                vec![FsKind::Unsupported("old".into())]
            } else {
                Vec::new()
            }
        }

        fn open(&self, _: Arc<Device>, _: &Candidate, _: &Found) -> anyhow::Result<Box<dyn Fs>> {
            anyhow::bail!("not here")
        }
    }

    #[test]
    fn sector_size_fallback() {
        // A 512-byte disk partitioned with 4096-byte sectors: slice 1 at 1 MiB.
        let mut disk = vec![0u8; 2 << 20];
        put(&mut disk, 0, &mbr(&[(0xa5, 256, 100)]));
        let dev = FakeDisk::new(disk, 512).into_device();
        let cands = probe_with(&dev, &At(vec![(1 << 20, 1)]));
        assert_eq!(ids(&cands), [("s1".to_string(), 2048)]);
        // Nothing found: the listing with the device's sector size.
        let cands = probe_with(&dev, &At(Vec::new()));
        assert_eq!(
            cands.iter().find(|c| c.id == "s1").unwrap().start,
            256 * 512
        );
        // An unsupported file system shows in the listing that finds it.
        let cands = probe_with(&dev, &Old(1 << 20));
        let s1 = cands.iter().find(|c| c.id == "s1").unwrap();
        assert!(matches!(s1.fs, FsKind::Unsupported(_)));
    }

    #[test]
    fn table_of_another_sector_size() {
        // A 4096-byte disk partitioned with 512-byte sectors: slice 1 at 1 MiB.
        let mut disk = vec![0u8; 2 << 20];
        put(&mut disk, 0, &mbr(&[(0xa5, 2048, 100)]));
        let dev = FakeDisk::new(disk, 4096).into_device();
        let cands = probe_with(&dev, &At(vec![(1 << 20, 1)]));
        assert_eq!(ids(&cands), [("s1".to_string(), 2048)]);
    }

    fn apm_entry(entries: u32, start: u32, blocks: u32, kind: &str) -> Vec<u8> {
        let mut e = vec![0u8; 512];
        e[0..2].copy_from_slice(b"PM");
        e[4..8].copy_from_slice(&entries.to_be_bytes());
        e[8..12].copy_from_slice(&start.to_be_bytes());
        e[12..16].copy_from_slice(&blocks.to_be_bytes());
        e[16..20].copy_from_slice(b"name");
        e[48..48 + kind.len()].copy_from_slice(kind.as_bytes());
        e
    }

    /// A map of `block`-byte blocks with its entries every `step` bytes.
    fn apm_disk(block: u16, step: usize) -> Vec<u8> {
        let mut disk = vec![0u8; 2048 * 64];
        disk[0..2].copy_from_slice(b"ER");
        disk[2..4].copy_from_slice(&block.to_be_bytes());
        let entries = [
            apm_entry(3, 1, 3, "Apple_partition_map"),
            apm_entry(3, 8, 8, "Apple_Free"),
            apm_entry(3, 16, 32, "Demo_FS"),
        ];
        for (i, e) in entries.iter().enumerate() {
            put(&mut disk, (i + 1) * step, e);
        }
        disk
    }

    #[test]
    fn apple_partition_map() {
        for (block, step, unit) in [(512, 512, 512), (2048, 2048, 2048), (2048, 512, 512)] {
            let dev = FakeDisk::new(apm_disk(block, step), 512).into_device();
            let cands = probe_with(&dev, &At(vec![(16 * unit, 1)]));
            assert_eq!(
                ids(&cands),
                [("m3".to_string(), 16 * unit / 512)],
                "block {block}, entries every {step}"
            );
            let m3 = cands.iter().find(|c| c.id == "m3").unwrap();
            assert_eq!(m3.len, 32 * unit);
            assert_eq!(m3.table, Table::Apm("Demo_FS".into()));
            assert!(!cands.iter().any(|c| c.id == "m1" || c.id == "m2"));
        }
    }

    #[test]
    fn several_file_systems_in_one_place() {
        let mut disk = vec![0u8; 100 * 512];
        put(&mut disk, 0, &mbr(&[(0xaf, 10, 50)]));
        let dev = FakeDisk::new(disk, 512).into_device();
        let cands = probe_with(&dev, &At(vec![(10 * 512, 2)]));
        assert_eq!(
            ids(&cands),
            [("s1v1".to_string(), 10), ("s1v2".to_string(), 10)]
        );
        let volumes: Vec<u32> = cands
            .iter()
            .filter_map(|c| c.found().map(|f| f.volume))
            .collect();
        assert_eq!(volumes, [0, 1]);
    }
}
