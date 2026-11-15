// SPDX-License-Identifier: AGPL-3.0-or-later
//! Read-only access to a raw disk or an image file.
//!
//! Reads are served from a cache of 64 KiB chunks at aligned device offsets.
//! A chunk that fails is retried one sector at a time for the requested range
//! only, so one bad sector does not take its neighbours down with it.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    DISK_GEOMETRY_EX, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_DISK_GET_LENGTH_INFO,
};

/// Size of a cache chunk. A multiple of every sector size in use.
pub const CHUNK: u64 = 64 * 1024;
/// Number of cached chunks (64 MiB).
const CACHE_CHUNKS: usize = 1024;

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_NOT_READY: u32 = 21;
const ERROR_CRC: u32 = 23;
const ERROR_SECTOR_NOT_FOUND: u32 = 27;
const ERROR_READ_FAULT: u32 = 30;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_DEV_NOT_EXIST: u32 = 55;
const ERROR_NO_SUCH_DEVICE: u32 = 433;
const ERROR_MEDIA_CHANGED: u32 = 1110;
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;
const ERROR_DEVICE_REMOVED: u32 = 1617;

/// A failed read from the disk, with the Win32 error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("disk read failed at byte {offset}: {}", win32_message(*.code))]
pub struct DeviceError {
    pub code: u32,
    pub offset: u64,
}

impl DeviceError {
    /// The disk is gone (unplugged, powered off).
    pub fn is_gone(&self) -> bool {
        matches!(
            self.code,
            ERROR_FILE_NOT_FOUND
                | ERROR_NOT_READY
                | ERROR_DEV_NOT_EXIST
                | ERROR_NO_SUCH_DEVICE
                | ERROR_MEDIA_CHANGED
                | ERROR_DEVICE_NOT_CONNECTED
                | ERROR_DEVICE_REMOVED
        )
    }

    /// The data on the medium is unreadable (bad sector). ERROR_IO_DEVICE is
    /// not one: USB bridges also report it for a reset or an unplug.
    pub fn is_media_error(&self) -> bool {
        matches!(
            self.code,
            ERROR_CRC | ERROR_SECTOR_NOT_FOUND | ERROR_READ_FAULT
        )
    }
}

fn win32_message(code: u32) -> String {
    format!(
        "{} (Win32 error {code})",
        io::Error::from_raw_os_error(code as i32)
    )
}

/// Find a [`DeviceError`] inside an [`io::Error`], also when rufs wrapped it.
pub fn device_error(e: &io::Error) -> Option<DeviceError> {
    let mut inner = e.get_ref()?;
    loop {
        if let Some(d) = inner.downcast_ref::<DeviceError>() {
            return Some(*d);
        }
        inner = inner.downcast_ref::<io::Error>()?.get_ref()?;
    }
}

/// The operating system side of a disk: exact reads at byte offsets.
pub trait RawDisk: Send + Sync {
    /// Fill `buf` from `offset`. Offset and length are multiples of the sector
    /// size, except for the tail of an image file. Returns a Win32 error code.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), u32>;

    /// Whether the device still answers. Asked after an unexpected error.
    fn is_alive(&self) -> bool {
        true
    }

    /// The OS handle of a disk, for removal requests.
    fn handle(&self) -> Option<usize> {
        None
    }
}

struct OsDisk {
    file: File,
    is_disk: bool,
}

impl RawDisk for OsDisk {
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), u32> {
        let mut done = 0;
        while done < buf.len() {
            match self.file.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) => return Err(ERROR_HANDLE_EOF),
                Ok(n) => done += n,
                Err(e) => return Err(e.raw_os_error().unwrap_or(ERROR_READ_FAULT as i32) as u32),
            }
        }
        Ok(())
    }

    fn is_alive(&self) -> bool {
        !self.is_disk || disk_geometry(&self.file).is_ok()
    }

    fn handle(&self) -> Option<usize> {
        self.is_disk.then(|| self.file.as_raw_handle() as usize)
    }
}

/// Where the data comes from, as given by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `\\.\PhysicalDriveN`
    PhysicalDrive(u32),
    /// An image file.
    Image(std::path::PathBuf),
}

impl Source {
    /// Parse `PhysicalDriveN`, `\\.\PhysicalDriveN` or an image path.
    pub fn parse(s: &str) -> Source {
        let bare = s
            .strip_prefix(r"\\.\")
            .or_else(|| s.strip_prefix(r"\\?\"))
            .unwrap_or(s);
        if let Some(n) = strip_prefix_ci(bare, "PhysicalDrive")
            && !n.is_empty()
            && n.len() <= 3
            && n.bytes().all(|b| b.is_ascii_digit())
        {
            return Source::PhysicalDrive(n.parse().unwrap());
        }
        Source::Image(s.into())
    }

    pub fn path(&self) -> String {
        match self {
            Source::PhysicalDrive(n) => format!(r"\\.\PhysicalDrive{n}"),
            Source::Image(p) => p.display().to_string(),
        }
    }
}

pub fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct Page([u8; 4096]);

/// A buffer whose memory is aligned to 4096 bytes, as some USB bridges need.
struct AlignedBuf {
    pages: Vec<Page>,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        AlignedBuf {
            pages: vec![Page([0; 4096]); len.div_ceil(4096)],
            len,
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the pages are contiguous plain bytes and cover at least `len` bytes.
        unsafe { std::slice::from_raw_parts_mut(self.pages.as_mut_ptr().cast::<u8>(), self.len) }
    }
}

struct Cache {
    chunks: HashMap<u64, (Arc<[u8]>, u64)>,
    clock: u64,
}

impl Cache {
    fn get(&mut self, idx: u64) -> Option<Arc<[u8]>> {
        self.clock += 1;
        let clock = self.clock;
        self.chunks.get_mut(&idx).map(|(data, used)| {
            *used = clock;
            data.clone()
        })
    }

    fn put(&mut self, idx: u64, data: Arc<[u8]>) {
        if self.chunks.len() >= CACHE_CHUNKS {
            // Drop the oldest eighth in one go, so a full cache does not scan on every miss.
            let mut ages: Vec<(u64, u64)> =
                self.chunks.iter().map(|(k, (_, u))| (*u, *k)).collect();
            ages.sort_unstable();
            for (_, k) in ages.iter().take(CACHE_CHUNKS / 8) {
                self.chunks.remove(k);
            }
        }
        self.clock += 1;
        self.chunks.insert(idx, (data, self.clock));
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A sector size reads and partition tables can use: [`CHUNK`] is a multiple of it.
fn supported_sector(size: u64) -> bool {
    size.is_power_of_two() && (512..=4096).contains(&size)
}

/// A disk or image, opened read-only.
pub struct Device {
    raw: Box<dyn RawDisk>,
    name: String,
    len: u64,
    /// Unit of raw reads: the sector size the device reports.
    io_sector: u64,
    /// Sector size for partition tables; differs only with `--sector-size`.
    table_sector: u64,
    cache: Mutex<Cache>,
    /// Sectors that failed with a media error, and the error code.
    bad_sectors: Mutex<HashMap<u64, u32>>,
    /// Chunks whose read failed on a present disk: read sector by sector.
    /// Any error counts, as some USB bridges report a bad sector as ERROR_IO_DEVICE.
    bad_chunks: Mutex<HashSet<u64>>,
    gone: AtomicBool,
    last_error: Mutex<Option<DeviceError>>,
}

impl Device {
    /// Open a disk or image. Nothing is ever written.
    pub fn open(source: &Source, sector_override: Option<u32>) -> anyhow::Result<Device> {
        let path = source.path();
        let file = OpenOptions::new()
            .read(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .open(&path)
            .map_err(|e| {
                if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
                    && matches!(source, Source::PhysicalDrive(_))
                {
                    anyhow::anyhow!(
                        "{path}: access denied. Raw disk access needs administrator rights."
                    )
                } else {
                    anyhow::anyhow!("{path}: {e}")
                }
            })?;
        let is_disk = matches!(source, Source::PhysicalDrive(_)) || file.metadata()?.len() == 0;
        let (len, io_sector) = if is_disk {
            disk_geometry(&file)
                .map_err(|e| anyhow::anyhow!("{path}: cannot query the size: {e}"))?
        } else {
            (file.metadata()?.len(), 512)
        };
        anyhow::ensure!(len > 0, "{path} is empty");
        anyhow::ensure!(
            supported_sector(io_sector),
            "{path}: unsupported sector size {io_sector} reported by the disk"
        );
        let table_sector = sector_override.map(u64::from).unwrap_or(io_sector);
        anyhow::ensure!(
            supported_sector(table_sector),
            "unsupported sector size {table_sector}"
        );
        let mut dev = Device::from_raw(Box::new(OsDisk { file, is_disk }), path, len, io_sector);
        dev.table_sector = table_sector;
        Ok(dev)
    }

    /// Build a device on any [`RawDisk`], for example a test double.
    /// Panics unless `sector` is a power of two from 512 to 4096.
    pub fn from_raw(raw: Box<dyn RawDisk>, name: String, len: u64, sector: u64) -> Device {
        assert!(supported_sector(sector), "unsupported sector size {sector}");
        Device {
            raw,
            name,
            len,
            io_sector: sector,
            table_sector: sector,
            cache: Mutex::new(Cache {
                chunks: HashMap::new(),
                clock: 0,
            }),
            bad_sectors: Mutex::new(HashMap::new()),
            bad_chunks: Mutex::new(HashSet::new()),
            gone: AtomicBool::new(false),
            last_error: Mutex::new(None),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The OS handle of a physical disk; None for an image.
    pub fn disk_handle(&self) -> Option<usize> {
        self.raw.handle()
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The sector size partition tables are read with.
    pub fn sector_size(&self) -> u64 {
        self.table_sector
    }

    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }

    /// Ask the disk whether it still answers (an IOCTL that does not spin it up).
    /// A disk that does not answer counts as gone from then on.
    pub fn is_alive(&self) -> bool {
        if self.is_gone() {
            return false;
        }
        if self.raw.is_alive() {
            return true;
        }
        if !self.gone.swap(true, Ordering::Relaxed) {
            log::error!("{}: the disk is gone", self.name);
        }
        false
    }

    /// The most recent device error, if any. Cleared by this call.
    pub fn take_last_error(&self) -> Option<DeviceError> {
        lock(&self.last_error).take()
    }

    /// The code of a failed raw read, or ERROR_DEVICE_NOT_CONNECTED when the
    /// disk no longer answers. Asked before an error is remembered as a bad sector.
    fn read_error(&self, code: u32, offset: u64) -> DeviceError {
        let mut err = DeviceError { code, offset };
        if !err.is_gone() && !self.raw.is_alive() {
            err.code = ERROR_DEVICE_NOT_CONNECTED;
        }
        err
    }

    fn fail(&self, code: u32, offset: u64) -> io::Error {
        let err = DeviceError { code, offset };
        if err.is_gone() && !self.gone.swap(true, Ordering::Relaxed) {
            log::error!("{}: the disk is gone ({err})", self.name);
        }
        *lock(&self.last_error) = Some(err);
        io::Error::other(err)
    }

    /// Read exactly `buf.len()` bytes at `offset`. The range must lie within the device.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&end| end <= self.len)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("read past the end of {} at byte {offset}", self.name),
                )
            })?;
        if self.is_gone() {
            return Err(self.fail(ERROR_DEVICE_NOT_CONNECTED, offset));
        }
        let mut pos = offset;
        while pos < end {
            let idx = pos / CHUNK;
            let chunk_start = idx * CHUNK;
            let from = (pos - chunk_start) as usize;
            let to = (end.min(chunk_start + CHUNK) - chunk_start) as usize;
            let at = (pos - offset) as usize;
            let dst = &mut buf[at..at + (to - from)];
            match self.chunk(idx)? {
                Some(data) => dst.copy_from_slice(&data[from..to]),
                None => self.read_sectors(chunk_start + from as u64, dst)?,
            }
            pos = chunk_start + to as u64;
        }
        Ok(())
    }

    /// The cached or freshly read chunk, or `None` if it must be read sector by sector.
    fn chunk(&self, idx: u64) -> io::Result<Option<Arc<[u8]>>> {
        if let Some(data) = lock(&self.cache).get(idx) {
            return Ok(Some(data));
        }
        if lock(&self.bad_chunks).contains(&idx) {
            return Ok(None);
        }
        let start = idx * CHUNK;
        let len = CHUNK.min(self.len - start) as usize;
        let mut buf = AlignedBuf::new(len);
        // No lock is held here: a failing USB read can take seconds.
        match self.raw.read_exact_at(start, buf.as_mut_slice()) {
            Ok(()) => {
                let data: Arc<[u8]> = Arc::from(&*buf.as_mut_slice());
                lock(&self.cache).put(idx, data.clone());
                Ok(Some(data))
            }
            Err(code) => {
                let err = self.read_error(code, start);
                if err.is_gone() {
                    return Err(self.fail(err.code, start));
                }
                log::warn!(
                    "{}: chunk at byte {start} failed ({err}), reading sector by sector",
                    self.name
                );
                lock(&self.bad_chunks).insert(idx);
                Ok(None)
            }
        }
    }

    /// Read a range sector by sector, uncached. Used around bad sectors.
    fn read_sectors(&self, offset: u64, dst: &mut [u8]) -> io::Result<()> {
        let sector = self.io_sector;
        let first = offset / sector;
        let last = (offset + dst.len() as u64 - 1) / sector;
        let mut buf = AlignedBuf::new(sector as usize);
        for s in first..=last {
            let s_start = s * sector;
            let s_len = sector.min(self.len - s_start) as usize;
            if let Some(&code) = lock(&self.bad_sectors).get(&s) {
                return Err(self.fail(code, s_start));
            }
            let data = &mut buf.as_mut_slice()[..s_len];
            if let Err(code) = self.raw.read_exact_at(s_start, data) {
                let err = self.read_error(code, s_start);
                if err.is_media_error() {
                    log::error!("{}: unreadable sector {s} (byte {s_start})", self.name);
                    lock(&self.bad_sectors).insert(s, err.code);
                }
                return Err(self.fail(err.code, s_start));
            }
            let lo = offset.max(s_start);
            let hi = (offset + dst.len() as u64).min(s_start + s_len as u64);
            dst[(lo - offset) as usize..(hi - offset) as usize]
                .copy_from_slice(&data[(lo - s_start) as usize..(hi - s_start) as usize]);
        }
        Ok(())
    }
}

/// Length in bytes and logical sector size of a disk or volume handle.
fn disk_geometry(file: &File) -> io::Result<(u64, u64)> {
    let handle = HANDLE(file.as_raw_handle());
    let mut length = GET_LENGTH_INFORMATION::default();
    let mut returned = 0u32;
    // SAFETY: the output buffer is a properly sized GET_LENGTH_INFORMATION.
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some((&raw mut length).cast()),
            size_of::<GET_LENGTH_INFORMATION>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(io::Error::other)?;
    let mut geometry = DISK_GEOMETRY_EX::default();
    // SAFETY: the output buffer is a properly sized DISK_GEOMETRY_EX.
    let sector = match unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            None,
            0,
            Some((&raw mut geometry).cast()),
            size_of::<DISK_GEOMETRY_EX>() as u32,
            Some(&mut returned),
            None,
        )
    } {
        Ok(()) => u64::from(geometry.Geometry.BytesPerSector),
        Err(_) => 512,
    };
    Ok((length.Length as u64, sector))
}

/// One partition as a `Read + Seek` stream for rufs.
///
/// `len` is a multiple of the rufs block size. A read that starts at or past
/// `len` is an error: rufs would turn a short read into zeros.
pub struct PartitionReader {
    dev: Arc<Device>,
    base: u64,
    len: u64,
    pos: u64,
}

impl PartitionReader {
    pub fn new(dev: Arc<Device>, base: u64, len: u64, block_size: u64) -> PartitionReader {
        PartitionReader {
            dev,
            base,
            len: len / block_size * block_size,
            pos: 0,
        }
    }

    pub fn device(&self) -> &Arc<Device> {
        &self.dev
    }
}

impl Read for PartitionReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "read at byte {} is outside the partition ({} bytes)",
                    self.pos, self.len
                ),
            ));
        }
        let n = (buf.len() as u64).min(self.len - self.pos) as usize;
        self.dev.read(self.base + self.pos, &mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for PartitionReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
        };
        self.pos = new.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self.pos)
    }
}

impl Write for PartitionReader {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[doc(hidden)]
pub mod testing {
    //! A fake disk that enforces raw disk rules and injects faults. For tests.

    use super::*;
    use std::sync::atomic::AtomicUsize;

    pub struct FakeDisk {
        pub data: Vec<u8>,
        pub sector: u64,
        /// Byte ranges that fail, with the Win32 code.
        pub bad: Vec<(std::ops::Range<u64>, u32)>,
        pub unplug_after: Option<usize>,
        pub reads: Arc<AtomicUsize>,
    }

    impl FakeDisk {
        pub fn new(data: Vec<u8>, sector: u64) -> FakeDisk {
            FakeDisk {
                data,
                sector,
                bad: Vec::new(),
                unplug_after: None,
                reads: Arc::new(AtomicUsize::new(0)),
            }
        }

        pub fn into_device(self) -> Arc<Device> {
            let len = self.data.len() as u64;
            let sector = self.sector;
            Arc::new(Device::from_raw(Box::new(self), "fake".into(), len, sector))
        }
    }

    impl RawDisk for FakeDisk {
        fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), u32> {
            let n = self.reads.fetch_add(1, Ordering::Relaxed);
            if self.unplug_after.is_some_and(|after| n >= after) {
                return Err(ERROR_DEVICE_NOT_CONNECTED);
            }
            assert_eq!(offset % self.sector, 0, "unaligned offset {offset}");
            assert_eq!(
                buf.len() as u64 % self.sector,
                0,
                "unaligned length {}",
                buf.len()
            );
            assert_eq!(buf.as_ptr() as usize % 4096, 0, "unaligned buffer");
            let end = offset + buf.len() as u64;
            if end > self.data.len() as u64 {
                return Err(ERROR_SECTOR_NOT_FOUND);
            }
            if let Some((_, code)) = self
                .bad
                .iter()
                .find(|(r, _)| r.start < end && offset < r.end)
            {
                return Err(*code);
            }
            buf.copy_from_slice(&self.data[offset as usize..end as usize]);
            Ok(())
        }

        fn is_alive(&self) -> bool {
            self.unplug_after
                .is_none_or(|after| self.reads.load(Ordering::Relaxed) < after)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeDisk;
    use super::*;

    const ERROR_IO_DEVICE: u32 = 1117;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn unaligned_reads_and_tail() {
        // 3.5 chunks of 512-byte sectors: the last chunk is short.
        let len = (3 * CHUNK + CHUNK / 2) as usize;
        let data = pattern(len);
        let dev = FakeDisk::new(data.clone(), 512).into_device();
        for (off, n) in [
            (0, 1),
            (511, 3),
            (CHUNK - 5, 10),
            (len as u64 - 100, 100),
            (1000, 200_000),
        ] {
            let mut buf = vec![0; n];
            dev.read(off, &mut buf).unwrap();
            assert_eq!(buf, data[off as usize..off as usize + n], "offset {off}");
        }
        let mut buf = [0u8; 2];
        assert!(dev.read(len as u64 - 1, &mut buf).is_err());
    }

    #[test]
    fn chunks_are_cached() {
        let disk = FakeDisk::new(pattern(4 * CHUNK as usize), 512);
        let reads = disk.reads.clone();
        let dev = disk.into_device();
        let mut buf = vec![0; 100];
        for _ in 0..5 {
            dev.read(10, &mut buf).unwrap();
        }
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bad_sector_spares_neighbours() {
        let data = pattern(4 * CHUNK as usize);
        let mut disk = FakeDisk::new(data.clone(), 512);
        disk.bad.push((CHUNK + 1024..CHUNK + 1536, ERROR_CRC));
        let reads = disk.reads.clone();
        let dev = disk.into_device();

        let mut buf = vec![0; 1024];
        dev.read(CHUNK, &mut buf).unwrap();
        assert_eq!(buf, data[CHUNK as usize..CHUNK as usize + 1024]);

        let mut buf = vec![0; 100];
        let err = dev.read(CHUNK + 1100, &mut buf).unwrap_err();
        let derr = device_error(&err).unwrap();
        assert!(derr.is_media_error());
        assert_eq!(derr.offset, CHUNK + 1024);
        assert_eq!(dev.take_last_error(), Some(derr));

        // The bad sector is not read again, and keeps its error code.
        let before = reads.load(Ordering::Relaxed);
        let err = dev.read(CHUNK + 1100, &mut buf).unwrap_err();
        assert_eq!(device_error(&err).unwrap().code, ERROR_CRC);
        assert_eq!(reads.load(Ordering::Relaxed), before);

        let mut buf = vec![0; 512];
        dev.read(CHUNK + 1536, &mut buf).unwrap();
        assert_eq!(buf, data[(CHUNK + 1536) as usize..(CHUNK + 2048) as usize]);
    }

    #[test]
    fn other_errors_mark_only_the_chunk() {
        // A timeout, and the code some USB bridges give for a bad sector.
        for code in [121, ERROR_IO_DEVICE] {
            let data = pattern(2 * CHUNK as usize);
            let mut disk = FakeDisk::new(data.clone(), 512);
            disk.bad.push((0..512, code));
            let reads = disk.reads.clone();
            let dev = disk.into_device();
            let mut buf = vec![0; 10];
            let err = dev.read(0, &mut buf).unwrap_err();
            assert_eq!(device_error(&err).unwrap().code, code);
            assert!(!dev.is_gone());
            assert!(lock(&dev.bad_sectors).is_empty(), "code {code}");
            assert_eq!(*lock(&dev.bad_chunks), HashSet::from([0]), "code {code}");

            // The sector is read again, the whole chunk is not.
            let before = reads.load(Ordering::Relaxed);
            assert!(dev.read(0, &mut buf).is_err());
            assert_eq!(reads.load(Ordering::Relaxed), before + 1, "code {code}");

            let mut buf = vec![0; 1024];
            dev.read(512, &mut buf).unwrap();
            assert_eq!(buf, data[512..1536]);
        }
    }

    #[test]
    #[should_panic(expected = "unsupported sector size 520")]
    fn odd_sector_size_is_refused() {
        FakeDisk::new(pattern(4096), 520).into_device();
    }

    #[test]
    fn media_error_on_a_dead_device_means_gone() {
        for code in [ERROR_CRC, ERROR_IO_DEVICE] {
            let mut disk = FakeDisk::new(pattern(2 * CHUNK as usize), 512);
            disk.bad.push((0..CHUNK, code));
            disk.unplug_after = Some(1);
            let dev = disk.into_device();
            let mut buf = vec![0; 10];
            let err = dev.read(0, &mut buf).unwrap_err();
            assert!(device_error(&err).unwrap().is_gone(), "code {code}");
            assert!(dev.is_gone());
            assert!(lock(&dev.bad_sectors).is_empty());
            assert!(lock(&dev.bad_chunks).is_empty());
        }
    }

    #[test]
    fn sector_sizes() {
        for ok in [512, 1024, 2048, 4096] {
            assert!(supported_sector(ok));
        }
        for bad in [0, 1, 256, 520, 3072, 8192] {
            assert!(!supported_sector(bad), "{bad}");
        }
    }

    #[test]
    fn unplug_fails_fast() {
        let mut disk = FakeDisk::new(pattern(4 * CHUNK as usize), 512);
        disk.unplug_after = Some(1);
        let dev = disk.into_device();
        let mut buf = vec![0; 10];
        dev.read(0, &mut buf).unwrap();
        let err = dev.read(CHUNK, &mut buf).unwrap_err();
        assert!(device_error(&err).unwrap().is_gone());
        assert!(dev.is_gone());
        assert!(
            dev.read(0, &mut buf).is_err(),
            "cached data is not served after unplug"
        );
    }

    #[test]
    fn unknown_error_on_a_dead_device_means_gone() {
        let mut disk = FakeDisk::new(pattern(2 * CHUNK as usize), 512);
        disk.bad.push((0..CHUNK, 31)); // ERROR_GEN_FAILURE
        disk.unplug_after = Some(1);
        let dev = disk.into_device();
        let mut buf = vec![0; 10];
        assert!(dev.read(0, &mut buf).is_err());
        assert!(dev.is_gone());
    }

    #[test]
    fn partition_reader_bounds() {
        let data = pattern(8192);
        let dev = FakeDisk::new(data.clone(), 512).into_device();
        let mut r = PartitionReader::new(dev, 1000, 5000, 2048);
        assert_eq!(r.len, 4096);
        let mut buf = [0u8; 4096];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf[..], data[1000..5096]);
        assert!(r.read(&mut buf).is_err());
        r.seek(SeekFrom::Start(100_000)).unwrap();
        assert!(r.read(&mut buf).is_err());
        assert!(r.write(&buf).is_err());
    }

    #[test]
    fn source_parse() {
        assert_eq!(Source::parse("PhysicalDrive1"), Source::PhysicalDrive(1));
        assert_eq!(
            Source::parse(r"\\.\physicaldrive12"),
            Source::PhysicalDrive(12)
        );
        assert!(matches!(Source::parse("PhysicalDrive"), Source::Image(_)));
        assert!(matches!(Source::parse("disk.img"), Source::Image(_)));
    }
}
