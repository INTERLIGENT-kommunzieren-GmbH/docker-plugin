use crate::ui;
use anyhow::{Result, bail};
use std::process::Command;

/// Official Claude Code installer (native binary). See https://docs.claude.com.
const CLAUDE_INSTALL_SCRIPT_URL: &str = "https://claude.ai/install.sh";

/// Companion codebase-memory-mcp installer, run right after Claude Code so the
/// MCP server is available for use with the freshly installed CLI.
const MEMORY_MCP_INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/DeusData/codebase-memory-mcp/main/install.sh";

/// Installs Claude Code using Anthropic's official install script, then installs
/// codebase-memory-mcp using its install script. Both are `curl -fsSL <url> | bash`
/// one-liners run directly with no confirmation, matching `install-deps` / `upgrade`.
/// Inherits the terminal so each installer's own progress output is shown.
pub fn execute() -> Result<()> {
    run_installer("Claude Code", CLAUDE_INSTALL_SCRIPT_URL)?;
    run_installer("codebase-memory-mcp", MEMORY_MCP_INSTALL_SCRIPT_URL)?;
    enable_memory_mcp_auto_index()?;
    Ok(())
}

/// Enables automatic indexing of new projects in codebase-memory-mcp, run once
/// right after its install so the MCP server is ready to auto-index out of the box.
fn enable_memory_mcp_auto_index() -> Result<()> {
    ui::info("Enabling codebase-memory-mcp auto-indexing of new projects...");

    let status = Command::new("codebase-memory-mcp")
        .args(["config", "set", "auto_index", "true"])
        .status()?;

    if !status.success() {
        bail!(
            "Failed to enable codebase-memory-mcp auto-indexing (status {})",
            status
        );
    }

    ui::success("codebase-memory-mcp auto-indexing enabled.");
    Ok(())
}

/// Runs a `curl -fsSL <url> | bash` installer through a shell (the pipe needs one),
/// streaming its output to the terminal and turning a non-zero exit into an error.
fn run_installer(name: &str, url: &str) -> Result<()> {
    ui::info(format!("Installing {name} via the official installer..."));

    let status = Command::new("bash")
        .arg("-c")
        .arg(format!("curl -fsSL {url} | bash"))
        .status()?;

    if !status.success() {
        bail!("{name} installation failed with status {}", status);
    }

    ui::success(format!("{name} installed successfully."));
    Ok(())
}
