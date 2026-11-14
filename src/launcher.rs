// SPDX-License-Identifier: AGPL-3.0-or-later

pub fn run_service(
    _prefix: Option<String>,
    _mountpoint: Option<String>,
    _user: Option<String>,
) -> anyhow::Result<()> {
    anyhow::bail!("launcher mode is not implemented yet")
}

pub fn register() -> anyhow::Result<()> {
    anyhow::bail!("register is not implemented yet")
}

pub fn unregister() -> anyhow::Result<()> {
    anyhow::bail!("unregister is not implemented yet")
}
