use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::Command;

/// Runs `sudo <args>`, returning an error including the exit status on failure.
pub fn run(args: &[&str]) -> Result<()> {
    run_in(None, args)
}

/// Like [`run`], but runs `sudo <args>` with the given working directory.
pub fn run_in(current_dir: Option<&Path>, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("sudo");
    cmd.args(args);
    if let Some(dir) = current_dir {
        cmd.current_dir(dir);
    }

    let status = cmd
        .status()
        .with_context(|| format!("Failed to execute sudo {}", args.join(" ")))?;

    if !status.success() {
        return Err(anyhow!(
            "sudo {} failed with status {}",
            args.join(" "),
            status
        ));
    }

    Ok(())
}
