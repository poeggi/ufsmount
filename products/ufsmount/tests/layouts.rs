// SPDX-License-Identifier: AGPL-3.0-or-later
//! Partition layouts built around the upstream image, and byte-patched images.

mod common;

use common::*;

const SECTOR: usize = 512;

fn mbr(entries: &[(u8, u32, u32)]) -> Vec<u8> {
    let mut s = vec![0u8; SECTOR];
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

/// A little-endian disklabel for 512-byte sectors. `parts` are (index, offset, size, fstype).
fn label(parts: &[(usize, u32, u32, u8)]) -> Vec<u8> {
    disklabel(parts, 512, false, 0)
}

/// A disklabel with 16 partitions, in either byte order. OpenBSD writes version 1.
fn disklabel(parts: &[(usize, u32, u32, u8)], secsize: u32, big: bool, version: u16) -> Vec<u8> {
    let u32b = |v: u32| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let u16b = |v: u16| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let mut s = vec![0u8; SECTOR];
    s[0..4].copy_from_slice(&u32b(0x8256_4557));
    s[132..136].copy_from_slice(&u32b(0x8256_4557));
    s[40..44].copy_from_slice(&u32b(secsize));
    s[114..116].copy_from_slice(&u16b(version));
    s[138..140].copy_from_slice(&u16b(16));
    for &(i, offset, size, fstype) in parts {
        let e = &mut s[148 + 16 * i..164 + 16 * i];
        e[0..4].copy_from_slice(&u32b(size));
        e[4..8].copy_from_slice(&u32b(offset));
        e[12] = fstype;
    }
    // The XOR of all 16-bit words is zero, in either byte order.
    let sum = (0..148 + 16 * 16)
        .step_by(2)
        .fold(0u16, |x, i| x ^ u16::from_le_bytes([s[i], s[i + 1]]));
    s[136..138].copy_from_slice(&sum.to_le_bytes());
    s
}

/// Sectors an image takes, rounded up.
fn sectors(img: &[u8], size: usize) -> u32 {
    img.len().div_ceil(size) as u32
}

fn put(disk: &mut [u8], lba: usize, data: &[u8]) {
    disk[lba * SECTOR..lba * SECTOR + data.len()].copy_from_slice(data);
}

/// MBR slice at LBA 63, BSD label inside, the image at `a`'s offset.
fn sliced_disk(img: &[u8], a_rel: u32, absolute: bool) -> Vec<u8> {
    let img_sectors = (img.len() / SECTOR) as u32;
    let slice_len = a_rel + img_sectors;
    let mut disk = vec![0u8; (63 + slice_len as usize) * SECTOR];
    put(&mut disk, 0, &mbr(&[(0xa5, 63, slice_len)]));
    let base = if absolute { 63 } else { 0 };
    let mut parts = vec![(0, base + a_rel, img_sectors, 0), (2, base, slice_len, 0)];
    if absolute {
        // NetBSD: d is the whole disk.
        parts.push((3, 0, 63 + slice_len, 0));
    }
    // The first 64 KiB of a UFS2 partition are unused, so the label may sit inside it.
    let lbl = label(&parts);
    put(&mut disk, (63 + a_rel) as usize, img);
    put(&mut disk, 64, &lbl);
    disk
}

#[test]
fn target_disk_layout() {
    // Slice at LBA 63, label a at +16 with fstype 0, c at 0.
    let img = image_bytes("ufs-little");
    let path = write_temp("sliced", &sliced_disk(&img, 16, false));
    let (vol, id) = open(&path, None);
    assert_eq!(id, "s1a");
    assert_eq!(root_names(&vol), ROOT_NAMES);
    assert_eq!(read_all(&vol, r"\file1"), b"This is a simple file.\n");
}

#[test]
fn label_with_absolute_offsets() {
    let img = image_bytes("ufs-little");
    let path = write_temp("absolute", &sliced_disk(&img, 16, true));
    let (vol, id) = open(&path, None);
    assert_eq!(id, "s1a");
    assert_eq!(root_names(&vol), ROOT_NAMES);
}

#[test]
fn partition_a_at_slice_start() {
    // a and c both at 0: the same file system must be offered once, as s1a.
    let img = image_bytes("ufs-little");
    let path = write_temp("a0", &sliced_disk(&img, 0, false));
    let (vol, id) = open(&path, None);
    assert_eq!(id, "s1a");
    assert_eq!(root_names(&vol), ROOT_NAMES);
}

#[test]
fn boot1_fake_mbr_entry() {
    // FreeBSD boot1 in sector 0 of a bare UFS: 0x55AA and a slice starting at 0.
    let mut img = image_bytes("ufs-little");
    img[..SECTOR].copy_from_slice(&mbr(&[(0xa5, 0, 50000)]));
    let path = write_temp("boot1", &img);
    let (vol, id) = open(&path, None);
    assert_eq!(id, "whole");
    assert_eq!(root_names(&vol), ROOT_NAMES);
}

fn gpt_header(my_lba: u64, entry_lba: u64, entries: &[u8]) -> Vec<u8> {
    let mut h = vec![0u8; SECTOR];
    h[0..8].copy_from_slice(b"EFI PART");
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    h[12..16].copy_from_slice(&92u32.to_le_bytes());
    h[24..32].copy_from_slice(&my_lba.to_le_bytes());
    h[72..80].copy_from_slice(&entry_lba.to_le_bytes());
    h[80..84].copy_from_slice(&4u32.to_le_bytes());
    h[84..88].copy_from_slice(&128u32.to_le_bytes());
    h[88..92].copy_from_slice(&crc32fast::hash(entries).to_le_bytes());
    let crc = crc32fast::hash(&h[..92]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    h
}

/// A GPT type GUID in on-disk byte order.
fn guid(s: &str) -> [u8; 16] {
    let hex = s.replace('-', "");
    let mut g: [u8; 16] = (0..16)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap();
    g[0..4].reverse();
    g[4..6].reverse();
    g[6..8].reverse();
    g
}

/// A disk of `total` sectors with one GPT partition from `first` to the
/// last usable LBA.
fn gpt_layout(total: usize, kind: &str, first: usize) -> Vec<u8> {
    let mut disk = vec![0u8; total * SECTOR];
    put(&mut disk, 0, &mbr(&[(0xee, 1, (total - 1) as u32)]));
    let mut entries = vec![0u8; 4 * 128];
    entries[0..16].copy_from_slice(&guid(kind));
    entries[32..40].copy_from_slice(&(first as u64).to_le_bytes());
    entries[40..48].copy_from_slice(&((total - 34) as u64).to_le_bytes());
    put(&mut disk, 1, &gpt_header(1, 2, &entries));
    put(&mut disk, 2, &entries);
    put(
        &mut disk,
        total - 1,
        &gpt_header((total - 1) as u64, 2, &entries),
    );
    disk
}

const FREEBSD_UFS: &str = "516e7cb6-6ecf-11d6-8ff8-00022d09712b";

fn gpt_disk(img: &[u8]) -> Vec<u8> {
    let total = 40 + sectors(img, SECTOR) as usize + 34;
    let mut disk = gpt_layout(total, FREEBSD_UFS, 40);
    put(&mut disk, 40, img);
    disk
}

#[test]
fn gpt() {
    let img = image_bytes("ufs-big");
    let path = write_temp("gpt", &gpt_disk(&img));
    let (vol, id) = open(&path, None);
    assert_eq!(id, "p1");
    assert_eq!(root_names(&vol), ROOT_NAMES);
}

#[test]
fn gpt_damaged_primary() {
    let img = image_bytes("ufs-little");
    let mut disk = gpt_disk(&img);
    disk[SECTOR + 30] ^= 0xff;
    let path = write_temp("gpt-backup", &disk);
    let (vol, id) = open(&path, None);
    assert_eq!(id, "p1");
    assert_eq!(root_names(&vol), ROOT_NAMES);
}

#[test]
fn explicit_part_and_errors() {
    let img = image_bytes("ufs-little");
    let path = write_temp("explicit", &sliced_disk(&img, 16, false));
    let (_, id) = open(&path, Some("S1A"));
    assert_eq!(id, "s1a");

    let dev = std::sync::Arc::new(
        win_mountkit::device::Device::open(
            &win_mountkit::device::Source::Image(path.to_path_buf()),
            None,
        )
        .unwrap(),
    );
    let cands = win_mountkit::partition::probe(&dev);
    assert!(win_mountkit::partition::select(&cands, Some("s1")).is_err());
    assert!(win_mountkit::partition::select(&cands, Some("p9")).is_err());
}

/// The summary of the file system `probe` finds in an image.
fn summary(path: &std::path::Path) -> String {
    let dev = std::sync::Arc::new(
        win_mountkit::device::Device::open(
            &win_mountkit::device::Source::Image(path.to_path_buf()),
            None,
        )
        .unwrap(),
    );
    let cands = win_mountkit::partition::probe(&dev);
    cands
        .iter()
        .find_map(|c| c.found())
        .unwrap()
        .summary
        .clone()
}

#[test]
fn not_clean() {
    // fs_clean: 0 after a crash; NetBSD keeps 2 while mounted.
    for value in [0, 2] {
        let mut img = image_bytes("ufs-little");
        img[65536 + 209] = value;
        let path = write_temp("dirty", &img);
        let (vol, _) = open(&path, None);
        let s = summary(&path);
        assert!(s.contains("NOT clean"), "fs_clean {value}: {s}");
        assert_eq!(root_names(&vol), ROOT_NAMES);
    }
}

#[test]
fn netbsd_ffsv2ea() {
    let mut img = image_bytes("ufs-little");
    img[65536 + 1372..65536 + 1376].copy_from_slice(&0x1901_2038u32.to_le_bytes());
    let path = write_temp("ffsv2ea", &img);
    let (vol, id) = open(&path, None);
    assert_eq!(id, "whole");
    assert!(summary(&path).contains("(FFSv2ea)"));
    assert_eq!(root_names(&vol), ROOT_NAMES);
    assert_eq!(read_all(&vol, r"\file1"), b"This is a simple file.\n");
}

#[test]
fn odd_names() {
    let mut img = image_bytes("ufs-little");
    let at = find(&img, b"\x08\x05file1") + 2;
    img[at] = 0xff;
    let at = find(&img, b"\x08\x05file3") + 4;
    img[at] = b':';
    let path = write_temp("names", &img);
    let (vol, _) = open(&path, None);
    let names = root_names(&vol);
    assert!(names.contains(&"\u{f1ff}ile1".to_string()), "{names:?}");
    assert!(names.contains(&"fi\u{f03a}e3".to_string()), "{names:?}");
    assert_eq!(
        read_all(&vol, "\\\u{f1ff}ile1"),
        b"This is a simple file.\n"
    );
}

#[test]
fn deleted_first_entry() {
    // Zero the inode number of ".", the first entry of the root directory's
    // first chunk. Everything behind it must still be listed.
    let mut img = image_bytes("ufs-little");
    let at = find(&img, b"\x02\x00\x00\x00\x0c\x00\x04\x01.");
    img[at..at + 4].fill(0);
    let path = write_temp("deleted", &img);
    let (vol, _) = open(&path, None);
    assert_eq!(root_names(&vol), ROOT_NAMES);
}

/// Open `disk`, expect the file system under `id`, and list it.
fn check(tag: &str, disk: &[u8], id: &str) {
    let path = write_temp(tag, disk);
    let (vol, found) = open(&path, None);
    assert_eq!(found, id, "{tag}");
    assert_eq!(root_names(&vol), ROOT_NAMES, "{tag}");
}

#[test]
fn dangerously_dedicated() {
    // A FreeBSD label at the disk start, without an MBR and with boot1's
    // fake one (slice 4 from 0).
    let img = image_bytes("ufs-little");
    let n = sectors(&img, SECTOR);
    let mut disk = vec![0u8; (16 + n as usize) * SECTOR];
    put(&mut disk, 1, &label(&[(0, 16, n, 7), (2, 0, 16 + n, 0)]));
    put(&mut disk, 16, &img);
    check("dedicated", &disk, "a");
    let boot1 = [(0, 0, 0), (0, 0, 0), (0, 0, 0), (0xa5, 0, 50000)];
    put(&mut disk, 0, &mbr(&boot1));
    check("dedicated-boot1", &disk, "a");
}

#[test]
fn damaged_cylinder_group_0() {
    // The header of group 0 is gone; its superblock copy shows the
    // superblock is the primary one.
    let mut img = image_bytes("ufs-little");
    let field =
        |off: usize| u32::from_le_bytes(img[65536 + off..65536 + off + 4].try_into().unwrap());
    let cg0 = (field(12) * field(52)) as usize;
    img[cg0..cg0 + 16].fill(0);
    check("cg0", &img, "whole");
}

#[test]
fn logical_slice() {
    // s1, then an extended partition at 200 with EBRs at 200 and 600:
    // s5 at 263, s6 at 663 with a FreeBSD label.
    let img = image_bytes("ufs-little");
    let n = sectors(&img, SECTOR);
    let total = 663 + 16 + n;
    let mut disk = vec![0u8; total as usize * SECTOR];
    put(
        &mut disk,
        0,
        &mbr(&[(0x0c, 63, 100), (0x0f, 200, total - 200)]),
    );
    put(
        &mut disk,
        200,
        &mbr(&[(0x83, 63, 100), (0x05, 400, total - 600)]),
    );
    put(&mut disk, 600, &mbr(&[(0xa5, 63, 16 + n)]));
    put(&mut disk, 664, &label(&[(0, 16, n, 7), (2, 0, 16 + n, 0)]));
    put(&mut disk, 679, &img);
    check("logical", &disk, "s6a");
}

/// Add an OpenBSD label to `disk`, whose table has a partition at LBA 64:
/// in the partition's second sector, offsets from the disk start, c the
/// whole disk, the file system in d at 128. OpenBSD writes version 1.
fn openbsd(img: &[u8], big: bool, version: u16, mut disk: Vec<u8>) -> Vec<u8> {
    let n = sectors(img, SECTOR);
    let total = (disk.len() / SECTOR) as u32;
    let lbl = disklabel(
        &[(0, 64, 64, 7), (2, 0, total, 0), (3, 128, n, 7)],
        512,
        big,
        version,
    );
    put(&mut disk, 65, &lbl);
    put(&mut disk, 128, img);
    disk
}

#[test]
fn openbsd_mbr() {
    let img = image_bytes("ufs-little");
    let total = 128 + sectors(&img, SECTOR) + 34;
    let mut disk = vec![0u8; total as usize * SECTOR];
    put(&mut disk, 0, &mbr(&[(0xa6, 64, total - 64)]));
    check("openbsd-mbr", &openbsd(&img, false, 1, disk.clone()), "s1d");
    // Version 0 (older OpenBSD, NetBSD on some boards): the superblock decides.
    check("absolute-v0", &openbsd(&img, false, 0, disk), "s1d");
}

#[test]
fn openbsd_gpt() {
    let img = image_bytes("ufs-little");
    let total = 128 + sectors(&img, SECTOR) as usize + 34;
    let disk = gpt_layout(total, "824cc7a0-36a8-11e3-890a-952519ad3f61", 64);
    check("openbsd-gpt", &openbsd(&img, false, 1, disk), "p1d");
}

#[test]
fn freebsd_gpt_with_label() {
    // A GPT "freebsd" partition holds a label with offsets relative to it.
    let img = image_bytes("ufs-little");
    let n = sectors(&img, SECTOR);
    let total = 40 + 16 + n as usize + 34;
    let mut disk = gpt_layout(total, "516e7cb4-6ecf-11d6-8ff8-00022d09712b", 40);
    put(&mut disk, 41, &label(&[(0, 16, n, 7), (2, 0, 16 + n, 0)]));
    put(&mut disk, 56, &img);
    check("freebsd-gpt", &disk, "p1a");
}

/// An Apple Partition Map of 512-byte blocks: the map, free space, then
/// one partition of type `kind` from block 64 to the end.
fn apm_disk(total: u32, kind: &str) -> Vec<u8> {
    let mut disk = vec![0u8; total as usize * SECTOR];
    disk[0..2].copy_from_slice(b"ER");
    disk[2..4].copy_from_slice(&512u16.to_be_bytes());
    disk[4..8].copy_from_slice(&total.to_be_bytes());
    for (i, (start, blocks, kind)) in [
        (1, 3, "Apple_partition_map"),
        (4, 60, "Apple_Free"),
        (64, total - 64, kind),
    ]
    .into_iter()
    .enumerate()
    {
        let e = &mut disk[(i + 1) * SECTOR..(i + 2) * SECTOR];
        e[0..2].copy_from_slice(b"PM");
        e[4..8].copy_from_slice(&3u32.to_be_bytes());
        e[8..12].copy_from_slice(&u32::to_be_bytes(start));
        e[12..16].copy_from_slice(&u32::to_be_bytes(blocks));
        e[16..16 + kind.len()].copy_from_slice(kind.as_bytes());
        e[48..48 + kind.len()].copy_from_slice(kind.as_bytes());
    }
    disk
}

#[test]
fn apple_partition_map() {
    let img = image_bytes("ufs-big");
    let mut disk = apm_disk(64 + sectors(&img, SECTOR), "Apple_UFS");
    put(&mut disk, 64, &img);
    check("apm", &disk, "m3");
}

#[test]
fn openbsd_macppc() {
    // A big-endian label in an APM "OpenBSD" partition.
    let img = image_bytes("ufs-big");
    let disk = apm_disk(128 + sectors(&img, SECTOR), "OpenBSD");
    check("openbsd-apm", &openbsd(&img, true, 1, disk), "m3d");
}

#[test]
fn sectors_of_4096_bytes() {
    // The disk was partitioned with 4096-byte sectors; images read as 512.
    // The label is in sector 1 of the slice, or at byte 512 as OpenBSD puts it.
    const BIG: usize = 4096;
    let img = image_bytes("ufs-little");
    let n = sectors(&img, BIG);
    for at in [2 * BIG + BIG, 2 * BIG + 512] {
        let mut disk = vec![0u8; (2 + 16 + n as usize) * BIG];
        put(&mut disk, 0, &mbr(&[(0xa5, 2, 16 + n)]));
        let lbl = disklabel(&[(0, 16, n, 7), (2, 0, 16 + n, 0)], 4096, false, 0);
        disk[at..at + SECTOR].copy_from_slice(&lbl);
        disk[18 * BIG..18 * BIG + img.len()].copy_from_slice(&img);
        check("4k", &disk, "s1a");
    }
}

#[test]
fn version_1_label_with_relative_offsets() {
    // OpenBSD's version, but offsets from the slice: the superblock decides.
    let img = image_bytes("ufs-little");
    let n = sectors(&img, SECTOR);
    let total = 128 + n + 34;
    let mut disk = vec![0u8; total as usize * SECTOR];
    put(&mut disk, 0, &mbr(&[(0xa6, 64, total - 64)]));
    let lbl = disklabel(&[(2, 0, total - 64, 0), (3, 64, n, 7)], 512, false, 1);
    put(&mut disk, 65, &lbl);
    put(&mut disk, 128, &img);
    check("relative-v1", &disk, "s1d");
}

#[test]
fn backup_superblock_is_no_file_system() {
    // A slice 32 KiB into a UFS2 sees a backup superblock where its own
    // would be: only the real one counts.
    let mut img = image_bytes("ufs-little");
    let n = sectors(&img, SECTOR);
    img[..SECTOR].copy_from_slice(&mbr(&[(0xa5, 64, n)]));
    // Room behind, so the backup's file system size fits on the disk.
    img.resize(img.len() + 65536, 0);
    check("backup", &img, "whole");
}
