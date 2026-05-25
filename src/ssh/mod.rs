use anyhow::{Result, anyhow};
use std::process::Command;

pub fn exec_ssh(user: &str, domain: &str, command: &str) -> Result<()> {
    // ssh -o LogLevel=QUIET -o StrictHostKeyChecking=accept-new -tA "$USER"@"$DOMAIN" -- "$COMMAND"
    let status = Command::new("ssh")
        .arg("-o")
        .arg("LogLevel=QUIET")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-tA")
        .arg(format!("{}@{}", user, domain))
        .arg("--")
        .arg(command)
        .status()?;

    if !status.success() {
        return Err(anyhow!("SSH command failed with status {}", status));
    }

    Ok(())
}

pub fn copy_ssh(user: &str, domain: &str, src: &std::path::Path, dest: &str) -> Result<()> {
    let status = Command::new("scp")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-A")
        .arg(src)
        .arg(format!("{}@{}:{}", user, domain, dest))
        .status()?;

    if !status.success() {
        return Err(anyhow!("SCP copy failed with status {}", status));
    }

    Ok(())
}
