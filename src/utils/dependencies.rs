use crate::ui;
use crate::utils::platform::{self, Platform};
use anyhow::{Result, anyhow};
use inquire::Confirm;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::process::Command;

/// Commands whose own `--version`/no-arg invocation may exit non-zero, so
/// presence is checked via `which` instead.
const CHECK_VIA_WHICH: &[&str] = &["scp", "7z", "getfacl"];

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
        // The old `p7zip` formula is gone from homebrew-core; `sevenzip` is
        // the current formula providing the `7z` binary.
        brew_formula: Some("sevenzip"),
    },
    Dependency {
        name: "setfacl",
        command: "setfacl",
        args: &["--version"],
        critical: false,
        description: "Required for granting the host user and the container's www-data user access to htdocs",
        // Linux ACL tooling, no reliable formula, especially on macOS.
        brew_formula: None,
    },
    Dependency {
        name: "getfacl",
        command: "getfacl",
        args: &[],
        critical: false,
        description: "Required for detecting existing ACLs on htdocs before re-applying them",
        brew_formula: None,
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
    if !is_brew_eligible(&platform::detect_platform().platform) {
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

    if !std::io::stdin().is_terminal() {
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
    let status = Command::new("brew").arg("install").args(&formulas).status();
    if !matches!(status, Ok(s) if s.success()) {
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

pub fn check_dependencies() -> Result<()> {
    // ui::debug("Checking external CLI dependencies...");
    let mut missing_critical = Vec::new();
    let mut missing_optional = Vec::new();

    for dep in DEPENDENCIES {
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
        // SSH/SCP are excluded: Homebrew's `openssh` formula is keg-only, so
        // installing it wouldn't reliably satisfy the check.
        assert!(!installable.contains(&"SSH"));
        assert!(!installable.contains(&"SCP"));
        assert!(!installable.contains(&"Docker"));
        assert!(!installable.contains(&"Docker Compose"));
        assert!(!installable.contains(&"Sudo"));
        assert!(!installable.contains(&"setfacl"));
        assert!(!installable.contains(&"getfacl"));
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
}
