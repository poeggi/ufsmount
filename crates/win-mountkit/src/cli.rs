// SPDX-License-Identifier: AGPL-3.0-or-later
//! The command line tool. Texts that name the product are set at run time.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::device::{Device, Source};
use crate::names::Charset;
use crate::partition::{self, human_size};
use crate::product::product;
use crate::volume::Volume;
use crate::{copy, launcher, winfs};

fn credits() -> String {
    format!(
        "\
Credits:
  WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
    https://github.com/winfsp/winfsp
{}
  winfsp-rs, by SnowflakePowered and contributors (GPL-3.0)
    https://github.com/SnowflakePowered/winfsp-rs",
        product().credits
    )
}

#[derive(Parser)]
struct Cli {
    /// More log output (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Option<Command>,

    /// Launcher mode: volume prefix (\<class>\PhysicalDriveN[part]).
    #[arg(short = 'u', hide = true)]
    prefix: Option<String>,

    /// Launcher mode: drive letter or empty.
    #[arg(short = 'm', hide = true, requires = "prefix")]
    mountpoint: Option<String>,

    /// Launcher mode: the user who ran `net use` (DOMAIN\name).
    #[arg(short = 'U', hide = true, requires = "prefix")]
    user: Option<String>,
}

#[derive(Args)]
struct Disk {
    /// PhysicalDriveN, CdRomN or an image file.
    source: String,

    /// Partition id as shown by `probe` (s1a, p2, whole, ...).
    #[arg(long)]
    part: Option<String>,

    /// Logical sector size, if the disk reports a different one than it was partitioned with.
    #[arg(long)]
    sector_size: Option<u32>,

    /// How to show names that are not valid UTF-8.
    #[arg(long, default_value = "utf8", value_parser = ["utf8", "latin1"])]
    charset: String,
}

impl Disk {
    fn open(&self) -> anyhow::Result<Volume> {
        let dev = Arc::new(Device::open(
            &Source::parse(&self.source),
            self.sector_size,
        )?);
        let cands = partition::probe(&dev);
        let cand = partition::select(&cands, self.part.as_deref())?;
        log::info!("using {cand}");
        let charset = if self.charset == "latin1" {
            Charset::Latin1
        } else {
            Charset::Utf8
        };
        Volume::open(dev.clone(), cand, charset)
    }
}

#[derive(Subcommand)]
enum Command {
    /// List the partitions and file systems on a disk.
    Probe {
        /// PhysicalDriveN, CdRomN or an image file.
        source: String,
        #[arg(long)]
        sector_size: Option<u32>,
    },
    /// List a directory.
    Ls {
        #[command(flatten)]
        disk: Disk,
        /// Path inside the file system.
        #[arg(default_value = "/")]
        path: String,
    },
    /// Copy a file or a directory tree out of the file system.
    Cp {
        #[command(flatten)]
        disk: Disk,
        /// Path inside the file system.
        from: String,
        /// Destination on Windows.
        to: PathBuf,
    },
    /// Mount the file system read-only until Ctrl+C.
    Mount {
        #[command(flatten)]
        disk: Disk,
        /// Drive letter or an empty directory.
        mountpoint: String,
        /// Security descriptor for all files (default: read for SYSTEM, Administrators and you).
        #[arg(long)]
        sddl: Option<String>,
    },
    /// Register with the WinFsp launcher (admin).
    Register,
    /// Remove the WinFsp launcher registration (admin).
    Unregister,
}

/// The command line definition with the product's names and texts.
fn command() -> clap::Command {
    // clap takes the long version as a `&'static str`.
    static LONG_VERSION: OnceLock<String> = OnceLock::new();
    let p = product();
    let long_version = LONG_VERSION.get_or_init(|| format!("{}\n\n{}", p.version, credits()));
    Cli::command()
        .name(p.id)
        .version(p.version)
        .about(p.description)
        .after_long_help(credits())
        .long_version(long_version.as_str())
        .mut_subcommand("probe", |c| {
            c.about(format!(
                "List the partitions and {} file systems on a disk",
                p.fs
            ))
        })
        .mut_subcommand("register", |c| {
            c.about(format!(
                r"Register with the WinFsp launcher, for `net use {}: \\{}\PhysicalDriveN` (admin)",
                p.first_letter, p.launcher_class
            ))
        })
        .mut_subcommand("mount", |c| {
            c.mut_arg("mountpoint", |a| {
                a.help(format!(
                    "Drive letter ({}:) or an empty directory",
                    p.first_letter
                ))
            })
        })
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    if cli.command.is_some() && cli.prefix.is_some() {
        anyhow::bail!("-u, -m and -U are for launcher mode and take no command");
    }
    let Some(command) = cli.command else {
        if cli.prefix.is_some() {
            launcher::run_service(cli.prefix, cli.mountpoint, cli.user)?;
            return Ok(ExitCode::SUCCESS);
        }
        anyhow::bail!("no command given, see --help");
    };
    match command {
        Command::Probe {
            source,
            sector_size,
        } => {
            let dev = Arc::new(Device::open(&Source::parse(&source), sector_size)?);
            println!(
                "{}: {}, {}-byte sectors",
                dev.name(),
                human_size(dev.len()),
                dev.sector_size()
            );
            for c in partition::probe(&dev) {
                println!("{c}");
            }
        }
        Command::Ls { disk, path } => copy::ls(&disk.open()?, &path)?,
        Command::Cp { disk, from, to } => {
            let vol = disk.open()?;
            let stats = copy::cp(&vol, &from, &to)?;
            println!(
                "{} files ({}), {} directories copied, {} skipped, {} failed",
                stats.files,
                human_size(stats.bytes),
                stats.dirs,
                stats.skipped,
                stats.failed
            );
            if stats.aborted {
                eprintln!("error: the disk went away, copy aborted");
            }
            if stats.failed > 0 || stats.aborted {
                return Ok(ExitCode::from(1));
            }
        }
        Command::Mount {
            disk,
            mountpoint,
            sddl,
        } => {
            let vol = disk.open()?;
            winfs::mount_console(vol, &mountpoint, sddl.as_deref())?;
        }
        Command::Register => launcher::register()?,
        Command::Unregister => launcher::unregister()?,
    }
    Ok(ExitCode::SUCCESS)
}

/// The command line tool. `set_product` runs first.
pub fn main() -> ExitCode {
    winfs::harden_dll_search();
    let cli = Cli::from_arg_matches(&command().get_matches())
        .unwrap_or_else(|e| e.format(&mut command()).exit());
    let level = match cli.verbose {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        2 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    };
    // Not from RUST_LOG: launcher mode and admin shells run elevated.
    env_logger::Builder::new()
        .filter_level(level)
        .format_timestamp_secs()
        .init();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            if e.is::<winfs::DiskGone>() {
                ExitCode::from(winfs::EXIT_DISK_GONE)
            } else {
                ExitCode::from(2)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_name_the_product() {
        command().debug_assert();
        let help = command().render_long_help().to_string();
        assert!(help.contains("Demo"));
        assert!(help.contains("  demo\n"));
        assert!(help.contains("List the partitions and DEMO file systems on a disk"));
        assert!(
            !help.contains("on a disk."),
            "clap drops the period of one-line texts"
        );
        let help = command()
            .find_subcommand_mut("register")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains(r"\\demo\PhysicalDriveN"));
    }
}
