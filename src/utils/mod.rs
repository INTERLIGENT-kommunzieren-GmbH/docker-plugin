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

/// The `.idea/vcs.xml` line registering `relative_path` as a git root.
fn vcs_mapping_entry(relative_path: &str) -> String {
    format!(
        "    <mapping directory=\"$PROJECT_DIR$/{}\" vcs=\"Git\" />",
        relative_path
    )
}

const VCS_XML_SKELETON: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
     <project version=\"4\">\n\
     \x20 <component name=\"VcsDirectoryMappings\">\n\
     \x20 </component>\n\
     </project>";

/// Registers `relative_path` (relative to the project root) as an additional git
/// root in `.idea/vcs.xml`, so PhpStorm shows its branches, commits and diffs in
/// the Git tool window instead of ignoring the nested repository.
///
/// No-op when the project has no `.idea` directory, and idempotent. Like
/// [`exclude_from_phpstorm`] this edits the XML textually — the crate has no XML
/// parser and this file is a flat, IDE-generated list.
pub fn register_phpstorm_git_root(project_dir: &Path, relative_path: &str) -> Result<()> {
    let idea_dir = project_dir.join(".idea");
    if !idea_dir.exists() {
        return Ok(());
    }

    let vcs_path = idea_dir.join("vcs.xml");
    let content = if vcs_path.exists() {
        std::fs::read_to_string(&vcs_path).context(format!("Failed to read {:?}", vcs_path))?
    } else {
        VCS_XML_SKELETON.to_string()
    };

    let entry = vcs_mapping_entry(relative_path);
    if content.contains(entry.trim()) {
        return Ok(());
    }

    // A project that has never had a VCS mapping can lack the component entirely.
    let updated = if let Some(component) = content.find("<component name=\"VcsDirectoryMappings\">")
    {
        // Anchor on the closing tag of *that* component, not on the first `</component>`
        // in the file: `vcs.xml` also holds `IssueNavigationConfiguration`, and IDEA
        // writes it first, so a file-wide search puts the mapping in the wrong component
        // — where PhpStorm ignores it and drops it on the next rewrite.
        let close = content[component..]
            .find("</component>")
            .map(|offset| component + offset)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Malformed {:?}: VcsDirectoryMappings has no closing tag",
                    vcs_path
                )
            })?;
        format!("{}{}\n  {}", &content[..close], entry, &content[close..])
    } else {
        content.replacen(
            "</project>",
            &format!(
                "  <component name=\"VcsDirectoryMappings\">\n{}\n  </component>\n</project>",
                entry
            ),
            1,
        )
    };

    std::fs::write(&vcs_path, updated).context(format!("Failed to write {:?}", vcs_path))?;
    Ok(())
}

/// Reverse of [`register_phpstorm_git_root`]. No-op when the mapping is absent.
pub fn unregister_phpstorm_git_root(project_dir: &Path, relative_path: &str) -> Result<()> {
    let vcs_path = project_dir.join(".idea/vcs.xml");
    if !vcs_path.exists() {
        return Ok(());
    }

    let content =
        std::fs::read_to_string(&vcs_path).context(format!("Failed to read {:?}", vcs_path))?;

    let entry = vcs_mapping_entry(relative_path);
    // Match on the trimmed entry so an IDE reflow of the indentation still matches.
    let Some(line) = content
        .lines()
        .find(|line| line.trim() == entry.trim())
        .map(str::to_string)
    else {
        return Ok(());
    };

    let updated = content.replace(&format!("{}\n", line), "");
    std::fs::write(&vcs_path, updated).context(format!("Failed to write {:?}", vcs_path))?;
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
