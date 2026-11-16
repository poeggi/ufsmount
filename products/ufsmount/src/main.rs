// SPDX-License-Identifier: AGPL-3.0-or-later
//! The command line tool. See `winfsp_mount::cli`.

fn main() -> std::process::ExitCode {
    ufsmount::init();
    ufsmount::winfsp_mount::cli::main()
}
