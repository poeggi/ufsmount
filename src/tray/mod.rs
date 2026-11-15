// SPDX-License-Identifier: AGPL-3.0-or-later
//! `ufsmount-tray.exe`: finds UFS disks, mounts them and lives in the
//! notification area.
//!
//! Started without arguments (Start menu) and not elevated, it hands over to
//! the elevated scheduled task, or asks for elevation. The task-started tray
//! (`--from-task`) takes no other input.

mod about;
mod instance;
pub mod log;
mod path;
pub mod sys;
pub mod task;
mod ui;
mod update;
pub mod worker;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use windows::Win32::Foundation::HWND;

use crate::security::{SecurityDescriptor, is_elevated};

/// Exit code of a task-started tray without admin rights (standard user).
const EXIT_NOT_ELEVATED: u8 = 2;
/// Exit code for unknown arguments. No message box: the installer runs older
/// trays, which must never block on a newer argument.
const EXIT_USAGE: u8 = 64;

#[derive(Debug, PartialEq)]
enum Mode {
    Start,
    FromTask,
    Quit,
    InstallTask { logon: bool },
    RemoveTask,
    AddToPath,
    RemoveFromPath,
    Source(PathBuf),
}

fn parse(args: &[String]) -> Result<Mode, String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] => Ok(Mode::Start),
        [task::FROM_TASK] => Ok(Mode::FromTask),
        ["--quit"] => Ok(Mode::Quit),
        ["--install-task"] => Ok(Mode::InstallTask { logon: false }),
        ["--install-task", "--logon"] => Ok(Mode::InstallTask { logon: true }),
        ["--remove-task"] => Ok(Mode::RemoveTask),
        ["--add-to-path"] => Ok(Mode::AddToPath),
        ["--remove-from-path"] => Ok(Mode::RemoveFromPath),
        ["--source", path] => Ok(Mode::Source(PathBuf::from(path))),
        _ => Err(format!("unknown arguments: {}", args.join(" "))),
    }
}

/// [`parse`] for the raw arguments. Empty ones are dropped (some launchers
/// pass them); one that is not Unicode is unknown.
fn parse_os(args: impl IntoIterator<Item = OsString>) -> Result<Mode, String> {
    let args = args
        .into_iter()
        .filter(|a| !a.is_empty())
        .map(|a| {
            a.into_string()
                .map_err(|a| format!("unknown argument: {}", a.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    parse(&args)
}

fn fail(text: &str) -> ExitCode {
    ::log::error!("{text}");
    sys::error_box(HWND::default(), text);
    ExitCode::FAILURE
}

/// Log to the tray folder; a failure here only costs the log.
fn start_log() {
    match task::data_dirs() {
        Ok(dir) => log::init(&dir),
        Err(e) => eprintln!("no log: {e:#}"),
    }
}

/// The elevated tray itself.
fn run_tray(source: Option<PathBuf>) -> ExitCode {
    start_log();
    let inst = match instance::Instance::create() {
        Ok(Some(i)) => i,
        Ok(None) => {
            instance::signal_show();
            return ExitCode::SUCCESS;
        }
        Err(e) => return fail(&format!("{e:#}")),
    };
    if let Err(e) = crate::winfs::load_winfsp() {
        return fail(&format!("{e:#}"));
    }
    let sd = match task::session_user_sid().and_then(|sid| SecurityDescriptor::read_only_for(&sid))
    {
        Ok(sd) => sd,
        Err(e) => return fail(&format!("{e:#}")),
    };
    ui::run(inst, source, sd)
}

/// Started from the Start menu, possibly not elevated.
fn start() -> ExitCode {
    if instance::running() {
        instance::signal_show();
        return ExitCode::SUCCESS;
    }
    if is_elevated() {
        return run_tray(None);
    }
    if task::exists() && task::run().is_ok() && instance::wait_running(Duration::from_secs(5)) {
        return ExitCode::SUCCESS;
    }
    match sys::relaunch_elevated() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

pub fn main() -> ExitCode {
    if is_elevated() {
        // SAFETY: no other thread runs yet, so nothing reads the environment
        // while it changes. Windows also serializes environment access itself.
        unsafe { sys::reset_environment() };
    }
    crate::winfs::harden_dll_search();
    let mode = match parse_os(std::env::args_os().skip(1)) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match mode {
        Mode::Start => start(),
        Mode::FromTask => {
            if !is_elevated() {
                return ExitCode::from(EXIT_NOT_ELEVATED);
            }
            if let Err(e) = task::check_install_dir() {
                start_log();
                return fail(&format!("refusing to run elevated: {e:#}"));
            }
            run_tray(None)
        }
        Mode::Source(path) => {
            if !is_elevated() {
                eprintln!("--source needs administrator rights");
                return ExitCode::FAILURE;
            }
            run_tray(Some(path))
        }
        Mode::Quit => match instance::request_quit(Duration::from_secs(15)) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        },
        Mode::InstallTask { logon } => {
            if !is_elevated() {
                eprintln!("install task: needs administrator rights");
                return ExitCode::FAILURE;
            }
            start_log();
            match task::install(logon) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    ::log::error!("install task: {e:#}");
                    eprintln!("install task: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Mode::AddToPath | Mode::RemoveFromPath => {
            let add = mode == Mode::AddToPath;
            let result = std::env::current_exe()
                .map_err(anyhow::Error::from)
                .and_then(|exe| {
                    if add {
                        path::add(&exe)
                    } else {
                        path::remove(&exe)
                    }
                });
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("PATH: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Mode::RemoveTask => match task::remove() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Mode, String> {
        parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn arguments() {
        assert_eq!(p(&[]), Ok(Mode::Start));
        assert_eq!(p(&["--from-task"]), Ok(Mode::FromTask));
        assert_eq!(p(&["--quit"]), Ok(Mode::Quit));
        assert_eq!(
            p(&["--install-task", "--logon"]),
            Ok(Mode::InstallTask { logon: true })
        );
        assert_eq!(
            p(&["--source", "x.img"]),
            Ok(Mode::Source(PathBuf::from("x.img")))
        );
        // The task-started tray takes nothing else.
        assert!(p(&["--from-task", "--source", "x.img"]).is_err());
        assert!(p(&["--source"]).is_err());
    }

    #[test]
    fn raw_arguments() {
        use std::os::windows::ffi::OsStringExt;
        let os = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(parse_os(os(&["", "--quit", ""])), Ok(Mode::Quit));
        assert_eq!(parse_os(os(&[])), Ok(Mode::Start));
        // An unpaired surrogate.
        let bad = OsString::from_wide(&[u16::from(b'x'), 0xd800]);
        assert!(parse_os([bad]).is_err());
    }
}
