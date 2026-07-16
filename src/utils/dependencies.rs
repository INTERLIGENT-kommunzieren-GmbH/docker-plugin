use crate::ui;
use crate::utils::platform::{self, Platform};
use anyhow::{Result, anyhow};
use inquire::Confirm;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::process::Command;

/// Commands whose own `--version`/no-arg invocation may exit non-zero, so
/// presence is checked via `which` instead.
const CHECK_VIA_WHICH: &[&str] = &["scp", "7z", "getfacl", "certutil"];

// Minimum versions required by the current compose templates.
// Docker 20.10: needed for `host-gateway` in extra_hosts.
// Docker Compose 2.4: needed for top-level `name:` in compose.yml.
const MIN_DOCKER_VERSION: (u32, u32) = (20, 10);
const MIN_COMPOSE_VERSION: (u32, u32) = (2, 4);

pub struct Dependency {
    pub name: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub critical: bool,
    pub description: &'static str,
    /// Homebrew formula that provides this binary, if installing it via
    /// `brew install` is sensible/reliable. `None` means "don't offer".
    pub brew_formula: Option<&'static str>,
}

const DEPENDENCIES: &[Dependency] = &[
    Dependency {
        name: "Docker",
        command: "docker",
        args: &["--version"],
        critical: true,
        description: "Required for all container operations",
        // brew only installs the CLI, not a working daemon/Docker Desktop.
        brew_formula: None,
    },
    Dependency {
        name: "Docker Compose",
        command: "docker",
        args: &["compose", "version"],
        critical: true,
        description: "Required for managing project services",
        brew_formula: None,
    },
    Dependency {
        name: "Git",
        command: "git",
        args: &["--version"],
        critical: true,
        description: "Required for release and merge workflows",
        brew_formula: Some("git"),
    },
    Dependency {
        name: "SSH",
        command: "ssh",
        args: &["-V"],
        critical: true,
        description: "Required for secure remote access",
        // Homebrew's `openssh` formula is keg-only on both macOS and
        // Linuxbrew (it conflicts with the OS-provided ssh/scp), so
        // installing it wouldn't reliably put `ssh` on PATH.
        brew_formula: None,
    },
    Dependency {
        name: "SCP",
        command: "scp",
        args: &[], // scp -? or similar might fail, but just checking if it exists
        critical: true,
        description: "Required for file transfers during deployment",
        // Same keg-only caveat as SSH above.
        brew_formula: None,
    },
    Dependency {
        name: "Bash",
        command: "bash",
        args: &["--version"],
        critical: true,
        description: "Required for executing scripts",
        brew_formula: Some("bash"),
    },
    Dependency {
        name: "Sudo",
        command: "sudo",
        args: &["--version"],
        critical: false,
        description: "Required for migration tasks requiring elevated privileges",
        // System/security binary, not meaningfully brew-installable.
        brew_formula: None,
    },
    Dependency {
        name: "Rsync",
        command: "rsync",
        args: &["--version"],
        critical: false,
        description: "Required for migration tasks",
        brew_formula: Some("rsync"),
    },
    Dependency {
        name: "7-Zip",
        command: "7z",
        args: &[],
        critical: false,
        description: "Required for creating deployment packages",
        // We invoke the `7z` binary, which is provided by `p7zip`. The `sevenzip`
        // formula ships the official upstream build whose binary is named `7zz`,
        // so it would not satisfy the `7z` check.
        brew_formula: Some("p7zip"),
    },
    Dependency {
        name: "setfacl",
        command: "setfacl",
        args: &["--version"],
        critical: false,
        description: "Required for granting the host user and the container's www-data user access to htdocs",
        // Linux-only ACL tooling; macOS falls back to `chmod +a` and skips this
        // check entirely. On Linux the `acl` Homebrew formula provides both
        // `setfacl` and `getfacl` (installers filter this out on macOS).
        brew_formula: Some("acl"),
    },
    Dependency {
        name: "getfacl",
        command: "getfacl",
        args: &[],
        critical: false,
        description: "Required for detecting existing ACLs on htdocs before re-applying them",
        // Provided by the `acl` formula on Linux (installers filter this out on macOS).
        brew_formula: Some("acl"),
    },
    Dependency {
        name: "certutil",
        command: "certutil",
        args: &[], // `certutil` with no args exits non-zero; presence is checked via `which`.
        critical: false,
        description: "Used by `trust-ca` to add the ingress CA to the Chrome/Chromium and \
                      Firefox trust stores",
        // The `nss` formula is not keg-only, so `certutil` lands on PATH.
        brew_formula: Some("nss"),
    },
];

/// Parse the first `MAJOR.MINOR` pair found in a version string like
/// "Docker version 24.0.5, build ced0996" or "Docker Compose version v2.20.3".
fn parse_version(output: &str) -> Option<(u32, u32)> {
    // Strip a leading 'v' if present, then find the first x.y sequence.
    let stripped = output.trim_start_matches('v');
    let start = stripped.find(|c: char| c.is_ascii_digit())?;
    let s = &stripped[start..];
    let mut parts = s.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    Some((major, minor))
}

fn version_output(command: &str, args: &[&str]) -> Option<String> {
    Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn check_version(label: &str, actual: (u32, u32), min: (u32, u32)) -> Result<()> {
    if actual < min {
        return Err(anyhow!(
            "{} {}.{} is below the minimum required version {}.{}. Please upgrade.",
            label,
            actual.0,
            actual.1,
            min.0,
            min.1
        ));
    }
    Ok(())
}

/// Checks whether `dep`'s binary is present on `PATH`.
fn dependency_exists(dep: &Dependency) -> bool {
    if CHECK_VIA_WHICH.contains(&dep.command) {
        // These might return non-zero for just --version or no args.
        Command::new("which")
            .arg(dep.command)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        Command::new(dep.command)
            .args(dep.args)
            .output() // Capture output to avoid it leaking to stdout/stderr
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Whether Homebrew is the expected/standard package manager on this platform.
pub(crate) fn is_brew_eligible(platform: &Platform) -> bool {
    matches!(platform, Platform::Macos | Platform::NativeLinux(_))
}

/// Deduplicates the Homebrew formulas needed to cover `deps`, preserving order.
fn dedup_formulas(deps: &[&Dependency]) -> Vec<&'static str> {
    let mut formulas: Vec<&'static str> = Vec::new();
    for dep in deps {
        if let Some(formula) = dep.brew_formula
            && !formulas.contains(&formula)
        {
            formulas.push(formula);
        }
    }
    formulas
}

/// Whether an interactive Homebrew prompt can be shown right now: Homebrew is the
/// platform's package manager and stdin is a terminal (so unattended/CI runs never
/// block on a prompt).
fn can_prompt_brew(platform: &Platform) -> bool {
    is_brew_eligible(platform) && std::io::stdin().is_terminal()
}

/// Runs `brew install <formulas>` and reports whether it succeeded.
fn run_brew_install(formulas: &[&str]) -> bool {
    matches!(
        Command::new("brew").arg("install").args(formulas).status(),
        Ok(s) if s.success()
    )
}

/// Interactively offers to `brew install <formula>` for a single formula — used when a
/// command discovers at runtime that it needs an optional tool (e.g. `trust-ca` needing
/// `certutil` from `nss`). `platform` is the already-detected platform, so this does not
/// re-run platform detection. Returns `true` only when `brew install` ran and succeeded.
/// No-op returning `false` when Homebrew isn't the platform's package manager, stdin
/// isn't a terminal, or Homebrew isn't installed.
pub fn offer_brew_install(formula: &str, platform: &Platform) -> bool {
    if !can_prompt_brew(platform) || platform::get_brew_prefix().is_none() {
        return false;
    }

    let should_install = Confirm::new(&format!("Install `{}` via Homebrew now?", formula))
        .with_default(true)
        .prompt()
        .unwrap_or(false);

    should_install && run_brew_install(&[formula])
}

/// Offers to `brew install` any missing dependency that has a `brew_formula`,
/// removing newly-installed ones from `missing_critical`/`missing_optional`.
/// No-op on platforms where Homebrew isn't the standard tool, when nothing
/// missing has a formula, when stdin isn't a terminal (so unattended/CI runs
/// never block on a prompt), when Homebrew itself isn't installed, or when
/// the user declines.
fn maybe_offer_brew_install(
    missing_critical: &mut Vec<&'static Dependency>,
    missing_optional: &mut Vec<&'static Dependency>,
) {
    if !can_prompt_brew(&platform::detect_platform().platform) {
        return;
    }

    let installable: Vec<&'static Dependency> = missing_critical
        .iter()
        .chain(missing_optional.iter())
        .copied()
        .filter(|dep| dep.brew_formula.is_some())
        .collect();
    if installable.is_empty() {
        return;
    }

    if platform::get_brew_prefix().is_none() {
        ui::warning(
            "Homebrew not found; install the missing dependencies manually or install Homebrew first.",
        );
        return;
    }

    let names: Vec<&str> = installable.iter().map(|dep| dep.name).collect();
    let should_install = Confirm::new(&format!(
        "Install missing dependencies ({}) via Homebrew now?",
        names.join(", ")
    ))
    .with_default(true)
    .prompt()
    .unwrap_or(false);
    if !should_install {
        return;
    }

    let formulas = dedup_formulas(&installable);
    if !run_brew_install(&formulas) {
        ui::warning(
            "Homebrew install failed; see output above. Falling back to manual install instructions.",
        );
    }

    // Only re-verify the deps we actually attempted to install — the rest
    // (e.g. Docker, Sudo) have no formula and couldn't have changed state.
    let still_missing: HashSet<&str> = installable
        .iter()
        .filter(|dep| !dependency_exists(dep))
        .map(|dep| dep.name)
        .collect();
    missing_critical.retain(|dep| dep.brew_formula.is_none() || still_missing.contains(dep.name));
    missing_optional.retain(|dep| dep.brew_formula.is_none() || still_missing.contains(dep.name));
}

/// Installs every dependency that has a Homebrew formula (critical *and* optional)
/// via a single `brew install`. Invoked by the `install-deps` command.
///
/// `brew install` is idempotent, so already-present formulas are simply skipped by
/// Homebrew. This tool is itself distributed via Homebrew, so `brew` is expected to be
/// present regardless of the detected platform; we still error out (rather than silently
/// no-op'ing) if it can't be found, so the user gets an actionable message.
pub fn install_brew_dependencies() -> Result<()> {
    if platform::get_brew_prefix().is_none() {
        return Err(anyhow!(
            "Homebrew is not installed. Install it from https://brew.sh and try again."
        ));
    }

    // Every dependency that Homebrew can reliably provide, critical and optional alike.
    // SSH/SCP/Docker/etc. carry no `brew_formula` and are filtered out.
    // macOS has no `acl` formula (and doesn't need setfacl/getfacl — it falls back
    // to `chmod +a`), so skip those tools there.
    let skip_acl_tools = cfg!(target_os = "macos");
    let installable: Vec<&'static Dependency> = DEPENDENCIES
        .iter()
        .filter(|dep| dep.brew_formula.is_some())
        .filter(|dep| !(skip_acl_tools && matches!(dep.command, "setfacl" | "getfacl")))
        .collect();

    if installable.is_empty() {
        ui::info("No Homebrew-installable dependencies are defined.");
        return Ok(());
    }

    let formulas = dedup_formulas(&installable);
    ui::info(format!(
        "Installing {} Homebrew formula(s): {}",
        formulas.len(),
        formulas.join(", ")
    ));

    if !run_brew_install(&formulas) {
        return Err(anyhow!(
            "`brew install {}` failed; see the output above.",
            formulas.join(" ")
        ));
    }

    // Report anything Homebrew claimed to install but that still isn't on PATH
    // (e.g. a keg-only formula), so the user isn't left with a false success.
    let still_missing: Vec<&str> = installable
        .iter()
        .filter(|dep| !dependency_exists(dep))
        .map(|dep| dep.name)
        .collect();

    if still_missing.is_empty() {
        ui::success("All Homebrew-installable dependencies are present.");
    } else {
        ui::warning(format!(
            "These dependencies are still missing after installation and may need manual \
             setup: {}",
            still_missing.join(", ")
        ));
    }

    Ok(())
}

pub fn check_dependencies() -> Result<()> {
    // ui::debug("Checking external CLI dependencies...");
    let mut missing_critical = Vec::new();
    let mut missing_optional = Vec::new();

    // macOS has neither `setfacl` nor `getfacl`; the ACL logic falls back to
    // `chmod`/`ls`, which ship with the OS, so don't check for the Linux tools.
    let skip_acl_tools = cfg!(target_os = "macos");
    // `trust-ca` only uses certutil (from `nss`) on macOS/Linux via Homebrew; on Windows
    // there's no such path, and `which` may be absent, so the probe would falsely report
    // certutil missing on every command. Skip it there.
    let skip_certutil = cfg!(target_os = "windows");

    for dep in DEPENDENCIES {
        if skip_acl_tools && matches!(dep.command, "setfacl" | "getfacl") {
            continue;
        }
        if skip_certutil && dep.command == "certutil" {
            continue;
        }
        if !dependency_exists(dep) {
            if dep.critical {
                missing_critical.push(dep);
            } else {
                missing_optional.push(dep);
            }
        }
    }

    if !missing_critical.is_empty() || !missing_optional.is_empty() {
        maybe_offer_brew_install(&mut missing_critical, &mut missing_optional);
    }

    if !missing_optional.is_empty() {
        for dep in missing_optional {
            ui::warning(format!(
                "Optional dependency '{}' ({}) is missing. {}",
                dep.name, dep.command, dep.description
            ));
        }
    }

    if !missing_critical.is_empty() {
        for dep in &missing_critical {
            ui::critical(format!(
                "Critical dependency '{}' ({}) is missing! {}",
                dep.name, dep.command, dep.description
            ));
        }
        return Err(anyhow!(
            "Missing {} critical dependencies. Please install them and try again.",
            missing_critical.len()
        ));
    }

    // Version checks — only reached if the binaries are present.
    if let Some(out) = version_output("docker", &["--version"]) {
        match parse_version(&out) {
            Some(v) => check_version("Docker", v, MIN_DOCKER_VERSION).inspect_err(|e| {
                ui::critical(e.to_string());
            })?,
            None => ui::warning(format!(
                "Could not parse Docker version from: {}",
                out.trim()
            )),
        }
    }

    if let Some(out) = version_output("docker", &["compose", "version"]) {
        match parse_version(&out) {
            Some(v) => {
                check_version("Docker Compose", v, MIN_COMPOSE_VERSION).inspect_err(|e| {
                    ui::critical(e.to_string());
                })?
            }
            None => ui::warning(format!(
                "Could not parse Docker Compose version from: {}",
                out.trim()
            )),
        }
    }

    // ui::debug("All critical dependencies are present.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_brew_eligible_only_on_macos_and_native_linux() {
        assert!(is_brew_eligible(&Platform::Macos));
        assert!(is_brew_eligible(&Platform::NativeLinux(None)));
        assert!(is_brew_eligible(&Platform::NativeLinux(Some(
            "172.17.0.1".to_string()
        ))));
        assert!(!is_brew_eligible(&Platform::Wsl));
        assert!(!is_brew_eligible(&Platform::Windows));
        assert!(!is_brew_eligible(&Platform::DockerDesktop));
        assert!(!is_brew_eligible(&Platform::Unknown));
    }

    #[test]
    fn brew_formula_filters_to_installable_deps_only() {
        let installable: Vec<&str> = DEPENDENCIES
            .iter()
            .filter(|dep| dep.brew_formula.is_some())
            .map(|dep| dep.name)
            .collect();

        assert!(installable.contains(&"Git"));
        assert!(installable.contains(&"Bash"));
        assert!(installable.contains(&"Rsync"));
        assert!(installable.contains(&"7-Zip"));
        // certutil is installable via the (non-keg-only) `nss` formula.
        assert!(installable.contains(&"certutil"));
        // SSH/SCP are excluded: Homebrew's `openssh` formula is keg-only, so
        // installing it wouldn't reliably satisfy the check.
        assert!(!installable.contains(&"SSH"));
        assert!(!installable.contains(&"SCP"));
        assert!(!installable.contains(&"Docker"));
        assert!(!installable.contains(&"Docker Compose"));
        assert!(!installable.contains(&"Sudo"));
        // setfacl/getfacl are installable on Linux via the `acl` formula (installers
        // filter them out on macOS, where the ACL logic falls back to `chmod +a`).
        assert!(installable.contains(&"setfacl"));
        assert!(installable.contains(&"getfacl"));
    }

    #[test]
    fn seven_zip_uses_p7zip_formula() {
        // We call the `7z` binary, provided by `p7zip`; the `sevenzip` formula's
        // binary is named `7zz` and would not satisfy the check.
        let seven_zip = DEPENDENCIES
            .iter()
            .find(|dep| dep.name == "7-Zip")
            .expect("7-Zip dependency should exist");
        assert_eq!(seven_zip.brew_formula, Some("p7zip"));
    }

    #[test]
    fn acl_tools_share_the_acl_formula() {
        for name in ["setfacl", "getfacl"] {
            let dep = DEPENDENCIES
                .iter()
                .find(|dep| dep.name == name)
                .unwrap_or_else(|| panic!("{name} dependency should exist"));
            assert_eq!(dep.brew_formula, Some("acl"));
        }
    }

    #[test]
    fn dedup_formulas_collapses_duplicate_formula_entries() {
        let a = Dependency {
            name: "A",
            command: "a",
            args: &[],
            critical: true,
            description: "",
            brew_formula: Some("shared"),
        };
        let b = Dependency {
            name: "B",
            command: "b",
            args: &[],
            critical: true,
            description: "",
            brew_formula: Some("shared"),
        };
        let c = Dependency {
            name: "C",
            command: "c",
            args: &[],
            critical: false,
            description: "",
            brew_formula: Some("other"),
        };

        assert_eq!(dedup_formulas(&[&a, &b, &c]), vec!["shared", "other"]);
    }

    #[test]
    fn getfacl_is_checked_via_which() {
        assert!(CHECK_VIA_WHICH.contains(&"getfacl"));
    }

    #[test]
    fn certutil_is_checked_via_which() {
        // `certutil` with no args exits non-zero, so presence must be probed via `which`.
        assert!(CHECK_VIA_WHICH.contains(&"certutil"));
    }

    #[test]
    fn certutil_maps_to_nss_formula() {
        let certutil = DEPENDENCIES
            .iter()
            .find(|d| d.command == "certutil")
            .expect("certutil dependency present");
        assert_eq!(certutil.brew_formula, Some("nss"));
        assert!(!certutil.critical);
    }
}
