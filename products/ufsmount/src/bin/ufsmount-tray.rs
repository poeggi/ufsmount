// SPDX-License-Identifier: AGPL-3.0-or-later
//! The tray app. See `winfsp_mount::tray`.
#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    ufsmount::init();
    ufsmount::winfsp_mount::tray::main()
}
