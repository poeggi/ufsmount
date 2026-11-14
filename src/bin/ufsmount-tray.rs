// SPDX-License-Identifier: AGPL-3.0-or-later
//! The tray app. See `ufsmount::tray`.
#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    ufsmount::tray::main()
}
