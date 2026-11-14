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

/// A disklabel. `parts` are (index, offset, size, fstype).
fn label(parts: &[(usize, u32, u32, u8)]) -> Vec<u8> {
    let magic = 0x8256_4557u32.to_le_bytes();
    let mut s = vec![0u8; SECTOR];
    s[0..4].copy_from_slice(&magic);
    s[132..136].copy_from_slice(&magic);
    s[40..44].copy_from_slice(&512u32.to_le_bytes());
    s[138..140].copy_from_slice(&8u16.to_le_bytes());
    for &(i, offset, size, fstype) in parts {
        let e = &mut s[148 + 16 * i..164 + 16 * i];
        e[0..4].copy_from_slice(&size.to_le_bytes());
        e[4..8].copy_from_slice(&offset.to_le_bytes());
        e[12] = fstype;
    }
    s
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
    // The first 64 KiB of a UFS2 partition are unused, so the label may sit inside it.
    let lbl = label(&[(0, base + a_rel, img_sectors, 0), (2, base, slice_len, 0)]);
    put(&mut disk, (63 + a_rel) as usize, img);
    put(&mut disk, 64, &lbl);
    disk
}

#[test]
fn target_disk_layout() {
    // As on the real disk: slice at LBA 63, label a at +16 with fstype 0, c at 0.
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

fn gpt_disk(img: &[u8]) -> Vec<u8> {
    const FREEBSD_UFS: [u8; 16] = [
        0xb6, 0x7c, 0x6e, 0x51, 0xcf, 0x6e, 0xd6, 0x11, 0x8f, 0xf8, 0x00, 0x02, 0x2d, 0x09, 0x71,
        0x2b,
    ];
    let img_sectors = img.len() / SECTOR;
    let total = 40 + img_sectors + 34;
    let mut disk = vec![0u8; total * SECTOR];
    put(&mut disk, 0, &mbr(&[(0xee, 1, (total - 1) as u32)]));
    let mut entries = vec![0u8; 4 * 128];
    entries[0..16].copy_from_slice(&FREEBSD_UFS);
    entries[32..40].copy_from_slice(&40u64.to_le_bytes());
    entries[40..48].copy_from_slice(&((40 + img_sectors - 1) as u64).to_le_bytes());
    put(&mut disk, 1, &gpt_header(1, 2, &entries));
    put(&mut disk, 2, &entries);
    put(
        &mut disk,
        total - 1,
        &gpt_header((total - 1) as u64, 2, &entries),
    );
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
        ufsmount::device::Device::open(&ufsmount::device::Source::Image(path), None).unwrap(),
    );
    let cands = ufsmount::partition::probe(&dev);
    assert!(ufsmount::partition::select(&cands, Some("s1")).is_err());
    assert!(ufsmount::partition::select(&cands, Some("p9")).is_err());
}

#[test]
fn not_clean() {
    let mut img = image_bytes("ufs-little");
    img[65536 + 209] = 0;
    let (vol, _) = open(&write_temp("dirty", &img), None);
    assert!(!vol.info.clean);
    assert_eq!(root_names(&vol), ROOT_NAMES);
}

#[test]
fn odd_names() {
    let mut img = image_bytes("ufs-little");
    let at = find(&img, b"\x08\x05file1") + 2;
    img[at] = 0xff;
    let at = find(&img, b"\x08\x05file3") + 4;
    img[at] = b':';
    let (vol, _) = open(&write_temp("names", &img), None);
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
    let (vol, _) = open(&write_temp("deleted", &img), None);
    assert_eq!(root_names(&vol), ROOT_NAMES);
}
