// SPDX-License-Identifier: AGPL-3.0-or-later
//! Mount the upstream image through WinFsp and use it with std::fs.
//! Needs WinFsp; runs only with UFSMOUNT_MOUNT_TEST=1.

mod common;

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use common::*;
use windows::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

struct Mounted {
    child: Child,
    root: PathBuf,
}

impl Mounted {
    /// Stop with Ctrl+Break, as the WinFsp launcher does. `None` if the
    /// event could not be delivered (no console).
    fn stop(&mut self) -> Option<ExitStatus> {
        // SAFETY: plain console call with the child's process group id.
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.child.id()) }.ok()?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(15) {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn enabled() -> bool {
    let on = std::env::var_os("UFSMOUNT_MOUNT_TEST").is_some();
    if !on {
        eprintln!("skipped: set UFSMOUNT_MOUNT_TEST=1 to run (needs WinFsp)");
    }
    on
}

fn free_letter() -> char {
    ('M'..='Z')
        .rev()
        .find(|l| !Path::new(&format!("{l}:\\")).exists())
        .expect("no free drive letter")
}

fn spawn(image: &Path, mountpoint: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_ufsmount"))
        .args(["-v", "mount"])
        .arg(image)
        .arg(mountpoint)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .unwrap()
}

fn mount(image: &Path) -> Option<Mounted> {
    if !enabled() {
        return None;
    }
    let letter = free_letter();
    let child = spawn(image, &format!("{letter}:"));
    let mut m = Mounted {
        child,
        root: PathBuf::from(format!("{letter}:\\")),
    };
    let start = Instant::now();
    while !m.root.join("file1").exists() {
        if let Some(status) = m.child.try_wait().unwrap() {
            panic!("ufsmount exited early: {status}");
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "mount did not appear"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Some(m)
}

#[test]
fn mounted_image() {
    for name in IMAGES {
        let path = write_temp(&format!("mount-{name}"), &image_bytes(name));
        let Some(m) = mount(&path) else { return };
        let root = &m.root;

        let mut names: Vec<String> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ROOT_NAMES, "{name}");

        assert_eq!(
            fs::read(root.join("file1")).unwrap(),
            b"This is a simple file.\n"
        );
        assert_eq!(
            fs::read(root.join("FILE1")).unwrap(),
            b"This is a simple file.\n"
        );
        assert_eq!(
            fs::read(root.join(r"dir1\dir2\dir3\file2")).unwrap(),
            b"Hello World\n"
        );

        let file3 = fs::read_to_string(root.join("file3")).unwrap();
        assert_eq!(file3.lines().count(), 1 << 16);
        assert_eq!(file3.lines().nth(4660).unwrap(), format!("{:015x}", 4660));

        let link = root.join("link1");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new(r"dir1\dir2\dir3\file2")
        );
        assert_eq!(fs::read(&link).unwrap(), b"Hello World\n");

        let meta = fs::metadata(root.join("sparse3")).unwrap();
        assert_eq!(meta.len(), 549_890_457_600);
        assert!(fs::metadata(root.join("dir1")).unwrap().is_dir());

        let sparse2 = fs::read(root.join("sparse2")).unwrap();
        assert_eq!(sparse2.len(), 134_615_040);
        assert!(sparse2[(12 + 4096) * 32768..].iter().all(|&b| b == b'x'));
        assert!(sparse2[..1 << 20].iter().all(|&b| b == 0));

        assert!(fs::write(root.join("new.txt"), b"x").is_err());
        assert!(
            fs::OpenOptions::new()
                .write(true)
                .open(root.join("file1"))
                .is_err()
        );
        assert!(fs::remove_file(root.join("file1")).is_err());
        assert!(fs::create_dir(root.join("newdir")).is_err());
        assert!(fs::rename(root.join("file1"), root.join("file9")).is_err());
        assert!(root.join("file1").exists());

        let mut m = m;
        match m.stop() {
            Some(status) => {
                assert!(status.success(), "Ctrl+Break exit: {status}");
                assert!(!m.root.exists(), "the drive is gone after the stop");
            }
            None => eprintln!("Ctrl+Break not delivered (no console); stopped by kill"),
        }
    }
}

#[test]
fn failed_mount_exits_with_an_error() {
    if !enabled() {
        return;
    }
    let path = write_temp("mount-busy", &image_bytes("ufs-little"));
    let windows = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let mut child = spawn(&path, &windows);
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("ufsmount kept running although {windows} is in use");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!status.success(), "mount on {windows} must fail");
}
