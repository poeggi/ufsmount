// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::volume::Volume;

pub fn harden_dll_search() {}

pub fn mount_console(_vol: Volume, _mountpoint: &str, _sddl: Option<&str>) -> anyhow::Result<()> {
    anyhow::bail!("mount is not implemented yet")
}
