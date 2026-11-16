// SPDX-License-Identifier: AGPL-3.0-or-later
//! The WinFsp file system: read-only, every write path reports write protection.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    NTSTATUS, STATUS_BUFFER_TOO_SMALL, STATUS_DEVICE_DATA_ERROR, STATUS_DEVICE_NOT_CONNECTED,
    STATUS_END_OF_FILE, STATUS_FILE_CORRUPT_ERROR, STATUS_IO_DEVICE_ERROR,
    STATUS_MEDIA_WRITE_PROTECTED, STATUS_NOT_A_DIRECTORY, STATUS_NOT_A_REPARSE_POINT,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_UNSUCCESSFUL,
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
use crate::fs::Kind;
use crate::product::product;
use crate::security::{SecurityDescriptor, current_user_sid, is_elevated};
use crate::volume::{self, Attr, Node, Volume};

/// Oldest WinFsp accepted: 2.2.26286 (2026 Beta5). Older builds have kernel
/// vulnerabilities, among them CVE-2026-3006.
pub const MIN_WINFSP: (u32, u32, u32) = (2, 2, 26286);

const IO_REPARSE_TAG_SYMLINK: u32 = 0xa000_000c;
const SYMLINK_FLAG_RELATIVE: u32 = 1;
const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;
const WRITE_ACCESS: u32 = crate::security::WRITE_RIGHTS;

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
    fi.index_number = a.id;
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

pub struct ReadOnlyFs {
    vol: Arc<Volume>,
    sd: SecurityDescriptor,
    label: String,
}

pub struct Ctx {
    node: Node,
    size: u64,
    dir: DirBuffer,
}

impl ReadOnlyFs {
    pub fn new(vol: Arc<Volume>, sd: SecurityDescriptor) -> ReadOnlyFs {
        let label = if vol.info.label.is_empty() {
            product().fs.to_string()
        } else {
            crate::names::display(&vol.info.label)
        };
        ReadOnlyFs { vol, sd, label }
    }

    fn lookup(&self, name: &U16CStr) -> winfsp::Result<Node> {
        self.vol.lookup(name.as_slice()).map_err(map_err)
    }

    fn attr(&self, node: &Node) -> winfsp::Result<Attr> {
        self.vol.attr(node).map_err(map_err)
    }

    fn reparse(&self, node: &Node, buffer: &mut [u8]) -> winfsp::Result<u64> {
        if node.kind != Kind::Symlink {
            return Err(status(STATUS_NOT_A_REPARSE_POINT));
        }
        let target = self.vol.readlink(node.id).map_err(map_err)?;
        symlink_reparse(&self.vol.link_target_windows(&target), buffer)
    }
}

fn write_protected<T>() -> winfsp::Result<T> {
    Err(status(STATUS_MEDIA_WRITE_PROTECTED))
}

impl FileSystemContext for ReadOnlyFs {
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
            .read(context.node.id, offset, buffer)
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
            let table = self.vol.dir(context.node.id).map_err(map_err)?;
            let mut info: DirInfo = DirInfo::new();
            if context.node.id != self.vol.root() {
                let dots: [(&[u16], Node); 2] = [
                    (&[b'.' as u16], context.node),
                    (
                        &[b'.' as u16, b'.' as u16],
                        Node {
                            id: table.parent,
                            kind: Kind::Dir,
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
                    id: e.id,
                    kind: e.kind,
                    parent: context.node.id,
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
        let table = self.vol.dir(context.node.id).map_err(map_err)?;
        let e = table
            .find(file_name.as_slice())
            .ok_or(status(STATUS_OBJECT_NAME_NOT_FOUND))?;
        let child = Node {
            id: e.id,
            kind: e.kind,
            parent: context.node.id,
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
        out_volume_info.total_size = self.vol.info.total;
        out_volume_info.free_size = self.vol.info.free;
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

    fn set_extended_attributes(
        &self,
        _context: &Ctx,
        _buffer: &[u8],
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        write_protected()
    }
}

/// Look up DLLs only in System32 and in explicitly given directories.
/// Exits the process if that fails. Call it first, before any DLL is loaded.
pub fn harden_dll_search() {
    // SAFETY: plain process-wide setting.
    if let Err(e) = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } {
        eprintln!("error: SetDefaultDllDirectories failed: {e}");
        std::process::exit(1);
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

/// Load winfsp-x64.dll by full path from the WinFsp install directory and check its
/// version. Only the first call loads; later calls return its result.
pub fn load_winfsp() -> anyhow::Result<FspInit> {
    static INIT: OnceLock<Result<FspInit, String>> = OnceLock::new();
    INIT.get_or_init(|| load_winfsp_once().map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| anyhow::anyhow!(e))
}

fn winfsp_dll() -> anyhow::Result<HSTRING> {
    let dir = winfsp_install_dir()?;
    Ok(HSTRING::from(format!(
        r"{}\bin\winfsp-x64.dll",
        dir.trim_end_matches('\\')
    )))
}

/// The version of the installed WinFsp, such as "2.2.26286".
pub fn winfsp_version() -> anyhow::Result<String> {
    let (major, minor, build, _) = file_version(&winfsp_dll()?)?;
    Ok(format!("{major}.{minor}.{build}"))
}

fn load_winfsp_once() -> anyhow::Result<FspInit> {
    let dll = winfsp_dll()?;
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

/// 4096-byte sectors per allocation unit of `size` bytes: a power of two in 1..=32768.
fn sectors_per_allocation_unit(size: u32) -> u16 {
    1 << (size / 4096).clamp(1, 1 << 15).ilog2()
}

/// Build the file system host. `prefix` is the UNC volume prefix in launcher mode.
pub fn build_host(
    vol: Arc<Volume>,
    sd: SecurityDescriptor,
    prefix: Option<&str>,
) -> winfsp::Result<FileSystemHost<ReadOnlyFs>> {
    let mut vp = VolumeParams::new();
    vp.sector_size(4096)
        .sectors_per_allocation_unit(sectors_per_allocation_unit(vol.info.block_size))
        .max_component_length(255)
        .volume_serial_number(vol.info.serial)
        .file_info_timeout(60_000)
        .case_sensitive_search(true)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .reparse_points(true)
        .read_only_volume(true)
        .pass_query_directory_filename(true)
        .post_cleanup_when_modified_only(true)
        .filesystem_name(product().fs);
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
        ReadOnlyFs::new(vol, sd),
    )?)
}

/// `U`, `U:` or `U:\` as `U:`; `None` for a directory.
fn drive_letter(mountpoint: &str) -> Option<String> {
    let t = mountpoint.trim_end_matches(['\\', '/']);
    match t.as_bytes() {
        [l] | [l, b':'] if l.is_ascii_alphabetic() => {
            Some(format!("{}:", l.to_ascii_uppercase() as char))
        }
        _ => None,
    }
}

/// Mount on a drive letter or directory. With `global`, a drive letter goes
/// through the Mount Manager (`\\.\U:`): a global drive that the same user's
/// non-elevated Explorer sees too. That needs admin rights.
fn mount_on(
    host: &mut FileSystemHost<ReadOnlyFs>,
    mountpoint: &str,
    global: bool,
) -> anyhow::Result<String> {
    let (target, shown) = match drive_letter(mountpoint) {
        Some(letter) if global => (format!(r"\\.\{letter}"), letter),
        Some(letter) => (letter.clone(), letter),
        None => (mountpoint.to_string(), mountpoint.to_string()),
    };
    host.mount(target.as_str())
        .map_err(|e| anyhow::anyhow!("cannot mount on {shown}: {e}"))?;
    Ok(shown)
}

/// A mounted, running file system. WinFsp allows one service per process, so
/// a process with several file systems uses these directly.
pub struct Mount {
    host: Option<FileSystemHost<ReadOnlyFs>>,
    shown: String,
}

impl Mount {
    /// Create the file system, mount it (if `mountpoint` is given) and start
    /// serving. `load_winfsp()` must have succeeded before.
    pub fn start(
        vol: Arc<Volume>,
        sd: SecurityDescriptor,
        prefix: Option<&str>,
        mountpoint: Option<&str>,
        global: bool,
    ) -> anyhow::Result<Mount> {
        let mut host = build_host(vol, sd, prefix)
            .map_err(|e| anyhow::anyhow!("cannot create the file system: {e:?}"))?;
        let shown = match mountpoint {
            Some(m) => mount_on(&mut host, m, global)?,
            None => prefix.unwrap_or_default().to_string(),
        };
        if let Err(e) = host.start_with_threads(4) {
            host.unmount();
            anyhow::bail!("cannot start the file system: {e:?}");
        }
        Ok(Mount {
            host: Some(host),
            shown,
        })
    }

    /// The drive letter, folder or prefix it is served on.
    pub fn shown(&self) -> &str {
        &self.shown
    }

    /// Stop serving and unmount. Drops the file system, which closes the disk.
    /// Calling it again does nothing.
    pub fn stop(&mut self) {
        if let Some(mut host) = self.host.take() {
            host.stop();
            host.unmount();
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How often `serve()` asks whether the disk is still there.
const WATCH_EVERY: Duration = Duration::from_secs(2);

/// FspServiceLoop calls the start callback at once unless it fails before.
/// Without that call for this long, `serve()` stops waiting for it and takes
/// the loop's result.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the unmount of a gone disk may take before the process exits
/// without it. A read stuck on a dying disk holds up the dispatcher threads,
/// and with them the unmount.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// The disk of a `serve()` mount went away. The mount is stopped and unmounted.
#[derive(Debug, thiserror::Error)]
#[error("{0}: the disk is gone")]
pub struct DiskGone(pub String);

/// Exit code of the command line tool after `DiskGone`.
pub const EXIT_DISK_GONE: u8 = 3;

/// Whether the start callback ran. Once `serve()` gives up waiting for it, a
/// start is refused: nothing would watch the disk.
struct StartGate(AtomicU8);

impl StartGate {
    const WAITING: u8 = 0;
    const STARTED: u8 = 1;
    const ABANDONED: u8 = 2;

    fn new() -> StartGate {
        StartGate(AtomicU8::new(Self::WAITING))
    }

    /// The start callback asks first. False once `serve()` gave up.
    fn start(&self) -> bool {
        self.leave_waiting(Self::STARTED)
    }

    /// Stop waiting for the start. False when the start callback ran first.
    fn abandon(&self) -> bool {
        self.leave_waiting(Self::ABANDONED)
    }

    fn started(&self) -> bool {
        self.0.load(Ordering::SeqCst) == Self::STARTED
    }

    fn leave_waiting(&self, to: u8) -> bool {
        self.0
            .compare_exchange(Self::WAITING, to, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

/// Why `wait_for_end` returned.
#[derive(Debug, PartialEq)]
enum End {
    /// The service ended, or did not start: `join()` has the result.
    Stopped,
    DiskGone,
}

/// Wait for a message on `ended` (the service ended). Asks `alive` every
/// `every`. After `start_timeout` without a start, abandons it and returns.
fn wait_for_end(
    ended: &Receiver<()>,
    every: Duration,
    start_timeout: Duration,
    mut alive: impl FnMut() -> bool,
    gate: &StartGate,
) -> End {
    let begin = Instant::now();
    loop {
        match ended.recv_timeout(every) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return End::Stopped,
            Err(RecvTimeoutError::Timeout) => {
                if !alive() {
                    return End::DiskGone;
                }
                if begin.elapsed() >= start_timeout && gate.abandon() {
                    return End::Stopped;
                }
            }
        }
    }
}

/// Serve the file system until Ctrl+C, Ctrl+Break, console close or the
/// disk going away (`DiskGone`). `prefix` is the launcher's UNC volume prefix.
pub fn serve(
    vol: Arc<Volume>,
    sd: SecurityDescriptor,
    prefix: Option<String>,
    mountpoint: Option<String>,
    global: bool,
) -> anyhow::Result<()> {
    let init = load_winfsp()?;
    // WinFsp reports a failed start only through its own exit code, which
    // winfsp-rs does not expose; keep the reason here.
    let failure: Arc<Mutex<Option<String>>> = Arc::default();
    let fail = failure.clone();
    let gate = Arc::new(StartGate::new());
    let start_gate = gate.clone();
    let dev = vol.device().clone();
    let (tx, ended) = mpsc::channel();
    let stop_tx = tx.clone();
    let mut service = FileSystemServiceBuilder::new()
        .with_start(move || {
            if !start_gate.start() {
                log::error!(
                    "the WinFsp service started after {} s, too late",
                    START_TIMEOUT.as_secs()
                );
                return Err(status(STATUS_UNSUCCESSFUL));
            }
            // A service stop before this callback does nothing: refuse a disk found gone.
            let mount = if vol.device().is_gone() {
                Err(anyhow::anyhow!("{}: the disk is gone", vol.device().name()))
            } else {
                Mount::start(
                    vol.clone(),
                    sd.clone(),
                    prefix.as_deref(),
                    mountpoint.as_deref(),
                    global,
                )
            };
            match mount {
                Ok(m) => {
                    match &mountpoint {
                        Some(_) => eprintln!(
                            "Mounted read-only on {}. Press Ctrl+C to unmount.",
                            m.shown()
                        ),
                        None => eprintln!("Serving {} read-only.", m.shown()),
                    }
                    Ok(m)
                }
                Err(e) => {
                    log::error!("{e:#}");
                    *fail.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("{e:#}"));
                    let _ = tx.send(());
                    Err(status(STATUS_UNSUCCESSFUL))
                }
            }
        })
        .with_stop(move |mount| {
            if let Some(m) = mount {
                m.stop();
            }
            let _ = stop_tx.send(());
            Ok(())
        })
        .build(product().id, init)
        .map_err(|e| anyhow::anyhow!("cannot create the WinFsp service: {e:?}"))?;
    service
        .start()
        .map_err(|e| anyhow::anyhow!("cannot start: {e:?}"))?;
    let end = wait_for_end(&ended, WATCH_EVERY, START_TIMEOUT, || dev.is_alive(), &gate);
    let joined = if end == End::DiskGone {
        // The deadline thread ends when `_stopped` drops after the join.
        let (_stopped, stopping) = mpsc::channel::<()>();
        let name = dev.name().to_string();
        std::thread::spawn(move || {
            if let Err(RecvTimeoutError::Timeout) = stopping.recv_timeout(STOP_TIMEOUT) {
                log::error!("{name}: the disk is gone, the unmount did not finish");
                std::process::exit(EXIT_DISK_GONE.into());
            }
        });
        // FspServiceStop runs the stop callback here: stop and unmount.
        service.stop();
        service.join()
    } else {
        service.join()
    };
    if dev.is_gone() {
        return Err(DiskGone(dev.name().to_string()).into());
    }
    joined.map_err(|e| anyhow::anyhow!("the file system stopped with an error: {e:?}"))?;
    if let Some(e) = failure
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        anyhow::bail!(e);
    }
    if !gate.started() {
        anyhow::bail!("the WinFsp service did not start");
    }
    Ok(())
}

/// Mount from the command line.
pub fn mount_console(vol: Volume, mountpoint: &str, sddl: Option<&str>) -> anyhow::Result<()> {
    let sd = match sddl {
        Some(s) => SecurityDescriptor::for_files(s)?,
        None => SecurityDescriptor::read_only_for(&current_user_sid()?)?,
    };
    serve(
        Arc::new(vol),
        sd,
        None,
        Some(mountpoint.to_string()),
        is_elevated(),
    )
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
        assert_eq!(drive_letter("u:").as_deref(), Some("U:"));
        assert_eq!(drive_letter("U").as_deref(), Some("U:"));
        assert_eq!(drive_letter(r"U:\").as_deref(), Some("U:"));
        assert_eq!(drive_letter(r"C:\mnt\disk"), None);
    }

    #[test]
    fn min_winfsp_matches_the_setup() {
        let iss = include_str!("../../../installer/winfsp.iss");
        let get = |name: &str| -> u32 {
            let prefix = format!("#define {name} ");
            iss.lines()
                .find_map(|l| l.strip_prefix(prefix.as_str()))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        assert_eq!(
            (
                get("WinFspMinMajor"),
                get("WinFspMinMinor"),
                get("WinFspMinBuild")
            ),
            MIN_WINFSP
        );
    }

    #[test]
    fn allocation_unit_from_fragment_size() {
        let cases = [
            (0, 1),
            (512, 1),
            (4096, 1),
            (8192, 2),
            (12288, 2),
            (65536, 16),
            (1 << 27, 32768),
            (1 << 28, 32768),
            (u32::MAX, 32768),
        ];
        for (fsize, sectors) in cases {
            assert_eq!(sectors_per_allocation_unit(fsize), sectors, "fsize {fsize}");
        }
    }

    #[test]
    fn service_end_and_disk_watch() {
        let ms = Duration::from_millis;
        let (tx, rx) = mpsc::channel();
        tx.send(()).unwrap();
        let gate = StartGate::new();
        assert_eq!(
            wait_for_end(&rx, ms(1), ms(1), || true, &gate),
            End::Stopped
        );

        let mut checks = 0;
        let alive = || {
            checks += 1;
            checks < 5
        };
        assert_eq!(
            wait_for_end(&rx, ms(1), ms(10_000), alive, &gate),
            End::DiskGone
        );
        assert_eq!(
            wait_for_end(&rx, ms(1), ms(1), || false, &gate),
            End::DiskGone
        );

        // Started: the disk is still watched after the start timeout.
        assert!(gate.start());
        let mut checks = 0;
        let alive = || {
            checks += 1;
            checks < 20
        };
        assert_eq!(wait_for_end(&rx, ms(1), ms(1), alive, &gate), End::DiskGone);
        assert!(gate.started());

        // Not started: waiting ends after the start timeout, a late start is refused.
        let gate = StartGate::new();
        assert_eq!(
            wait_for_end(&rx, ms(1), ms(20), || true, &gate),
            End::Stopped
        );
        assert!(!gate.start());
        assert!(!gate.started());

        drop(tx);
        assert_eq!(
            wait_for_end(&rx, ms(1), ms(10_000), || true, &StartGate::new()),
            End::Stopped
        );
    }

    #[test]
    fn write_access_mask() {
        assert_eq!(WRITE_ACCESS, crate::security::WRITE_RIGHTS);
        // FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
        assert_eq!(0x0012_00a9 & WRITE_ACCESS, 0);
    }
}
