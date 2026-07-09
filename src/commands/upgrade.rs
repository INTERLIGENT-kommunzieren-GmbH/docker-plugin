use crate::ui;
use crate::utils::{dependencies::is_brew_eligible, platform, throttle_cache};
use anyhow::{Result, bail};
use std::process::Command;

const TAP_FORMULA: &str = "INTERLIGENT-kommunzieren-GmbH/tap/docker-control";

/// Minimum time between checking Homebrew for a newer docker-control
/// release. Throttled like the container image staleness check, since it's
/// not something worth doing on every single invocation.
const UPDATE_CHECK_INTERVAL: chrono::Duration = chrono::Duration::days(7);

pub fn execute() -> Result<()> {
    ui::info(format!("Upgrading {}...", TAP_FORMULA));
    let status = Command::new("brew")
        .args(["upgrade", TAP_FORMULA])
        .status()?;
    if !status.success() {
        bail!("brew upgrade failed with status {}", status);
    }

    ui::success("docker-control upgraded successfully.");

    Ok(())
}

/// Cache file recording when docker-control itself was last checked for
/// updates, so the check can be throttled to once a week. Lives in the OS
/// config dir since it's local machine state, not tied to any project.
fn update_check_cache_path() -> Option<std::path::PathBuf> {
    let proj_dirs = directories::ProjectDirs::from("com", "interligent", "docker-control")?;
    Some(proj_dirs.config_dir().join("self-update-check.json"))
}

/// Best-effort check, via Homebrew only, of whether a newer docker-control
/// release is available. Returns `None` when it can't be determined (not on
/// a Homebrew-standard platform, Homebrew missing, offline, checked too
/// recently, etc.) — this never blocks or fails the command being run.
pub fn check_outdated() -> Option<bool> {
    if std::env::var("DOCKER_CONTROL_SKIP_SELF_UPDATE_CHECK").is_ok() {
        return None;
    }

    let cache_path = update_check_cache_path();

    // Cheap cache-file check first, before the platform probe below (which
    // can shell out to `docker info`) — no point paying that cost on the
    // ~6 out of 7 invocations where the throttle would skip the check anyway.
    if !throttle_cache::is_due(cache_path.as_deref(), UPDATE_CHECK_INTERVAL) {
        ui::debug("Skipping self-update check (last checked within the past week)".to_string());
        return None;
    }

    if !is_brew_eligible(&platform::detect_platform().platform) {
        return None;
    }

    let output = Command::new("brew")
        .args(["outdated", "--json=v2", TAP_FORMULA])
        .output()
        .ok()?;

    // `brew outdated <formula>` exits non-zero both when the formula IS
    // outdated and on a real error (e.g. unknown formula) — the exit code
    // alone can't tell those apart. On a real error nothing is printed to
    // stdout, so trust the JSON on stdout as the source of truth instead of
    // gating on the exit status.
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let formulae = json.get("formulae")?.as_array()?;

    // Only stamp the cache once we actually got a parseable answer, so a
    // one-off failure (bad JSON, brew missing, etc.) doesn't suppress the
    // real check for a week.
    throttle_cache::record(cache_path.as_deref());

    Some(!formulae.is_empty())
}

/// Abstracts the "upgrade now?" confirmation so tests can inject a fixed
/// answer instead of blocking on a real prompt, matching the
/// `PromptProvider`/`MergePromptProvider` pattern used elsewhere in this
/// codebase for interactive `inquire` prompts.
pub trait UpgradePromptProvider {
    fn confirm_upgrade(&self) -> bool;
}

pub struct InteractiveUpgradePromptProvider;

impl UpgradePromptProvider for InteractiveUpgradePromptProvider {
    fn confirm_upgrade(&self) -> bool {
        inquire::Confirm::new("Upgrade docker-control now?")
            .with_default(true)
            .prompt()
            .unwrap_or(false)
    }
}
