use crate::docker;
use crate::ui;
use anyhow::Result;
use std::path::Path;
use std::process::Command;

/// uid of the `www-data` user inside the `fduarte42/docker-php` images.
const CONTAINER_WWW_DATA_UID: u32 = 33;

/// Grants the current host user rwX access to `htdocs`, including a default
/// ACL so files later created by the container (as root or www-data) stay
/// accessible without needing sudo again.
pub fn apply_host_acl(project_dir: &Path) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    if host_acl_already_set(project_dir, uid) {
        ui::info("Host ACL permissions on htdocs already set, skipping.");
        return Ok(());
    }
    ui::info("Setting host ACL permissions on htdocs (may prompt for sudo password)...");
    run_sudo_setfacl(project_dir, &format!("u:{}:rwX", uid), false)?;
    run_sudo_setfacl(project_dir, &format!("u:{}:rwX", uid), true)?;
    Ok(())
}

/// Checks whether `htdocs` already has both the regular and default ACL
/// entries for `uid`, so we can skip re-running `sudo setfacl` (and avoid an
/// unnecessary sudo prompt) on subsequent invocations.
fn host_acl_already_set(project_dir: &Path, uid: u32) -> bool {
    let output = Command::new("getfacl")
        .arg("-p")
        .arg("-n")
        .arg("htdocs")
        .current_dir(project_dir)
        .output();

    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let user_entry = format!("user:{}:rwx", uid);
    let default_entry = format!("default:user:{}:rwx", uid);
    text.lines().any(|l| l.trim() == user_entry) && text.lines().any(|l| l.trim() == default_entry)
}

/// Grants `www-data` (uid 33 inside the `php` container) rwX access to
/// `/var/www`, including a default ACL so files later created by the
/// host user stay accessible to the container without extra steps.
pub fn apply_container_acl(project_dir: &Path) -> Result<()> {
    ui::info("Setting container ACL permissions on htdocs...");
    let entry = format!("u:{}:rwX", CONTAINER_WWW_DATA_UID);
    run_container_setfacl(project_dir, &entry, false)?;
    run_container_setfacl(project_dir, &entry, true)?;
    Ok(())
}

fn run_sudo_setfacl(project_dir: &Path, entry: &str, default_acl: bool) -> Result<()> {
    let mut args = vec!["setfacl", "-R"];
    if default_acl {
        args.push("-d");
    }
    args.push("-m");
    args.push(entry);
    args.push("htdocs");

    crate::utils::sudo::run_in(Some(project_dir), &args)
}

fn run_container_setfacl(project_dir: &Path, entry: &str, default_acl: bool) -> Result<()> {
    let mut args = vec!["setfacl", "-R"];
    if default_acl {
        args.push("-d");
    }
    args.push("-m");
    args.push(entry);
    args.push("/var/www");

    docker::exec_as_root(project_dir, "php", &args)
}
