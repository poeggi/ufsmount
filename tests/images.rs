// SPDX-License-Identifier: AGPL-3.0-or-later
//! The volume layer on the upstream fuse-ufs test images.

mod common;

use common::*;
use rufs::InodeType;
use ufsmount::volume::Error;

fn each_image(f: impl Fn(&str, &ufsmount::volume::Volume)) {
    for name in IMAGES {
        let path = write_temp(name, &image_bytes(name));
        let (vol, id) = open(&path, None);
        assert_eq!(id, "whole");
        f(name, &vol);
    }
}

#[test]
fn listing() {
    each_image(|name, vol| {
        assert_eq!(root_names(vol), ROOT_NAMES, "{name}");
    });
}

#[test]
fn small_files() {
    each_image(|_, vol| {
        assert_eq!(read_all(vol, r"\file1"), b"This is a simple file.\n");
        assert_eq!(read_all(vol, "/dir1/dir2/dir3/file2"), b"Hello World\n");
    });
}

#[test]
fn indirect_file() {
    each_image(|_, vol| {
        let data = read_all(vol, r"\file3");
        let text = String::from_utf8(data).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1 << 16);
        for (i, l) in lines.iter().enumerate() {
            assert_eq!(*l, format!("{i:015x}"));
        }
    });
}

#[test]
fn sparse_files() {
    each_image(|_, vol| {
        let attr = |p: &str| vol.attr(&vol.lookup(&w(p)).unwrap()).unwrap();
        assert_eq!(attr(r"\sparse").size, 134_643_712);
        assert_eq!(
            read_at(vol, r"\sparse", (12 + 4096) * 32768, 32768),
            vec![b'x'; 32768]
        );
        assert_eq!(
            read_at(vol, r"\sparse", (12 + 5) * 32768, 32768),
            vec![0; 32768]
        );
        assert_eq!(attr(r"\sparse2").size, 134_615_040);
        assert_eq!(
            read_at(vol, r"\sparse2", (12 + 4096) * 32768, 4096),
            vec![b'x'; 4096]
        );
        assert_eq!(attr(r"\sparse3").size, 549_890_457_600);
        let off = (12 + 4096 + 4096 * 4096) * 32768;
        assert_eq!(read_at(vol, r"\sparse3", off, 32768), vec![b'x'; 32768]);
        assert_eq!(
            read_at(vol, r"\sparse3", 549_883_084_800, 128 * 1024),
            vec![0; 128 * 1024]
        );
    });
}

#[test]
fn eof() {
    each_image(|_, vol| {
        let node = vol.lookup(&w(r"\file1")).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(vol.read(node.inr, 23, &mut buf).unwrap(), 0);
        assert_eq!(vol.read(node.inr, 1 << 40, &mut buf).unwrap(), 0);
        assert_eq!(vol.read(node.inr, 20, &mut buf).unwrap(), 3);
    });
}

#[test]
fn symlinks() {
    each_image(|_, vol| {
        let link = vol.lookup(&w(r"\link1")).unwrap();
        assert_eq!(link.kind, InodeType::Symlink);
        assert_eq!(vol.readlink(link.inr).unwrap(), b"dir1/dir2/dir3/file2");
        let attr = vol.attr(&link).unwrap();
        assert!(attr.is_symlink && !attr.link_to_dir);
        assert_eq!(attr.size, 0);
        assert_eq!(
            String::from_utf16(&vol.link_target_windows(b"dir1/dir2/dir3/file2")).unwrap(),
            r"dir1\dir2\dir3\file2"
        );

        let long = vol.lookup(&w(r"\long-link")).unwrap();
        let target = vol.readlink(long.inr).unwrap();
        let expected = "./".repeat(508) + "//file1";
        assert_eq!(target, expected.as_bytes());
        assert!(!vol.attr(&long).unwrap().link_to_dir);
    });
}

#[test]
fn directory_attributes() {
    each_image(|_, vol| {
        let node = vol.lookup(&w(r"\dir1\dir2")).unwrap();
        let attr = vol.attr(&node).unwrap();
        assert!(attr.is_dir);
        assert_eq!(attr.size, 0);
        assert!(attr.write > 0 && attr.creation > 0);
        let parent = vol.dir(node.inr).unwrap().parent;
        assert_eq!(parent, vol.lookup(&w(r"\dir1")).unwrap().inr);
    });
}

#[test]
fn lookup_errors_and_case() {
    each_image(|_, vol| {
        assert!(matches!(vol.lookup(&w(r"\nope")), Err(Error::NotFound)));
        assert!(matches!(
            vol.lookup(&w(r"\nope\x")),
            Err(Error::PathNotFound)
        ));
        assert!(matches!(
            vol.lookup(&w(r"\file1\x")),
            Err(Error::PathNotFound)
        ));
        assert_eq!(
            vol.lookup(&w(r"\FILE1")).unwrap().inr,
            vol.lookup(&w(r"\file1")).unwrap().inr
        );
        assert_eq!(vol.lookup(&w("")).unwrap().inr, rufs::InodeNum::ROOT);
    });
}

#[test]
fn volume_info() {
    each_image(|_, vol| {
        assert_eq!(vol.info.bsize, 32768);
        assert_eq!(vol.info.fsize, 4096);
        assert!(vol.info.frags > 0);
    });
}
