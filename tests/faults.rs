// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bad sectors and a vanishing disk, through rufs and the volume layer.

mod common;

use common::*;
use ufsmount::device::device_error;
use ufsmount::device::testing::FakeDisk;
use ufsmount::names::Charset;
use ufsmount::partition;
use ufsmount::volume::{Error, Volume};

const ERROR_CRC: u32 = 23;

fn volume(disk: FakeDisk) -> Volume {
    let dev = disk.into_device();
    let cands = partition::probe(&dev);
    let cand = partition::select(&cands, None).unwrap().clone();
    Volume::open(dev, &cand, Charset::Utf8).unwrap()
}

fn sector_of(img: &[u8], pattern: &[u8]) -> std::ops::Range<u64> {
    let at = find(img, pattern) as u64 / 512 * 512;
    at..at + 512
}

#[test]
fn bad_sector_under_file_data() {
    let img = image_bytes("ufs-little");
    let bad = sector_of(&img, b"This is a simple file.");
    let mut disk = FakeDisk::new(img, 512);
    disk.bad.push((bad, ERROR_CRC));
    let vol = volume(disk);

    assert_eq!(root_names(&vol), ROOT_NAMES);
    let node = vol.lookup(&w(r"\file1")).unwrap();
    let mut buf = [0u8; 64];
    match vol.read(node.inr, 0, &mut buf) {
        Err(Error::Io(e)) => assert_eq!(device_error(&e).unwrap().code, ERROR_CRC),
        other => panic!("expected a device error, got {other:?}"),
    }
    // Other files still read.
    assert_eq!(read_all(&vol, "/dir1/dir2/dir3/file2"), b"Hello World\n");
    assert_eq!(read_all(&vol, r"\file3").len(), 1 << 20);
}

#[test]
fn bad_sector_under_a_directory() {
    let img = image_bytes("ufs-little");
    // The directory block of dir3 holds the entry for file2.
    let bad = sector_of(&img, b"\x08\x05file2");
    let mut disk = FakeDisk::new(img, 512);
    disk.bad.push((bad, ERROR_CRC));
    let vol = volume(disk);

    match vol.lookup(&w(r"\dir1\dir2\dir3\file2")) {
        Err(Error::Io(e)) => assert!(device_error(&e).unwrap().is_media_error()),
        other => panic!("expected a device error, got {other:?}"),
    }
    assert_eq!(read_all(&vol, r"\file1"), b"This is a simple file.\n");
}

#[test]
fn disk_gone_during_use() {
    let img = image_bytes("ufs-little");
    // Count the raw reads of opening and reading file3 once, then unplug before the end.
    let probe = FakeDisk::new(img.clone(), 512);
    let reads = probe.reads.clone();
    let vol = volume(probe);
    let open_reads = reads.load(std::sync::atomic::Ordering::Relaxed);
    read_all(&vol, r"\file3");
    let total = reads.load(std::sync::atomic::Ordering::Relaxed);
    assert!(total > open_reads + 2);

    let mut disk = FakeDisk::new(img, 512);
    disk.unplug_after = Some(total - 2);
    let vol = volume(disk);
    let mut failed = false;
    for _ in 0..100 {
        match vol.lookup(&w(r"\file3")).and_then(|n| {
            let mut buf = vec![0u8; 1 << 20];
            vol.read(n.inr, 0, &mut buf)
        }) {
            Ok(_) => {}
            Err(Error::Io(e)) => {
                assert!(device_error(&e).unwrap().is_gone());
                failed = true;
                break;
            }
            Err(e) => panic!("unexpected {e}"),
        }
    }
    assert!(failed);
    assert!(vol.device().is_gone());
}

/// ufs-little with dir2's entry for dir3 changed by `patch`, which gets the
/// image, the entry's offset and dir2's inode number (bytes, as on disk).
fn with_dir3_entry(patch: impl FnOnce(&mut [u8], usize, [u8; 4])) -> Volume {
    let mut img = image_bytes("ufs-little");
    // struct direct: d_ino u32, d_reclen u16, d_type u8, d_namlen u8, d_name.
    let entry = find(&img, b"\x04\x04dir3") - 6;
    let chunk = entry / 512 * 512;
    assert_eq!(&img[chunk + 6..chunk + 9], b"\x04\x01.", "dir2's . entry");
    let dir2 = img[chunk..chunk + 4].try_into().unwrap();
    patch(&mut img, entry, dir2);
    volume(FakeDisk::new(img, 512))
}

fn cp_dir1(vol: &Volume, tag: &str) -> (ufsmount::copy::Stats, std::path::PathBuf) {
    let out = temp_path(tag);
    std::fs::create_dir_all(&out).unwrap();
    let stats = ufsmount::copy::cp(vol, "/dir1", &out).unwrap();
    (stats, out)
}

fn remove_tree(dir: &std::path::Path) {
    std::fs::remove_dir_all(format!(r"\\?\{}", dir.display())).unwrap();
}

#[test]
fn cp_keeps_crafted_names_inside() {
    let mut img = image_bytes("ufs-little");
    let at = find(&img, b"\x08\x05file2");
    img[at + 2..at + 7].copy_from_slice(b"../x2");
    let vol = volume(FakeDisk::new(img, 512));

    let (stats, out) = cp_dir1(&vol, "cp-crafted");
    assert_eq!((stats.files, stats.failed), (1, 0));
    assert!(!out.join(r"dir1\dir2\x2").exists());
    assert!(out.join(r"dir1\dir2\dir3").join("..\u{f02f}x2").exists());
    remove_tree(&out);
}

#[test]
fn cp_stops_at_a_directory_loop() {
    let vol = with_dir3_entry(|img, entry, dir2| img[entry..entry + 4].copy_from_slice(&dir2));
    let (stats, out) = cp_dir1(&vol, "cp-loop");
    assert_eq!((stats.dirs, stats.failed), (2, 1));
    assert!(out.join(r"dir1\dir2").is_dir());
    assert!(!out.join(r"dir1\dir2\dir3").exists());
    remove_tree(&out);
}

#[test]
fn cp_skips_an_empty_name() {
    let vol = with_dir3_entry(|img, entry, dir2| {
        img[entry..entry + 4].copy_from_slice(&dir2);
        img[entry + 7] = 0;
    });
    let dir2 = vol.lookup(&w(r"\dir1\dir2")).unwrap();
    let table = vol.dir(dir2.inr).unwrap();
    assert_eq!((table.entries.len(), table.hidden), (0, 1));

    let (stats, out) = cp_dir1(&vol, "cp-empty");
    assert_eq!((stats.dirs, stats.failed), (2, 1));
    assert!(!out.join(r"dir1\dir2\dir3").exists());
    remove_tree(&out);
}

#[test]
fn cp_tree() {
    let img = write_temp("cp", &image_bytes("ufs-big"));
    let (vol, _) = open(&img, None);
    let out = temp_path("cp-out");
    std::fs::create_dir_all(&out).unwrap();

    let stats = ufsmount::copy::cp(&vol, "/dir1", &out).unwrap();
    assert_eq!((stats.files, stats.dirs, stats.failed), (1, 3, 0));
    assert_eq!(
        std::fs::read(out.join(r"dir1\dir2\dir3\file2")).unwrap(),
        b"Hello World\n"
    );

    let stats = ufsmount::copy::cp(&vol, "/link1", &out.join("via-link")).unwrap();
    assert_eq!(stats.files, 1, "a symlink source is followed");
    assert_eq!(
        std::fs::read(out.join("via-link")).unwrap(),
        b"Hello World\n"
    );

    let stats = ufsmount::copy::cp(&vol, "dir1/../file1", &out.join("nul")).unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(
        std::fs::read(format!(r"\\?\{}\nul", out.display())).unwrap(),
        b"This is a simple file.\n",
        "a DOS device name is a plain file"
    );
    std::fs::remove_dir_all(format!(r"\\?\{}", out.display())).unwrap();
}
