// SPDX-License-Identifier: AGPL-3.0-or-later
//! The tray app. See `win_mountkit::tray`.
#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    ufsmount::init();
    ufsmount::win_mountkit::tray::main()
}
