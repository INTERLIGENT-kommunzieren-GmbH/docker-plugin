use crate::utils::dependencies;
use anyhow::Result;

/// Installs all Homebrew-installable dependencies, including the optional ones.
pub fn execute() -> Result<()> {
    dependencies::install_brew_dependencies()
}
