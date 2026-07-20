use crate::docker;
use crate::ui;
use crate::utils::dependencies;
use crate::utils::platform::{self, Platform};
use crate::utils::sudo;
use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

const NSS_NICKNAME: &str = "docker-control-ca";
const LINUX_DEBIAN_ANCHOR: &str = "/usr/local/share/ca-certificates/docker-control-ca.crt";
const LINUX_RHEL_ANCHOR: &str = "/etc/pki/ca-trust/source/anchors/docker-control-ca.crt";

/// Candidate CA filenames written by the `proxy-companion` container, newest first.
const CA_FILENAMES: &[&str] = &["nginx-proxy-ca.crt", "ca.crt"];

pub fn execute() -> Result<()> {
    let tls_dir = docker::ingress_tls_dir();
    let ca_path = find_ca_cert(&tls_dir).ok_or_else(|| {
        anyhow!(
            "No CA certificate found in {:?}. Run `docker-control start-ingress` first so the \
             proxy companion can generate one.",
            tls_dir
        )
    })?;

    ui::info(format!("Found CA certificate at {:?}", ca_path));

    let platform_info = platform::detect_platform();
    // `Platform::DockerDesktop` is returned for both a Linux host and a native Windows
    // host running Docker Desktop (see utils::platform::detect_platform); disambiguate
    // using the actual host OS rather than assuming Linux.
    let is_windows_host = std::env::consts::OS == "windows";

    match platform_info.platform {
        Platform::Macos => install_macos_trust(&ca_path)?,
        Platform::DockerDesktop if is_windows_host => install_windows_trust(&ca_path)?,
        Platform::NativeLinux(_) | Platform::DockerDesktop => {
            install_linux_system_trust(&ca_path)?;
        }
        Platform::Wsl => {
            install_linux_system_trust(&ca_path)?;
            install_windows_trust_from_wsl(&ca_path);
        }
        Platform::Windows => install_windows_trust(&ca_path)?,
        Platform::Unknown => {
            return Err(anyhow!("Unsupported platform for CA trust installation."));
        }
    }

    // Browser NSS trust (Chrome/Chromium + Firefox) is done via certutil and Homebrew,
    // neither of which applies on a Windows host, so skip it there. `is_windows_host`
    // also covers the DockerDesktop-on-Windows and native `Platform::Windows` cases.
    if !is_windows_host {
        install_browser_trust(&ca_path)?;
    }

    ui::success("CA certificate trusted. Restart your browser for the change to take effect.");
    Ok(())
}

fn find_ca_cert(tls_dir: &Path) -> Option<PathBuf> {
    CA_FILENAMES
        .iter()
        .map(|name| tls_dir.join(name))
        .find(|path| path.exists())
}

fn install_macos_trust(ca_path: &Path) -> Result<()> {
    ui::info("Installing CA into the System keychain (may prompt for your sudo password)...");
    sudo::run(&[
        "security",
        "add-trusted-cert",
        "-d",
        "-r",
        "trustRoot",
        "-k",
        "/Library/Keychains/System.keychain",
        &ca_path.to_string_lossy(),
    ])
}

fn command_exists(name: &str) -> bool {
    // `.output()` (not `.status()`) so `which`'s "no <name> in ..." stderr is captured
    // and discarded rather than leaking to the console.
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Ensures `certutil` is available for the browser NSS imports. `certutil` is genuinely
/// required to add the CA to the Chrome/Chromium and Firefox trust stores (they keep their
/// own NSS databases and ignore the system CA bundle), so — like the other per-command
/// dependencies — it is treated as mandatory: [`dependencies::require_dependency`] offers a
/// direct Homebrew install of `nss` and errors out if it still isn't available. Only called
/// once a browser NSS store has actually been found, so users without such browsers are
/// never prompted or blocked.
fn ensure_certutil() -> Result<()> {
    dependencies::require_dependency("certutil")
}

/// True if `anchor` already exists and has the same content as `ca_path`, so the sudo
/// copy + trust-store rebuild can be skipped on repeat runs.
fn anchor_already_matches(ca_path: &Path, anchor: &Path) -> bool {
    match (std::fs::read(ca_path), std::fs::read(anchor)) {
        (Ok(ca_bytes), Ok(anchor_bytes)) => ca_bytes == anchor_bytes,
        _ => false,
    }
}

fn install_linux_system_trust(ca_path: &Path) -> Result<()> {
    if command_exists("update-ca-certificates") {
        return install_linux_anchor(
            "update-ca-certificates",
            &["update-ca-certificates"],
            LINUX_DEBIAN_ANCHOR,
            ca_path,
        );
    }

    if command_exists("update-ca-trust") {
        return install_linux_anchor(
            "update-ca-trust",
            &["update-ca-trust", "extract"],
            LINUX_RHEL_ANCHOR,
            ca_path,
        );
    }

    Err(anyhow!(
        "No supported system CA trust tool found (update-ca-certificates / update-ca-trust). \
         Import {:?} into your distribution's trust store manually.",
        ca_path
    ))
}

/// Copies `ca_path` to `anchor` and runs `update_cmd`, both via a single sudo prompt.
/// Skips both steps if `anchor` already has identical content.
fn install_linux_anchor(
    tool_name: &str,
    update_cmd: &[&str],
    anchor: &str,
    ca_path: &Path,
) -> Result<()> {
    if anchor_already_matches(ca_path, Path::new(anchor)) {
        ui::info(format!(
            "CA already installed at {} via {}, skipping.",
            anchor, tool_name
        ));
        return Ok(());
    }

    ui::info(format!(
        "Installing CA via {} (may prompt for your sudo password)...",
        tool_name
    ));

    // Combine the copy and the trust-store rebuild into a single sudo invocation (one
    // password prompt). The two paths are passed as `$1`/`$2` positional shell
    // parameters rather than interpolated into the script text, so they can't be
    // mis-parsed as shell syntax.
    let script = format!("cp \"$1\" \"$2\" && {}", update_cmd.join(" "));
    let ca_str = ca_path.to_string_lossy();
    sudo::run(&["sh", "-c", &script, "--", &ca_str, anchor])
}

/// Idempotent import of the CA into a single NSS DB (`db_arg` is e.g. `sql:<dir>`).
/// Deletes any existing entry under `NSS_NICKNAME` first so repeat runs don't fail,
/// then adds the CA as a trusted root. Returns the status of the add command.
fn nss_import(db_arg: &str, ca_path: &Path) -> std::io::Result<std::process::ExitStatus> {
    // Ignore failure: nothing to delete on first run. `.output()` captures certutil's
    // "could not find certificate" stderr so it doesn't alarm the user on a clean import.
    let _ = Command::new("certutil")
        .args(["-d", db_arg, "-D", "-n", NSS_NICKNAME])
        .output();

    Command::new("certutil")
        .args(["-d", db_arg, "-A", "-t", "C,,", "-n", NSS_NICKNAME, "-i"])
        .arg(ca_path)
        .status()
}

/// The shared Chrome/Chromium NSS database (`~/.pki/nssdb`), if it exists.
fn chromium_nssdb_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let nssdb = PathBuf::from(home).join(".pki").join("nssdb");
    nssdb.exists().then_some(nssdb)
}

/// Candidate Firefox profile-parent directories for the current OS and the common
/// install types (native, Flatpak, Snap). Non-existent paths are filtered later.
fn firefox_profile_parents() -> Vec<PathBuf> {
    let mut parents = Vec::new();

    match std::env::consts::OS {
        "macos" => {
            if let Some(home) = std::env::var_os("HOME") {
                parents.push(
                    PathBuf::from(home)
                        .join("Library")
                        .join("Application Support")
                        .join("Firefox")
                        .join("Profiles"),
                );
            }
        }
        "windows" => {
            if let Some(appdata) = std::env::var_os("APPDATA") {
                parents.push(
                    PathBuf::from(appdata)
                        .join("Mozilla")
                        .join("Firefox")
                        .join("Profiles"),
                );
            }
        }
        _ => {
            // Linux / WSL: native, Flatpak, and Snap install layouts.
            if let Some(home) = std::env::var_os("HOME") {
                let home = PathBuf::from(home);
                parents.push(home.join(".mozilla").join("firefox"));
                parents.push(
                    home.join(".var")
                        .join("app")
                        .join("org.mozilla.firefox")
                        .join(".mozilla")
                        .join("firefox"),
                );
                parents.push(
                    home.join("snap")
                        .join("firefox")
                        .join("common")
                        .join(".mozilla")
                        .join("firefox"),
                );
            }
        }
    }

    parents
}

/// Subdirectories of `parents` that contain a `cert9.db` (an initialised Firefox
/// profile using the modern SQLite NSS database).
fn firefox_profiles_with_db(parents: &[PathBuf]) -> Vec<PathBuf> {
    let mut profiles = Vec::new();

    for parent in parents {
        let Ok(entries) = std::fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("cert9.db").exists() {
                profiles.push(path);
            }
        }
    }

    profiles
}

/// Imports the CA into the browser NSS trust stores that don't read the system CA bundle:
/// the shared Chrome/Chromium database (`~/.pki/nssdb`) and every discoverable Firefox
/// profile. `certutil` is resolved once — and only when there is at least one NSS store to
/// update, so users without these browsers are never prompted. If a store exists, `certutil`
/// is mandatory (an error is returned when it can't be obtained); the individual per-store
/// imports remain best-effort and only warn on failure.
fn install_browser_trust(ca_path: &Path) -> Result<()> {
    let chromium_nssdb = chromium_nssdb_path();
    let firefox_profiles = firefox_profiles_with_db(&firefox_profile_parents());

    if chromium_nssdb.is_none() && firefox_profiles.is_empty() {
        ui::debug(
            "No Chrome/Chromium or Firefox NSS databases found, skipping browser trust import.",
        );
        return Ok(());
    }

    ensure_certutil()?;

    if let Some(nssdb) = chromium_nssdb {
        let db_arg = format!("sql:{}", nssdb.display());
        match nss_import(&db_arg, ca_path) {
            Ok(status) if status.success() => {
                ui::info("CA also imported into the NSS trust store (Chrome/Chromium).");
            }
            Ok(status) => {
                ui::warning(format!("certutil NSS import failed with status {}", status));
            }
            Err(e) => {
                ui::warning(format!("Failed to run certutil for NSS import: {}", e));
            }
        }
    }

    let mut imported = 0usize;
    for profile in &firefox_profiles {
        let db_arg = format!("sql:{}", profile.display());
        match nss_import(&db_arg, ca_path) {
            Ok(status) if status.success() => imported += 1,
            Ok(status) => {
                ui::warning(format!(
                    "certutil Firefox import failed for {:?} with status {}",
                    profile, status
                ));
            }
            Err(e) => {
                ui::warning(format!(
                    "Failed to run certutil for Firefox profile {:?}: {}",
                    profile, e
                ));
            }
        }
    }

    if imported > 0 {
        ui::info(format!(
            "CA also imported into {} Firefox profile(s).",
            imported
        ));
    }

    Ok(())
}

fn install_windows_trust(ca_path: &Path) -> Result<()> {
    ui::info("Installing CA into the current user's Root store...");
    let status = Command::new("certutil")
        .arg("-user")
        .arg("-addstore")
        .arg("-f")
        .arg("Root")
        .arg(ca_path)
        .status()
        .context("Failed to execute certutil")?;

    if !status.success() {
        return Err(anyhow!("certutil failed with status {}", status));
    }

    Ok(())
}

/// Best-effort install into the Windows host's Root store from within WSL, via
/// interop (`certutil.exe`). Never fails the command: WSL interop may be disabled.
fn install_windows_trust_from_wsl(ca_path: &Path) {
    let wslpath_output = match Command::new("wslpath").arg("-w").arg(ca_path).output() {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            ui::warning(format!(
                "wslpath failed to resolve a Windows path for the CA (status {}); skipping Windows trust install.",
                output.status
            ));
            return;
        }
        Err(e) => {
            ui::warning(format!(
                "Could not run wslpath ({}); skipping Windows trust install.",
                e
            ));
            return;
        }
    };

    let win_path = String::from_utf8_lossy(&wslpath_output.stdout)
        .trim()
        .to_string();

    let status = Command::new("certutil.exe")
        .arg("-user")
        .arg("-addstore")
        .arg("-f")
        .arg("Root")
        .arg(&win_path)
        .status();

    match status {
        Ok(status) if status.success() => {
            ui::info("CA also imported into the Windows host's current user Root store.");
        }
        Ok(status) => {
            ui::warning(format!("certutil.exe failed with status {}", status));
        }
        Err(e) => {
            ui::warning(format!(
                "Could not run certutil.exe ({}); is WSL interop enabled?",
                e
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn prefers_new_filename_over_legacy() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("ca.crt"), "legacy").unwrap();
        std::fs::write(dir.path().join("nginx-proxy-ca.crt"), "current").unwrap();

        let found = find_ca_cert(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "nginx-proxy-ca.crt");
    }

    #[test]
    fn falls_back_to_legacy_filename() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("ca.crt"), "legacy").unwrap();

        let found = find_ca_cert(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "ca.crt");
    }

    #[test]
    fn returns_none_when_no_ca_present() {
        let dir = tempdir().unwrap();
        assert!(find_ca_cert(dir.path()).is_none());
    }

    #[test]
    fn anchor_matches_when_content_identical() {
        let dir = tempdir().unwrap();
        let ca = dir.path().join("ca.crt");
        let anchor = dir.path().join("anchor.crt");
        std::fs::write(&ca, "same bytes").unwrap();
        std::fs::write(&anchor, "same bytes").unwrap();
        assert!(anchor_already_matches(&ca, &anchor));
    }

    #[test]
    fn anchor_does_not_match_when_missing_or_different() {
        let dir = tempdir().unwrap();
        let ca = dir.path().join("ca.crt");
        let anchor = dir.path().join("anchor.crt");
        std::fs::write(&ca, "same bytes").unwrap();
        assert!(!anchor_already_matches(&ca, &anchor));

        std::fs::write(&anchor, "different bytes").unwrap();
        assert!(!anchor_already_matches(&ca, &anchor));
    }

    #[test]
    fn firefox_profiles_only_returns_dirs_with_cert9_db() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join(".mozilla").join("firefox");

        let with_db = parent.join("abc.default-release");
        std::fs::create_dir_all(&with_db).unwrap();
        std::fs::write(with_db.join("cert9.db"), "db").unwrap();

        let without_db = parent.join("xyz.dev-edition");
        std::fs::create_dir_all(&without_db).unwrap();

        let profiles = firefox_profiles_with_db(&[parent]);
        assert_eq!(profiles, vec![with_db]);
    }

    #[test]
    fn firefox_profiles_empty_for_missing_or_empty_parents() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(firefox_profiles_with_db(&[missing]).is_empty());

        let empty_parent = dir.path().join("empty");
        std::fs::create_dir_all(&empty_parent).unwrap();
        assert!(firefox_profiles_with_db(&[empty_parent]).is_empty());
    }
}
