use anyhow::{Context, Result};
use std::path::Path;

pub mod acl;
pub mod dependencies;
pub mod forwarding;
pub mod platform;
pub mod sudo;
pub mod throttle_cache;

pub fn stop_ssh_agent() -> Result<()> {
    let pid_file = "/tmp/docker-control-ssh-agent.pid";
    let pid_str = std::fs::read_to_string(pid_file)
        .map_err(|_| anyhow::anyhow!("SSH agent daemon is not running (PID file not found)"))?;
    let pid: u32 = pid_str
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid PID in file"))?;
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to kill process {}: {}", pid, e))?;
    if !status.success() {
        // Process not found, but clean up anyway
        crate::ui::warning(format!(
            "Process {} not found (stale PID file), cleaning up.",
            pid
        ));
    }
    std::fs::remove_file(pid_file)
        .map_err(|e| anyhow::anyhow!("Failed to remove PID file {}: {}", pid_file, e))?;
    Ok(())
}

pub fn sanitize_command_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(anyhow::anyhow!(
            "Invalid command name '{}': must be a plain filename with no path separators",
            name
        ));
    }
    Ok(())
}

pub fn is_managed(project_dir: &Path) -> bool {
    project_dir
        .join(".managed-by-docker-control-plugin")
        .exists()
        || project_dir.join(".managed-by-docker-control").exists()
}

pub fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    std::fs::create_dir_all(dst).context(format!("Failed to create directory {:?}", dst))?;

    for entry in std::fs::read_dir(src).context(format!("Failed to read directory {:?}", src))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.join(entry.file_name()))?;
        } else {
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            if dst_path.exists() {
                let src_hash = hash_file(&src_path)?;
                let dst_hash = hash_file(&dst_path)?;

                if src_hash != dst_hash {
                    std::fs::copy(&src_path, &dst_path).context(format!(
                        "Failed to copy file from {:?} to {:?}",
                        src_path, dst_path
                    ))?;
                }
            } else {
                std::fs::copy(&src_path, &dst_path).context(format!(
                    "Failed to copy file from {:?} to {:?}",
                    src_path, dst_path
                ))?;
            }
        }
    }
    Ok(())
}

pub fn exclude_from_phpstorm(project_dir: &Path, folder_name: &str) -> Result<()> {
    let idea_dir = project_dir.join(".idea");
    if !idea_dir.exists() {
        return Ok(());
    }

    let exclude_entry = format!(
        "      <excludeFolder url=\"file://$MODULE_DIR$/{}\" />",
        folder_name
    );

    for entry in std::fs::read_dir(&idea_dir)
        .context(format!("Failed to read .idea directory {:?}", idea_dir))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("iml") {
            continue;
        }

        let content =
            std::fs::read_to_string(&path).context(format!("Failed to read {:?}", path))?;

        if content.contains(&exclude_entry) {
            continue;
        }

        let updated = content.replacen(
            "</content>",
            &format!("{}\n    </content>", exclude_entry),
            1,
        );

        std::fs::write(&path, updated).context(format!("Failed to write {:?}", path))?;
    }

    Ok(())
}

pub fn remove_phpstorm_exclude(project_dir: &Path, folder_name: &str) -> Result<()> {
    let idea_dir = project_dir.join(".idea");
    if !idea_dir.exists() {
        return Ok(());
    }

    let exclude_line = format!(
        "      <excludeFolder url=\"file://$MODULE_DIR$/{}\" />\n",
        folder_name
    );

    for entry in std::fs::read_dir(&idea_dir)
        .context(format!("Failed to read .idea directory {:?}", idea_dir))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("iml") {
            continue;
        }

        let content =
            std::fs::read_to_string(&path).context(format!("Failed to read {:?}", path))?;

        if !content.contains(&exclude_line) {
            continue;
        }

        let updated = content.replace(&exclude_line, "");
        std::fs::write(&path, updated).context(format!("Failed to write {:?}", path))?;
    }

    Ok(())
}

pub fn hash_file(path: impl AsRef<Path>) -> Result<String> {
    Ok(hash_bytes(&std::fs::read(path)?))
}

/// Hex-encoded SHA-256 of `bytes`. Shared with [`hash_file`] so file and
/// in-memory hashes are directly comparable.
pub fn hash_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}
