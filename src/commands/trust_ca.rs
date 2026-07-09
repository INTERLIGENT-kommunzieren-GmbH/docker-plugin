use crate::docker;
use crate::ui;
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
            install_nss_trust(&ca_path);
        }
        Platform::Wsl => {
            install_linux_system_trust(&ca_path)?;
            install_nss_trust(&ca_path);
            install_windows_trust_from_wsl(&ca_path);
        }
        Platform::Windows => install_windows_trust(&ca_path)?,
        Platform::Unknown => {
            return Err(anyhow!("Unsupported platform for CA trust installation."));
        }
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
    Command::new("which")
        .arg(name)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

/// Best-effort NSS trust store install so Chrome/Chromium (which don't read the
/// system CA bundle on Linux) also trust the CA. Never fails the command.
fn install_nss_trust(ca_path: &Path) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };

    let nssdb = PathBuf::from(home).join(".pki").join("nssdb");
    if !nssdb.exists() {
        ui::debug("No NSS database found at ~/.pki/nssdb, skipping browser trust store import.");
        return;
    }

    if !command_exists("certutil") {
        ui::debug(
            "certutil not found (install libnss3-tools/nss-tools), skipping NSS trust store import.",
        );
        return;
    }

    let db_arg = format!("sql:{}", nssdb.display());

    // Ignore failure: nothing to delete on first run.
    let _ = Command::new("certutil")
        .args(["-d", &db_arg, "-D", "-n", NSS_NICKNAME])
        .status();

    let add_status = Command::new("certutil")
        .args(["-d", &db_arg, "-A", "-t", "C,,", "-n", NSS_NICKNAME, "-i"])
        .arg(ca_path)
        .status();

    match add_status {
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
}
