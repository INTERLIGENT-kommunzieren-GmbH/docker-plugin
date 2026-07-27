use crate::assets::AssetManager;
use crate::docker;
use crate::ui;
use crate::utils;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::time::SystemTime;
use walkdir::WalkDir;

/// Abstracts the "update the project?" confirmation so tests can inject a fixed
/// answer instead of blocking on a real prompt, matching the
/// `UpgradePromptProvider`/`MergePromptProvider` pattern used elsewhere in this
/// codebase for interactive `inquire` prompts.
pub trait UpdatePromptProvider {
    fn confirm_update(&self) -> bool;
}

pub struct InteractiveUpdatePromptProvider;

impl UpdatePromptProvider for InteractiveUpdatePromptProvider {
    fn confirm_update(&self) -> bool {
        inquire::Confirm::new("Continue updating THIS PROJECT from the template?")
            // Safe default: don't rewrite the project unless the user opts in.
            .with_default(false)
            .prompt()
            .unwrap_or(false)
    }
}

/// Decide whether to proceed with the destructive project update. `yes`
/// bypasses the prompt entirely; otherwise an interactive terminal is warned
/// (naming `upgrade` as the likely intended command) and prompted, while a
/// non-interactive run is refused so scripts/CI can't silently rewrite a
/// project. Pure (no filesystem work) so the decision matrix is unit-testable.
fn confirm_or_abort(
    yes: bool,
    is_interactive: bool,
    prompt: &dyn UpdatePromptProvider,
) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !is_interactive {
        bail!(
            "refusing to rewrite the project non-interactively. \
             Re-run with --yes to confirm. \
             (To upgrade docker-control itself, run `upgrade`.)"
        );
    }
    ui::warning("This rewrites THIS PROJECT's files from the template (a backup is created).");
    ui::info("To upgrade docker-control itself instead, run `upgrade`.");
    Ok(prompt.confirm_update())
}

pub fn execute(project_dir: &Path, yes: bool) -> Result<()> {
    let is_interactive = std::io::stdin().is_terminal();
    if !confirm_or_abort(yes, is_interactive, &InteractiveUpdatePromptProvider)? {
        ui::info("Update cancelled.");
        return Ok(());
    }
    apply_template_update(project_dir)
}

fn apply_template_update(project_dir: &Path) -> Result<()> {
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
    copy_recursive(project_dir, &backup_dir, &backup_excludes)?;

    // Sync from template to project via `sudo rsync`. Containers create files
    // under the project as root/www-data that the host user often can't
    // overwrite, so a plain copy fails with EACCES. `rsync -a` run as root can
    // always overwrite them, and because it preserves the template's (host-user)
    // ownership, refreshed files end up owned by the invoking user rather than
    // root — clearing the permission problem going forward. `logs`/`volumes` are
    // left untouched, and without `--delete` files absent from the template are
    // preserved (matching the previous merge behaviour).
    ui::info("Applying template changes (may prompt for sudo password)...");
    let template_src = format!("{}/", template_dir.display());
    let project_dst = format!("{}/", project_dir.display());
    utils::sudo::run(&[
        "rsync",
        "-a",
        "--exclude=logs",
        "--exclude=volumes",
        &template_src,
        &project_dst,
    ])?;

    // Refresh the Capistrano Dockerfile if this project already uses one (it's opt-in
    // and not part of the template, so it's kept in sync here rather than via
    // copy_recursive). Rebuild the image only when the file actually changed.
    let capistrano_build_dir = project_dir.join("build/capistrano");
    if crate::commands::migrate::write_capistrano_dockerfile(&capistrano_build_dir)? {
        ui::info("Rebuilding Capistrano image (Dockerfile changed)...");
        docker::execute_compose(project_dir, &["build", "capistrano"])?;
    }

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

/// Snapshots the immediate children of `src` into `dst`, skipping names matched
/// by `excludes` (a single trailing `*` acts as a prefix wildcard). Used to back
/// up the project before a template update.
fn copy_recursive(src: &Path, dst: &Path, excludes: &[&str]) -> Result<()> {
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
            let mut options = fs_extra::dir::CopyOptions::new();
            options.copy_inside = true;
            options.overwrite = true;
            fs_extra::dir::copy(path, dst, &options)
                .map_err(|e| anyhow::anyhow!("Failed to copy dir {:?}: {}", path, e))?;
        } else {
            fs::copy(path, &target)
                .with_context(|| format!("Failed to copy file to {:?}", target))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockPrompt(bool);
    impl UpdatePromptProvider for MockPrompt {
        fn confirm_update(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn yes_flag_proceeds_without_consulting_prompt() {
        // A prompt that would say "no" must be ignored when --yes is set,
        // regardless of interactivity.
        assert!(confirm_or_abort(true, true, &MockPrompt(false)).unwrap());
        assert!(confirm_or_abort(true, false, &MockPrompt(false)).unwrap());
    }

    #[test]
    fn non_interactive_without_yes_is_refused() {
        let err = confirm_or_abort(false, false, &MockPrompt(true)).unwrap_err();
        assert!(err.to_string().contains("--yes"));
    }

    #[test]
    fn interactive_follows_prompt_answer() {
        assert!(confirm_or_abort(false, true, &MockPrompt(true)).unwrap());
        assert!(!confirm_or_abort(false, true, &MockPrompt(false)).unwrap());
    }
}
