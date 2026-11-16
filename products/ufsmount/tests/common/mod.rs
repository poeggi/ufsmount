// SPDX-License-Identifier: AGPL-3.0-or-later
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use winfsp_mount::device::{Device, Source};
use winfsp_mount::names::Charset;
use winfsp_mount::partition;
use winfsp_mount::volume::Volume;

pub const IMAGES: [&str; 2] = ["ufs-little", "ufs-big"];

/// The upstream test image, unpacked once per test run.
pub fn image_bytes(name: &str) -> Vec<u8> {
    let zst = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/fuse-ufs/resources")
        .join(format!("{name}.img.zst"));
    zstd::decode_all(std::fs::File::open(&zst).unwrap()).unwrap()
}

/// A file in the test temp dir, deleted on drop.
pub struct TempFile(PathBuf);

impl std::ops::Deref for TempFile {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn temp_path(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{tag}-{}-{n}", std::process::id()))
}

/// Write bytes to a fresh file in the test temp dir.
pub fn write_temp(tag: &str, data: &[u8]) -> TempFile {
    let path = temp_path(tag).with_extension("img");
    std::fs::write(&path, data).unwrap();
    TempFile(path)
}

pub fn open(path: &Path, part: Option<&str>) -> (Volume, String) {
    ufsmount::init();
    let dev = Arc::new(Device::open(&Source::Image(path.into()), None).unwrap());
    let cands = partition::probe(&dev);
    let cand = partition::select(&cands, part).unwrap().clone();
    (Volume::open(dev, &cand, Charset::Utf8).unwrap(), cand.id)
}

pub fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub fn read_all(vol: &Volume, path: &str) -> Vec<u8> {
    let node = vol.lookup(&w(path)).unwrap();
    let attr = vol.attr(&node).unwrap();
    let mut buf = vec![0u8; attr.size as usize];
    let mut off = 0;
    while off < buf.len() {
        let n = vol.read(node.id, off as u64, &mut buf[off..]).unwrap();
        assert!(n > 0);
        off += n;
    }
    buf
}

pub fn read_at(vol: &Volume, path: &str, offset: u64, len: usize) -> Vec<u8> {
    let node = vol.lookup(&w(path)).unwrap();
    let mut buf = vec![0u8; len];
    let n = vol.read(node.id, offset, &mut buf).unwrap();
    assert_eq!(n, len);
    buf
}

pub fn root_names(vol: &Volume) -> Vec<String> {
    let table = vol.dir(vol.root()).unwrap();
    let mut names: Vec<String> = table
        .entries
        .iter()
        .map(|e| String::from_utf16(&e.name).unwrap())
        .collect();
    names.sort();
    names
}

pub const ROOT_NAMES: [&str; 12] = [
    ".snap",
    "dir1",
    "file1",
    "file3",
    "link1",
    "long-link",
    "sparse",
    "sparse2",
    "sparse3",
    "xattrs",
    "xattrs2",
    "xattrs3",
];

/// Find a byte pattern in an image.
pub fn find(data: &[u8], pattern: &[u8]) -> usize {
    data.windows(pattern.len())
        .position(|w| w == pattern)
        .unwrap_or_else(|| panic!("pattern {pattern:?} not found"))
}
