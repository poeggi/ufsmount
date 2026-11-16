// SPDX-License-Identifier: AGPL-3.0-or-later
//! The command line tool. See `win_mountkit::cli`.

fn main() -> std::process::ExitCode {
    ufsmount::init();
    ufsmount::win_mountkit::cli::main()
}
