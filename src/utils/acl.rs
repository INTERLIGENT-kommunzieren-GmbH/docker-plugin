use crate::docker;
use crate::ui;
use anyhow::Result;
use std::path::Path;
use std::process::Command;

/// uid of the `www-data` user inside the `fduarte42/docker-php` images.
const CONTAINER_WWW_DATA_UID: u32 = 33;

/// macOS ACL permission set granted to the host user, expressed in the NFSv4
/// vocabulary that `chmod +a` uses. `file_inherit`/`directory_inherit` are the
/// macOS equivalent of a Linux *default* ACL: they make the entry propagate to
/// files and subdirectories created later.
const MACOS_ACL_PERMS: &str = "read,write,execute,file_inherit,directory_inherit";

/// Grants the current host user rwX access to `htdocs`, including inheritance
/// (a Linux *default* ACL, or macOS inherit flags) so files later created by
/// the container (as root or www-data) stay accessible without needing sudo
/// again.
///
/// On Linux this uses `sudo setfacl`. macOS has neither `setfacl` nor a
/// separate default ACL, so `sudo chmod +a` is used with inheritance flags
/// instead.
pub fn apply_host_acl(project_dir: &Path) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    if host_acl_already_set(project_dir, uid) {
        ui::info("Host ACL permissions on htdocs already set, skipping.");
        return Ok(());
    }

    if cfg!(target_os = "macos") {
        ui::info("Setting host ACL permissions on htdocs (may prompt for sudo password)...");
        run_chmod_acl(project_dir, &current_username(uid))?;
    } else {
        ui::info("Setting host ACL permissions on htdocs (may prompt for sudo password)...");
        run_sudo_setfacl(project_dir, &format!("u:{}:rwX", uid), false)?;
        run_sudo_setfacl(project_dir, &format!("u:{}:rwX", uid), true)?;
    }
    Ok(())
}

/// Checks whether `htdocs` already carries the host user's ACL entry (with
/// inheritance), so we can skip re-applying it — and, on Linux, avoid an
/// unnecessary sudo prompt — on subsequent invocations.
fn host_acl_already_set(project_dir: &Path, uid: u32) -> bool {
    if cfg!(target_os = "macos") {
        macos_host_acl_already_set(project_dir, &current_username(uid))
    } else {
        linux_host_acl_already_set(project_dir, uid)
    }
}

/// Checks for both the regular and default `setfacl` entries for `uid`.
fn linux_host_acl_already_set(project_dir: &Path, uid: u32) -> bool {
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

/// macOS equivalent of [`linux_host_acl_already_set`]. Reads the directory's own
/// ACL via `ls -lde` (the macOS stand-in for `getfacl`) and looks for the host
/// user's entry with both inherit flags already present.
fn macos_host_acl_already_set(project_dir: &Path, username: &str) -> bool {
    let output = Command::new("ls")
        .arg("-lde")
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
    let user_prefix = format!("user:{} allow", username);
    // `ls -le` may normalise the permission ordering, so match on the presence
    // of the user's allow entry plus both inherit flags rather than an exact
    // string. Both flags on one entry mean it already covers inheritance.
    text.lines().any(|l| {
        let l = l.trim();
        l.contains(&user_prefix) && l.contains("file_inherit") && l.contains("directory_inherit")
    })
}

/// Resolves the login name for `uid`. macOS `chmod`/`ls` ACL entries use user
/// *names*, not numeric UIDs, so we translate. Falls back to the numeric uid if
/// the name can't be determined (which will simply cause `chmod` to no-op with
/// a warning, matching the graceful degradation the callers already expect).
fn current_username(uid: u32) -> String {
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        return user;
    }
    if let Ok(output) = Command::new("id").arg("-un").output()
        && output.status.success()
    {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    uid.to_string()
}

/// Builds the macOS `chmod +a` ACL entry string granting `username` rwX with
/// inheritance.
fn macos_host_ace(username: &str) -> String {
    format!("user:{} allow {}", username, MACOS_ACL_PERMS)
}

/// macOS equivalent of [`run_sudo_setfacl`]: applies the host user's ACL entry
/// recursively with `sudo chmod +a`. Inheritance is baked into the entry
/// itself, so unlike Linux there is no separate default-ACL pass.
fn run_chmod_acl(project_dir: &Path, username: &str) -> Result<()> {
    let entry = macos_host_ace(username);
    crate::utils::sudo::run_in(Some(project_dir), &["chmod", "-R", "+a", &entry, "htdocs"])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_host_ace_uses_username_and_inherit_flags() {
        let ace = macos_host_ace("tito");
        // macOS ACL entries take a user *name*, not a numeric UID.
        assert_eq!(
            ace,
            "user:tito allow read,write,execute,file_inherit,directory_inherit"
        );
        // Inheritance flags are the macOS stand-in for a Linux default ACL.
        assert!(ace.contains("file_inherit"));
        assert!(ace.contains("directory_inherit"));
    }

    #[test]
    fn current_username_prefers_user_env() {
        // SAFETY: single-threaded test; restore afterwards.
        let prev = std::env::var("USER").ok();
        unsafe { std::env::set_var("USER", "alice") };
        assert_eq!(current_username(0), "alice");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("USER", v),
                None => std::env::remove_var("USER"),
            }
        }
    }
}
