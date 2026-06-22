use crate::ui;
use anyhow::{Result, anyhow};
use std::process::Command;

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
}

const DEPENDENCIES: &[Dependency] = &[
    Dependency {
        name: "Docker",
        command: "docker",
        args: &["--version"],
        critical: true,
        description: "Required for all container operations",
    },
    Dependency {
        name: "Docker Compose",
        command: "docker",
        args: &["compose", "version"],
        critical: true,
        description: "Required for managing project services",
    },
    Dependency {
        name: "Git",
        command: "git",
        args: &["--version"],
        critical: true,
        description: "Required for release and merge workflows",
    },
    Dependency {
        name: "SSH",
        command: "ssh",
        args: &["-V"],
        critical: true,
        description: "Required for secure remote access",
    },
    Dependency {
        name: "SCP",
        command: "scp",
        args: &[], // scp -? or similar might fail, but just checking if it exists
        critical: true,
        description: "Required for file transfers during deployment",
    },
    Dependency {
        name: "Bash",
        command: "bash",
        args: &["--version"],
        critical: true,
        description: "Required for executing scripts",
    },
    Dependency {
        name: "Sudo",
        command: "sudo",
        args: &["--version"],
        critical: false,
        description: "Required for migration tasks requiring elevated privileges",
    },
    Dependency {
        name: "Rsync",
        command: "rsync",
        args: &["--version"],
        critical: false,
        description: "Required for migration tasks",
    },
    Dependency {
        name: "7-Zip",
        command: "7z",
        args: &[],
        critical: false,
        description: "Required for creating deployment packages",
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

pub fn check_dependencies() -> Result<()> {
    // ui::debug("Checking external CLI dependencies...");
    let mut missing_critical = Vec::new();
    let mut missing_optional = Vec::new();

    for dep in DEPENDENCIES {
        let exists = if dep.command == "scp" || dep.command == "7z" {
            // These might return non-zero for just --version or no args
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
        };

        if !exists {
            if dep.critical {
                missing_critical.push(dep);
            } else {
                missing_optional.push(dep);
            }
        }
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
            Some(v) => check_version("Docker", v, MIN_DOCKER_VERSION).map_err(|e| {
                ui::critical(e.to_string());
                e
            })?,
            None => ui::warning(format!(
                "Could not parse Docker version from: {}",
                out.trim()
            )),
        }
    }

    if let Some(out) = version_output("docker", &["compose", "version"]) {
        match parse_version(&out) {
            Some(v) => check_version("Docker Compose", v, MIN_COMPOSE_VERSION).map_err(|e| {
                ui::critical(e.to_string());
                e
            })?,
            None => ui::warning(format!(
                "Could not parse Docker Compose version from: {}",
                out.trim()
            )),
        }
    }

    // ui::debug("All critical dependencies are present.");
    Ok(())
}
