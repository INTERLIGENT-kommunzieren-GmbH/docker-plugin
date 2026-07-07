use crate::ui;
use anyhow::{Result, bail};
use std::process::Command;

const TAP_FORMULA: &str = "INTERLIGENT-kommunzieren-GmbH/tap/docker-control";

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
