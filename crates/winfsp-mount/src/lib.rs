// SPDX-License-Identifier: AGPL-3.0-or-later
//! Read-only file systems on Windows through WinFsp: disk access, partition
//! tables, the WinFsp file system, the command line and the tray. A product
//! adds its file system code and names ([`product::Product`]).

pub mod cli;
pub mod copy;
pub mod device;
pub mod fs;
pub mod launcher;
pub mod names;
pub mod partition;
pub mod product;
pub mod security;
pub mod tray;
pub mod volume;
pub mod winfs;

pub use product::{Product, product, set_product};
