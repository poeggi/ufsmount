// SPDX-License-Identifier: AGPL-3.0-or-later
//! A small file logger: the tray has no console. One rotation at 1 MiB.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LEN: u64 = 1 << 20;

struct FileLog {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

/// `YYYY-MM-DDTHH:MM:SSZ` for seconds since the epoch.
pub fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

impl FileLog {
    fn open(&self) -> Option<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok()
    }
}

impl log::Log for FileLog {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        // rufs logs a superblock summary at info level on every open.
        let max = if meta.target().starts_with("rufs") {
            log::Level::Warn
        } else {
            log::Level::Info
        };
        meta.level() <= max
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let line = format!(
            "{} {:<5} {}: {}\r\n",
            utc(secs),
            record.level(),
            record.target(),
            record.args()
        );
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .is_some_and(|m| m.len() > MAX_LEN)
        {
            *guard = None;
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
        if guard.is_none() {
            *guard = self.open();
        }
        if let Some(f) = guard.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {}
}

/// Log to `dir\tray.log`, and log panics.
pub fn init(dir: &Path) {
    let logger = FileLog {
        path: dir.join("tray.log"),
        file: Mutex::new(None),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
}

#[cfg(test)]
mod tests {
    use super::utc;

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_794_703_445), "2026-11-15T00:44:05Z");
    }
}
