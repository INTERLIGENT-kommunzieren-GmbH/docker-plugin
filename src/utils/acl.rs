use crate::docker;
use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::Command;

/// uid of the `www-data` user inside the `fduarte42/docker-php` images.
const CONTAINER_WWW_DATA_UID: u32 = 33;

/// Grants the current host user rwX access to `htdocs`, including a default
/// ACL so files later created by the container (as root or www-data) stay
/// accessible without needing sudo again.
pub fn apply_host_acl(project_dir: &Path) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    run_sudo_setfacl(project_dir, &format!("u:{}:rwX", uid), false)?;
    run_sudo_setfacl(project_dir, &format!("u:{}:rwX", uid), true)?;
    Ok(())
}

/// Grants `www-data` (uid 33 inside the `php` container) rwX access to
/// `/var/www/html`, including a default ACL so files later created by the
/// host user stay accessible to the container without extra steps.
pub fn apply_container_acl(project_dir: &Path) -> Result<()> {
    let entry = format!("u:{}:rwX", CONTAINER_WWW_DATA_UID);
    run_container_setfacl(project_dir, &entry, false)?;
    run_container_setfacl(project_dir, &entry, true)?;
    Ok(())
}

fn run_sudo_setfacl(project_dir: &Path, entry: &str, default_acl: bool) -> Result<()> {
    let mut cmd = Command::new("sudo");
    cmd.arg("setfacl").arg("-R");
    if default_acl {
        cmd.arg("-d");
    }
    cmd.arg("-m")
        .arg(entry)
        .arg("htdocs")
        .current_dir(project_dir);

    let status = cmd.status().context("Failed to execute sudo setfacl")?;
    if !status.success() {
        return Err(anyhow!("sudo setfacl failed with status {}", status));
    }
    Ok(())
}

fn run_container_setfacl(project_dir: &Path, entry: &str, default_acl: bool) -> Result<()> {
    let mut args = vec!["setfacl", "-R"];
    if default_acl {
        args.push("-d");
    }
    args.push("-m");
    args.push(entry);
    args.push("/var/www/html");

    docker::exec_as_root(project_dir, "php", &args)
}
