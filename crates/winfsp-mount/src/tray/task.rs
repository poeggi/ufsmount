// SPDX-License-Identifier: AGPL-3.0-or-later
//! The scheduled task that starts the tray elevated without a UAC prompt, and
//! the tray's folders under ProgramData.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::sys::{run_hidden, session_user, system_dir};
use crate::product::product;
use crate::security::{
    ADMIN_DIR_USERS_READ, ADMIN_ONLY_DIR, account_sid, check_install_path, current_user_sid,
    is_elevated, program_data, remove_dir, secure_dir,
};

/// `<product>-tray`.
pub fn task_name() -> String {
    format!("{}-tray", product().id)
}

const SYSTEM_SID: &str = "S-1-5-18";

/// The argument the task passes. A tray started with it takes no other input.
pub const FROM_TASK: &str = "--from-task";

/// `<ProgramData>\<product>`: only SYSTEM and Administrators.
pub fn base_dir() -> anyhow::Result<PathBuf> {
    Ok(program_data()?.join(product().id))
}

/// Create and secure `<ProgramData>\<product>` (admin-only) and its `tray`
/// folder (Users may read the log). Returns the `tray` folder.
pub fn data_dirs() -> anyhow::Result<PathBuf> {
    let base = base_dir()?;
    secure_dir(&base, ADMIN_ONLY_DIR)?;
    let tray = base.join("tray");
    secure_dir(&tray, ADMIN_DIR_USERS_READ)?;
    Ok(tray)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The task definition. `sid` is the user it runs for; `exe` a literal path.
/// Task Scheduler expands `%VAR%` in the command, so a `%` is refused.
pub fn task_xml(exe: &Path, sid: &str, logon: bool) -> anyhow::Result<String> {
    let exe = exe
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("not a Unicode path: {}", exe.display()))?;
    anyhow::ensure!(!exe.contains('%'), "{exe}: a task command cannot hold %");
    let sid = xml_escape(sid);
    let exe = xml_escape(exe);
    let id = product().id;
    let triggers = if logon {
        format!(
            "    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{sid}</UserId>\n    </LogonTrigger>\n"
        )
    } else {
        String::new()
    };
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts the {id} tray with administrator rights, without a UAC prompt.</Description>
    <SecurityDescriptor>D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;{sid})</SecurityDescriptor>
  </RegistrationInfo>
  <Triggers>
{triggers}  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{FROM_TASK}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    ))
}

fn schtasks(args: &str) -> anyhow::Result<u32> {
    run_hidden(&system_dir().join("schtasks.exe"), args)
}

/// The SID of the user at the desk, or of the process user if there is none.
pub fn session_user_sid() -> anyhow::Result<String> {
    match session_user().map(|n| account_sid(&n)) {
        Some(Ok(sid)) => Ok(sid),
        Some(Err(e)) => {
            log::warn!("session user: {e:#}; using the process user");
            current_user_sid()
        }
        None => current_user_sid(),
    }
}

/// Our exe must lie under Program Files and be writable by administrators
/// only, with every folder on the way: the task runs it elevated without asking.
pub fn check_install_dir() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    check_install_path(&exe)?;
    Ok(exe)
}

/// `"1"` or `"0"`: whether the task starts the tray at sign-in. The tray menu
/// shows it; the setup reads it to keep the choice on an upgrade.
fn sign_in_file() -> anyhow::Result<PathBuf> {
    Ok(secure_base_dir()?.join("start-at-sign-in"))
}

/// `<ProgramData>\<product>` once it is checked or made admin-only. Files in
/// it are read and written only then: any user may create that folder.
pub fn secure_base_dir() -> anyhow::Result<PathBuf> {
    data_dirs()?;
    base_dir()
}

/// Whether the task starts the tray at sign-in. None when not known.
pub fn starts_at_sign_in() -> Option<bool> {
    match std::fs::read(sign_in_file().ok()?).ok()?.as_slice() {
        b"1" => Some(true),
        b"0" => Some(false),
        _ => None,
    }
}

/// Create (or replace) the task for the session user, and the data folders.
pub fn install(logon: bool) -> anyhow::Result<()> {
    anyhow::ensure!(is_elevated(), "needs administrator rights");
    let exe = check_install_dir()?;
    data_dirs()?;
    let sid = session_user_sid()?;
    if sid == SYSTEM_SID {
        // Setup ran as SYSTEM with nobody signed in (deployment tools). A start
        // from the Start menu then asks for elevation instead.
        log::info!("no signed-in user; task {} not created", task_name());
        return Ok(());
    }
    let xml = task_xml(&exe, &sid, logon)?;
    let file = base_dir()?.join("task.xml");
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(xml.encode_utf16().flat_map(|u| u.to_le_bytes()));
    // A fresh file in the admin-only folder: nobody else can hold it open.
    match std::fs::remove_file(&file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)?
        .write_all(&bytes)?;
    let code = schtasks(&format!(
        "/Create /TN {} /XML \"{}\" /F",
        task_name(),
        file.display()
    ));
    let _ = std::fs::remove_file(&file);
    let code = code?;
    anyhow::ensure!(code == 0, "schtasks /Create failed with exit code {code}");
    log::info!(
        "task {} created for {sid} (start at logon: {logon})",
        task_name()
    );
    let file = sign_in_file()?;
    if let Err(e) = std::fs::write(&file, if logon { "1" } else { "0" }) {
        log::warn!("{}: {e}", file.display());
    }
    Ok(())
}

/// Delete the task and `<ProgramData>\<product>`.
pub fn remove() -> anyhow::Result<()> {
    anyhow::ensure!(is_elevated(), "needs administrator rights");
    let code = schtasks(&format!("/Delete /TN {} /F", task_name()))?;
    if code != 0 {
        log::info!("schtasks /Delete: exit code {code} (no task?)");
    }
    remove_dir(&base_dir()?)
}

pub fn exists() -> bool {
    schtasks(&format!("/Query /TN {}", task_name())).is_ok_and(|c| c == 0)
}

pub fn run() -> anyhow::Result<()> {
    let code = schtasks(&format!("/Run /TN {}", task_name()))?;
    anyhow::ensure!(code == 0, "schtasks /Run failed with exit code {code}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_definition() {
        let exe = Path::new(r"C:\Program Files\demo\demo-tray.exe");
        let x = task_xml(exe, "S-1-5-21-1-2-3-1001", false).unwrap();
        assert!(x.contains(r"<Command>C:\Program Files\demo\demo-tray.exe</Command>"));
        assert!(x.contains("Starts the demo tray"));
        assert_eq!(task_name(), "demo-tray");
        assert!(x.contains("<Arguments>--from-task</Arguments>"));
        assert!(x.contains("<UserId>S-1-5-21-1-2-3-1001</UserId>"));
        assert!(x.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(x.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(x.contains("<Priority>4</Priority>"));
        assert!(x.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(x.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(!x.contains("LogonTrigger"));
        assert!(!x.contains('%'));
        assert!(
            task_xml(exe, "S-1-5-21-1-2-3-1001", true)
                .unwrap()
                .contains("<LogonTrigger>")
        );
        assert!(
            task_xml(Path::new("a&b"), "x", false)
                .unwrap()
                .contains("a&amp;b")
        );
        assert!(task_xml(Path::new(r"C:\Program Files\%x%\a.exe"), "x", false).is_err());
    }
}
