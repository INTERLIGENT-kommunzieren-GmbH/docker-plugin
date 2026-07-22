use crate::docker;
use crate::ui;
use crate::utils;
use anyhow::{Result, anyhow};
use std::path::Path;

/// Composer/XDG home directories under `/var/www`. `www-data`'s home is
/// `/var/www`, so these are where Composer keeps its config and caches. If they
/// don't exist when the container ACL is applied at start time, `www-data` can
/// end up unable to write them later — the exact failure this command repairs.
const HOME_DIRS: &[&str] = &[
    "/var/www/.composer",
    "/var/www/.config",
    "/var/www/.cache",
    "/var/www/.local",
];

/// Maximum number of offending paths to list before truncating the output.
const MAX_LISTED: usize = 50;

/// Checks that every path under `/var/www` is readable and writable by the
/// container's `www-data` user, and with `fix` repairs any that aren't.
pub fn execute(project_dir: &Path, fix: bool) -> Result<()> {
    if !docker::is_running(project_dir) {
        ui::critical(
            "Project containers are not running. Start the project first with `docker-control start`.",
        );
        std::process::exit(1);
    }

    // Host-side ACL checks/repairs need the host `setfacl`/`getfacl` (no-op on macOS).
    utils::dependencies::require_acl_tools()?;

    if fix {
        // Container side: ensure the Composer/XDG homes exist, then re-apply the ACL.
        ensure_setfacl_available(project_dir)?;
        ui::info("Ensuring Composer/XDG home directories exist...");
        let mut mkdir_args = vec!["mkdir", "-p"];
        mkdir_args.extend_from_slice(HOME_DIRS);
        docker::exec_as_root(project_dir, "php", &mkdir_args)?;
        // Re-applies `setfacl -R` (regular + default), recomputing the ACL mask
        // over the now-existing directories.
        utils::acl::apply_container_acl(project_dir)?;

        // Host side: force a replay of the htdocs ACL, even if it looks set —
        // the recorded state may not actually be in effect.
        if let Err(e) = utils::acl::reapply_host_acl(project_dir) {
            ui::warning(format!(
                "Could not set host ACL permissions on htdocs: {}",
                e
            ));
        }
    }

    let mut healthy = true;

    // Host ACL on htdocs — lets the host user edit files the container creates.
    ui::info("Checking host ACL on htdocs...");
    if utils::acl::host_acl_is_set(project_dir) {
        ui::success("Host ACL on htdocs is applied.");
    } else {
        ui::warning(
            "Host ACL on htdocs is NOT applied; the host user may be unable to edit container-created files.",
        );
        healthy = false;
    }

    // Container access to /var/www for the www-data user.
    ui::info("Checking /var/www access for www-data...");
    let inaccessible = find_inaccessible_paths(project_dir)?;
    if inaccessible.is_empty() {
        ui::success("All files under /var/www are readable/writable by www-data.");
    } else {
        ui::warning(format!(
            "{} path(s) under /var/www are not readable/writable by www-data:",
            inaccessible.len()
        ));
        for path in inaccessible.iter().take(MAX_LISTED) {
            ui::warning(format!("  {}", path));
        }
        if inaccessible.len() > MAX_LISTED {
            ui::warning(format!(
                "  ... and {} more",
                inaccessible.len() - MAX_LISTED
            ));
        }
        healthy = false;
    }

    if healthy {
        return Ok(());
    }

    if fix {
        Err(anyhow!("permission problems remain after --fix"))
    } else {
        ui::info("Run `docker control doctor --fix` to repair.");
        Err(anyhow!("permission problems detected"))
    }
}

/// Lists paths under `/var/www` that the container's `www-data` user cannot read
/// or write.
fn find_inaccessible_paths(project_dir: &Path) -> Result<Vec<String>> {
    let output = docker::exec_as_user_output(
        project_dir,
        "php",
        "www-data",
        &[
            "find",
            "/var/www",
            "(",
            "!",
            "-readable",
            "-o",
            "!",
            "-writable",
            ")",
            "-print",
        ],
    )?;
    Ok(parse_inaccessible_paths(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Verifies that `setfacl` exists in the `php` container before `--fix` tries to
/// use it, so a missing `acl` package produces an actionable message instead of a
/// raw `OCI runtime exec failed: exec: "setfacl": ... not found` error.
fn ensure_setfacl_available(project_dir: &Path) -> Result<()> {
    let output = docker::exec_as_user_output(
        project_dir,
        "php",
        "root",
        &["sh", "-c", "command -v setfacl"],
    )?;
    if output.status.success() && !output.stdout.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "`setfacl` is not available in the php container; the image is missing the `acl` package. \
         Update the image (`docker control pull`) or install `acl` in it, then retry."
    ))
}

/// Parses the `find` output into a list of paths, trimming whitespace and
/// dropping blank lines.
fn parse_inaccessible_paths(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_inaccessible_paths_empty_is_empty() {
        assert!(parse_inaccessible_paths("").is_empty());
        assert!(parse_inaccessible_paths("   \n\n  \n").is_empty());
    }

    #[test]
    fn parse_inaccessible_paths_trims_and_drops_blanks() {
        let out = "/var/www/.config\n  /var/www/.composer  \n\n/var/www/.cache\n";
        assert_eq!(
            parse_inaccessible_paths(out),
            vec![
                "/var/www/.config".to_string(),
                "/var/www/.composer".to_string(),
                "/var/www/.cache".to_string(),
            ]
        );
    }
}
