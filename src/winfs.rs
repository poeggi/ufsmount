// SPDX-License-Identifier: AGPL-3.0-or-later
//! The WinFsp file system: read-only, every write path reports write protection.

use std::ffi::c_void;
use std::sync::Arc;

use rufs::{InodeNum, InodeType};
use windows::Win32::Foundation::{
    NTSTATUS, STATUS_BUFFER_TOO_SMALL, STATUS_DEVICE_DATA_ERROR, STATUS_DEVICE_NOT_CONNECTED,
    STATUS_END_OF_FILE, STATUS_FILE_CORRUPT_ERROR, STATUS_IO_DEVICE_ERROR,
    STATUS_MEDIA_WRITE_PROTECTED, STATUS_NOT_A_DIRECTORY, STATUS_NOT_A_REPARSE_POINT,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
};
use windows::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    SetDefaultDllDirectories,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{HSTRING, w};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext,
    ModificationDescriptor, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FileSystemParams, VolumeParams};
use winfsp::service::FileSystemServiceBuilder;
use winfsp::{FspError, FspInit, U16CStr};

use crate::device::device_error;
use crate::security::{SecurityDescriptor, current_user_sid};
use crate::volume::{self, Attr, Node, Volume};

/// Oldest WinFsp accepted: 2.2.26286 (2026 Beta5). Older builds have kernel
/// vulnerabilities, among them CVE-2026-3006.
pub const MIN_WINFSP: (u32, u32, u32) = (2, 2, 26286);

const IO_REPARSE_TAG_SYMLINK: u32 = 0xa000_000c;
const SYMLINK_FLAG_RELATIVE: u32 = 1;
const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;
/// FILE_WRITE_DATA, FILE_APPEND_DATA, FILE_WRITE_EA, FILE_DELETE_CHILD,
/// FILE_WRITE_ATTRIBUTES, DELETE, WRITE_DAC, WRITE_OWNER, GENERIC_ALL, GENERIC_WRITE.
const WRITE_ACCESS: u32 = 0x5000_0000 | 0x000d_0000 | 0x0156;

fn status(s: NTSTATUS) -> FspError {
    FspError::NTSTATUS(s.0)
}

fn map_err(e: volume::Error) -> FspError {
    match e {
        volume::Error::NotFound => status(STATUS_OBJECT_NAME_NOT_FOUND),
        volume::Error::PathNotFound => status(STATUS_OBJECT_PATH_NOT_FOUND),
        volume::Error::NotADirectory => status(STATUS_NOT_A_DIRECTORY),
        volume::Error::Io(io) => {
            let s = match device_error(&io) {
                Some(d) if d.is_gone() => STATUS_DEVICE_NOT_CONNECTED,
                Some(d) if d.is_media_error() => STATUS_DEVICE_DATA_ERROR,
                Some(_) => STATUS_IO_DEVICE_ERROR,
                None => STATUS_FILE_CORRUPT_ERROR,
            };
            log::error!("{}", volume::describe(&io));
            status(s)
        }
    }
}

fn file_attributes(a: &Attr) -> u32 {
    let mut x = 0;
    if a.is_dir || (a.is_symlink && a.link_to_dir) {
        x |= FILE_ATTRIBUTE_DIRECTORY.0;
    }
    if a.is_symlink {
        x |= FILE_ATTRIBUTE_REPARSE_POINT.0;
    }
    if x == 0 { FILE_ATTRIBUTE_NORMAL.0 } else { x }
}

fn fill(fi: &mut FileInfo, a: &Attr) {
    fi.file_attributes = file_attributes(a);
    fi.reparse_tag = if a.is_symlink {
        IO_REPARSE_TAG_SYMLINK
    } else {
        0
    };
    fi.allocation_size = a.alloc;
    fi.file_size = a.size;
    fi.creation_time = a.creation;
    fi.last_access_time = a.access;
    fi.last_write_time = a.write;
    fi.change_time = a.change;
    fi.index_number = a.inr;
    fi.hard_links = a.nlink;
    fi.ea_size = 0;
}

/// A symlink reparse buffer (REPARSE_DATA_BUFFER) with the same substitute
/// and print name. Returns the size written.
pub fn symlink_reparse(target: &[u16], buffer: &mut [u8]) -> winfsp::Result<u64> {
    let name = target.len() * 2;
    let total = 20 + 2 * name;
    if buffer.len() < total || name > u16::MAX as usize / 2 - 12 {
        return Err(status(STATUS_BUFFER_TOO_SMALL));
    }
    let b = &mut buffer[..total];
    b[0..4].copy_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    b[4..6].copy_from_slice(&((12 + 2 * name) as u16).to_le_bytes());
    b[6..8].fill(0);
    b[8..10].copy_from_slice(&0u16.to_le_bytes());
    b[10..12].copy_from_slice(&(name as u16).to_le_bytes());
    b[12..14].copy_from_slice(&(name as u16).to_le_bytes());
    b[14..16].copy_from_slice(&(name as u16).to_le_bytes());
    b[16..20].copy_from_slice(&SYMLINK_FLAG_RELATIVE.to_le_bytes());
    for (i, unit) in target.iter().chain(target.iter()).enumerate() {
        b[20 + 2 * i..22 + 2 * i].copy_from_slice(&unit.to_le_bytes());
    }
    Ok(total as u64)
}

pub struct UfsFs {
    vol: Arc<Volume>,
    sd: SecurityDescriptor,
    label: String,
}

pub struct Ctx {
    node: Node,
    size: u64,
    dir: DirBuffer,
}

impl UfsFs {
    pub fn new(vol: Arc<Volume>, sd: SecurityDescriptor) -> UfsFs {
        let label = if vol.info.volname.is_empty() {
            "UFS".to_string()
        } else {
            crate::names::display(&vol.info.volname)
        };
        UfsFs { vol, sd, label }
    }

    fn lookup(&self, name: &U16CStr) -> winfsp::Result<Node> {
        self.vol.lookup(name.as_slice()).map_err(map_err)
    }

    fn attr(&self, node: &Node) -> winfsp::Result<Attr> {
        self.vol.attr(node).map_err(map_err)
    }

    fn reparse(&self, node: &Node, buffer: &mut [u8]) -> winfsp::Result<u64> {
        if node.kind != InodeType::Symlink {
            return Err(status(STATUS_NOT_A_REPARSE_POINT));
        }
        let target = self.vol.readlink(node.inr).map_err(map_err)?;
        symlink_reparse(&self.vol.link_target_windows(&target), buffer)
    }
}

fn write_protected<T>() -> winfsp::Result<T> {
    Err(status(STATUS_MEDIA_WRITE_PROTECTED))
}

impl FileSystemContext for UfsFs {
    type FileContext = Ctx;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        if let Some(security) = reparse_point_resolver(file_name) {
            return Ok(security);
        }
        let node = self.lookup(file_name)?;
        let attr = self.attr(&node)?;
        self.sd.copy_to(security_descriptor);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.sd.len() as u64,
            attributes: file_attributes(&attr),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Ctx> {
        if granted_access & WRITE_ACCESS != 0 || create_options & FILE_DELETE_ON_CLOSE != 0 {
            return write_protected();
        }
        let node = self.lookup(file_name)?;
        let attr = self.attr(&node)?;
        fill(file_info.as_mut(), &attr);
        Ok(Ctx {
            node,
            size: attr.size,
            dir: DirBuffer::new(),
        })
    }

    fn close(&self, _context: Ctx) {}

    fn get_file_info(&self, context: &Ctx, file_info: &mut FileInfo) -> winfsp::Result<()> {
        fill(file_info, &self.attr(&context.node)?);
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Ctx,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        self.sd.copy_to(security_descriptor);
        Ok(self.sd.len() as u64)
    }

    fn read(&self, context: &Ctx, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        if offset >= context.size {
            return Err(status(STATUS_END_OF_FILE));
        }
        let n = self
            .vol
            .read(context.node.inr, offset, buffer)
            .map_err(map_err)?;
        Ok(n as u32)
    }

    fn read_directory(
        &self,
        context: &Ctx,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if let Ok(lock) = context.dir.acquire(marker.is_none(), None) {
            let table = self.vol.dir(context.node.inr).map_err(map_err)?;
            let mut info: DirInfo = DirInfo::new();
            if context.node.inr != InodeNum::ROOT {
                let dots: [(&[u16], Node); 2] = [
                    (&[b'.' as u16], context.node),
                    (
                        &[b'.' as u16, b'.' as u16],
                        Node {
                            inr: table.parent,
                            kind: InodeType::Directory,
                            parent: table.parent,
                        },
                    ),
                ];
                for (name, node) in dots {
                    info.reset();
                    fill(info.file_info_mut(), &self.vol.attr_or_default(&node, name));
                    info.set_name_raw(name)?;
                    lock.write(&mut info)?;
                }
            }
            for e in &table.entries {
                let child = Node {
                    inr: e.inr,
                    kind: e.kind,
                    parent: context.node.inr,
                };
                info.reset();
                fill(
                    info.file_info_mut(),
                    &self.vol.attr_or_default(&child, &e.name),
                );
                info.set_name_raw(e.name.as_slice())?;
                lock.write(&mut info)?;
            }
        }
        Ok(context.dir.read(marker, buffer))
    }

    fn get_dir_info_by_name(
        &self,
        context: &Ctx,
        file_name: &U16CStr,
        out_dir_info: &mut DirInfo,
    ) -> winfsp::Result<()> {
        let table = self.vol.dir(context.node.inr).map_err(map_err)?;
        let e = table
            .find(file_name.as_slice())
            .ok_or(status(STATUS_OBJECT_NAME_NOT_FOUND))?;
        let child = Node {
            inr: e.inr,
            kind: e.kind,
            parent: context.node.inr,
        };
        out_dir_info.reset();
        fill(
            out_dir_info.file_info_mut(),
            &self.vol.attr_or_default(&child, &e.name),
        );
        out_dir_info.set_name_raw(e.name.as_slice())?;
        Ok(())
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let fsize = u64::from(self.vol.info.fsize);
        out_volume_info.total_size = self.vol.info.blocks * fsize;
        out_volume_info.free_size = self.vol.info.bfree * fsize;
        out_volume_info.set_volume_label(&self.label);
        Ok(())
    }

    fn get_reparse_point_by_name(
        &self,
        file_name: &U16CStr,
        _is_directory: bool,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        let node = self.lookup(file_name)?;
        self.reparse(&node, buffer)
    }

    fn get_reparse_point(
        &self,
        context: &Ctx,
        _file_name: &U16CStr,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        self.reparse(&context.node, buffer)
    }

    fn flush(&self, context: Option<&Ctx>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        if let Some(context) = context {
            fill(file_info, &self.attr(&context.node)?);
        }
        Ok(())
    }

    fn create(
        &self,
        _file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        _file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Ctx> {
        write_protected()
    }

    fn overwrite(
        &self,
        _context: &Ctx,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        write_protected()
    }

    fn write(
        &self,
        _context: &Ctx,
        _buffer: &[u8],
        _offset: u64,
        _write_to_eof: bool,
        _constrained_io: bool,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        write_protected()
    }

    fn set_basic_info(
        &self,
        _context: &Ctx,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        write_protected()
    }

    fn set_file_size(
        &self,
        _context: &Ctx,
        _new_size: u64,
        _set_allocation_size: bool,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        write_protected()
    }

    fn set_delete(
        &self,
        _context: &Ctx,
        _file_name: &U16CStr,
        _delete_file: bool,
    ) -> winfsp::Result<()> {
        write_protected()
    }

    fn rename(
        &self,
        _context: &Ctx,
        _file_name: &U16CStr,
        _new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        write_protected()
    }

    fn set_security(
        &self,
        _context: &Ctx,
        _security_information: u32,
        _modification_descriptor: ModificationDescriptor,
    ) -> winfsp::Result<()> {
        write_protected()
    }

    fn set_reparse_point(
        &self,
        _context: &Ctx,
        _file_name: &U16CStr,
        _buffer: &[u8],
    ) -> winfsp::Result<()> {
        write_protected()
    }

    fn delete_reparse_point(
        &self,
        _context: &Ctx,
        _file_name: &U16CStr,
        _buffer: &[u8],
    ) -> winfsp::Result<()> {
        write_protected()
    }

    fn set_volume_label(
        &self,
        _volume_label: &U16CStr,
        _volume_info: &mut VolumeInfo,
    ) -> winfsp::Result<()> {
        write_protected()
    }
}

/// Look up DLLs only in System32 and in explicitly given directories.
pub fn harden_dll_search() {
    // SAFETY: plain process-wide setting.
    if let Err(e) = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } {
        log::warn!("SetDefaultDllDirectories failed: {e}");
    }
}

fn winfsp_install_dir() -> anyhow::Result<String> {
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buf holds `size` bytes.
    let err = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\WOW6432Node\WinFsp"),
            w!("InstallDir"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    anyhow::ensure!(
        err.is_ok(),
        "WinFsp is not installed (get it from https://github.com/winfsp/winfsp/releases)"
    );
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..len]))
}

fn file_version(path: &HSTRING) -> anyhow::Result<(u32, u32, u32, u32)> {
    // SAFETY: standard version resource query with a buffer of the reported size.
    unsafe {
        let size = GetFileVersionInfoSizeW(path, None);
        anyhow::ensure!(size > 0, "no version information in {path}");
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(path, None, size, data.as_mut_ptr().cast())?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        anyhow::ensure!(
            VerQueryValueW(data.as_ptr().cast(), w!(r"\"), &mut ptr, &mut len).as_bool()
                && len as usize >= size_of::<VS_FIXEDFILEINFO>(),
            "no version information in {path}"
        );
        let v = &*ptr.cast::<VS_FIXEDFILEINFO>();
        Ok((
            v.dwFileVersionMS >> 16,
            v.dwFileVersionMS & 0xffff,
            v.dwFileVersionLS >> 16,
            v.dwFileVersionLS & 0xffff,
        ))
    }
}

/// Load winfsp-x64.dll by full path from the WinFsp install directory and check its version.
pub fn load_winfsp() -> anyhow::Result<FspInit> {
    let dir = winfsp_install_dir()?;
    let dll = HSTRING::from(format!(
        r"{}\bin\winfsp-x64.dll",
        dir.trim_end_matches('\\')
    ));
    let (major, minor, build, _) = file_version(&dll)?;
    anyhow::ensure!(
        (major, minor, build) >= MIN_WINFSP,
        "WinFsp {major}.{minor}.{build} is too old, {}.{}.{} or later is needed",
        MIN_WINFSP.0,
        MIN_WINFSP.1,
        MIN_WINFSP.2
    );
    // SAFETY: loading a DLL by full path.
    unsafe {
        LoadLibraryExW(
            &dll,
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
    .map_err(|e| anyhow::anyhow!("cannot load {dll}: {e}"))?;
    winfsp::winfsp_init().map_err(|e| anyhow::anyhow!("WinFsp init failed: {e:?}"))
}

/// Build the file system host. `prefix` is the UNC volume prefix in launcher mode.
pub fn build_host(
    vol: Arc<Volume>,
    sd: SecurityDescriptor,
    prefix: Option<&str>,
) -> winfsp::Result<FileSystemHost<UfsFs>> {
    let fsize = vol.info.fsize;
    let id = vol.info.id;
    let mut vp = VolumeParams::new();
    vp.sector_size(4096)
        .sectors_per_allocation_unit((fsize / 4096).max(1) as u16)
        .max_component_length(255)
        .volume_serial_number(id[0] ^ id[1])
        .file_info_timeout(60_000)
        .case_sensitive_search(true)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .reparse_points(true)
        .read_only_volume(true)
        .pass_query_directory_filename(true)
        .post_cleanup_when_modified_only(true)
        .filesystem_name("UFS");
    if let Some(prefix) = prefix {
        vp.prefix(prefix);
    }
    let params = FileSystemParams {
        use_dir_info_by_name: true,
        volume_params: vp,
        debug_mode: Default::default(),
    };
    Ok(FileSystemHost::new_with_options(
        params,
        UfsFs::new(vol, sd),
    )?)
}

/// `U:` or `U` becomes the Mount Manager form `\\.\U:`: a global drive, so the
/// same user's non-elevated Explorer sees it too. Directories stay as they are.
fn global_drive(mountpoint: &str) -> Option<String> {
    let t = mountpoint.trim_end_matches(['\\', '/']);
    let b = t.as_bytes();
    let letter = match b {
        [l] | [l, b':'] if l.is_ascii_alphabetic() => l.to_ascii_uppercase() as char,
        _ => return None,
    };
    Some(format!(r"\\.\{letter}:"))
}

/// Mount and serve until Ctrl+C, Ctrl+Break or console close.
pub fn mount_console(vol: Volume, mountpoint: &str, sddl: Option<&str>) -> anyhow::Result<()> {
    let init = load_winfsp()?;
    let sd = match sddl {
        Some(s) => SecurityDescriptor::from_sddl(s)?,
        None => SecurityDescriptor::read_only_for(&current_user_sid()?)?,
    };
    let vol = Arc::new(vol);
    let mountpoint = mountpoint.to_string();
    let mut service = FileSystemServiceBuilder::new()
        .with_start(move || {
            let mut host = build_host(vol.clone(), sd.clone(), None)?;
            let mounted = match global_drive(&mountpoint) {
                Some(global) => match host.mount(global.as_str()) {
                    Ok(()) => Ok(global),
                    Err(e) => {
                        log::info!("global mount on {global} failed ({e:?}), mounting for this session only");
                        host.mount(mountpoint.as_str()).map(|()| mountpoint.clone())
                    }
                },
                None => host.mount(mountpoint.as_str()).map(|()| mountpoint.clone()),
            };
            if let Err(e) = mounted {
                log::error!("cannot mount on {mountpoint}: {e}");
                return Err(e.into());
            }
            host.start_with_threads(4)?;
            eprintln!("Mounted read-only on {mountpoint}. Press Ctrl+C to unmount.");
            Ok(host)
        })
        .with_stop(|host| {
            if let Some(host) = host {
                host.stop();
                host.unmount();
            }
            Ok(())
        })
        .build("ufsmount", init)
        .map_err(|e| anyhow::anyhow!("cannot create the WinFsp service: {e:?}"))?;
    service
        .start()
        .map_err(|e| anyhow::anyhow!("cannot start: {e:?}"))?;
    service
        .join()
        .map_err(|e| anyhow::anyhow!("the file system stopped with an error: {e:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reparse_buffer_layout() {
        let target: Vec<u16> = r"dir\file".encode_utf16().collect();
        let mut buf = vec![0u8; 100];
        let n = symlink_reparse(&target, &mut buf).unwrap() as usize;
        assert_eq!(n, 20 + 2 * 16);
        assert_eq!(&buf[0..4], &IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        assert_eq!(u16::from_le_bytes([buf[4], buf[5]]) as usize, n - 8);
        assert_eq!(u16::from_le_bytes([buf[14], buf[15]]), 16);
        assert_eq!(&buf[16..20], &1u32.to_le_bytes());
        assert!(symlink_reparse(&target, &mut [0u8; 20]).is_err());
    }

    #[test]
    fn mount_point_forms() {
        assert_eq!(global_drive("u:").as_deref(), Some(r"\\.\U:"));
        assert_eq!(global_drive("U").as_deref(), Some(r"\\.\U:"));
        assert_eq!(global_drive(r"U:\").as_deref(), Some(r"\\.\U:"));
        assert_eq!(global_drive(r"C:\mnt\ufs"), None);
    }

    #[test]
    fn write_access_mask() {
        assert_eq!(WRITE_ACCESS, 0x500d_0156);
        // FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
        assert_eq!(0x0012_00a9 & WRITE_ACCESS, 0);
    }
}
