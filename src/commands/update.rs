use crate::assets::AssetManager;
use crate::docker;
use crate::ui;
use crate::utils;
use anyhow::Result;
use std::fs;
use std::path::Path;
use std::time::SystemTime;
use walkdir::WalkDir;

pub fn execute(project_dir: &Path) -> Result<()> {
    ui::info("Updating project with latest template...");

    let was_running = docker::is_running(project_dir);
    if was_running {
        ui::info("Stopping project...");
        docker::execute_compose(project_dir, &["down"])?;
    }

    let asset_manager = AssetManager::new()?;
    asset_manager.ensure_assets()?;
    let template_dir = asset_manager.get_template_dir();

    // Create backup
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    let backup_name = format!("backup_{}", now);
    let backup_dir = project_dir.join(&backup_name);

    ui::info(format!("Creating backup {}...", backup_name));
    fs::create_dir_all(&backup_dir)?;
    utils::exclude_from_phpstorm(project_dir, &backup_name)?;

    // Backup current files (excluding what bash excludes)
    let backup_excludes = ["backup_*", ".git", "htdocs", "logs", "volumes"];
    copy_recursive(project_dir, &backup_dir, &backup_excludes, true)?;

    ui::info("Applying template changes...");
    // Sync from template to project
    let template_excludes = ["logs", "volumes"];
    copy_recursive(&template_dir, project_dir, &template_excludes, false)?;

    // Merge .gitignore
    let gitignore_dist = project_dir.join(".gitignore-dist");
    let gitignore = project_dir.join(".gitignore");

    if gitignore_dist.exists() {
        let mut content = fs::read_to_string(&gitignore).unwrap_or_default();
        let dist_content = fs::read_to_string(&gitignore_dist)?;
        content.push('\n');
        content.push_str(&dist_content);

        let mut lines: Vec<String> = content
            .lines()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .collect();
        lines.sort();
        lines.dedup();

        fs::write(&gitignore, lines.join("\n"))?;
        fs::remove_file(gitignore_dist)?;
    }

    if was_running {
        ui::info("Restarting project...");
        docker::execute_compose(project_dir, &["up", "-d"])?;
    }

    ui::success("Project updated successfully.");

    Ok(())
}

fn copy_recursive(src: &Path, dst: &Path, excludes: &[&str], is_backup: bool) -> Result<()> {
    for entry in WalkDir::new(src).min_depth(1).max_depth(1) {
        let entry = entry?;
        let path = entry.path();
        let file_name = path.file_name().unwrap().to_string_lossy();

        let mut should_exclude = false;
        for exclude in excludes {
            if exclude.contains('*') {
                let pattern = exclude.replace('*', "");
                if file_name.starts_with(&pattern) {
                    should_exclude = true;
                    break;
                }
            } else if file_name == *exclude {
                should_exclude = true;
                break;
            }
        }

        if should_exclude {
            continue;
        }

        let target = dst.join(&*file_name);
        if path.is_dir() {
            if is_backup {
                // For backup, we copy everything inside
                let mut options = fs_extra::dir::CopyOptions::new();
                options.copy_inside = true;
                options.overwrite = true;
                fs_extra::dir::copy(path, dst, &options)
                    .map_err(|e| anyhow::anyhow!("Failed to copy dir: {}", e))?;
            } else {
                // For template sync, we copy the directory itself
                let mut options = fs_extra::dir::CopyOptions::new();
                options.overwrite = true;
                options.content_only = true;
                fs::create_dir_all(&target)?;
                fs_extra::dir::copy(path, &target, &options)
                    .map_err(|e| anyhow::anyhow!("Failed to sync dir: {}", e))?;
            }
        } else {
            fs::copy(path, target)?;
        }
    }
    Ok(())
}
