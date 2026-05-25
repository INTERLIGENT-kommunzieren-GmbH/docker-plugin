use crate::git::{CleanupMode, GitService, WorktreeCleanup};
use crate::ui;
use anyhow::{Result, anyhow};
use inquire::{Confirm, Select};
use std::path::Path;
use std::process::{Command, Stdio};

pub trait MergePromptProvider {
    fn select_module(&self, modules: Vec<String>) -> Result<String>;
    fn select_release_branch(&self, branches: Vec<String>) -> Result<String>;
    fn confirm_cherry_pick(&self, count: usize) -> Result<bool>;
    fn select_conflict_resolution(&self) -> Result<String>;
    fn confirm_push(&self, branch: &str) -> Result<bool>;
}

pub struct InteractiveMergePromptProvider;

impl MergePromptProvider for InteractiveMergePromptProvider {
    fn select_module(&self, modules: Vec<String>) -> Result<String> {
        Ok(Select::new("Select project or vendor module to merge", modules).prompt()?)
    }

    fn select_release_branch(&self, branches: Vec<String>) -> Result<String> {
        Ok(Select::new("Select release branch to merge", branches).prompt()?)
    }

    fn confirm_cherry_pick(&self, count: usize) -> Result<bool> {
        Ok(Confirm::new(&format!(
            "Proceed with cherry-picking these {} commits?",
            count
        ))
        .with_default(true)
        .prompt()?)
    }

    fn select_conflict_resolution(&self) -> Result<String> {
        Ok(Select::new(
            "Conflict resolution",
            vec!["Start merge tool", "I have resolved it", "Abort"],
        )
        .prompt()?
        .to_string())
    }

    fn confirm_push(&self, branch: &str) -> Result<bool> {
        Ok(
            Confirm::new(&format!("Push merge branch {} to remote?", branch))
                .with_default(true)
                .prompt()?,
        )
    }
}

pub struct MergeOptions {
    pub prompt_provider: Box<dyn MergePromptProvider>,
    pub keep_worktree: bool,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            prompt_provider: Box::new(InteractiveMergePromptProvider),
            keep_worktree: false,
        }
    }
}

pub fn execute(project_dir: &Path, module: Option<String>, options: MergeOptions) -> Result<()> {
    let mut selected_module = module;

    // Module selection logic
    if selected_module.is_none() {
        let vendor_modules = GitService::list_vendor_modules(project_dir)?;
        if !vendor_modules.is_empty() {
            let mut opts = vec!["Main Project".to_string()];
            opts.extend(vendor_modules);

            let selection = options.prompt_provider.select_module(opts)?;
            if selection != "Main Project" {
                selected_module = Some(selection);
            }
        }
    }

    let git_path = if let Some(m) = &selected_module {
        project_dir.join("htdocs/vendor").join(m)
    } else {
        project_dir.join("htdocs")
    };

    if !git_path.exists() {
        return Err(anyhow!("Git directory not found: {:?}", git_path));
    }

    if !git_path.join(".git").exists() {
        return Err(anyhow!("{:?} is not a git repository", git_path));
    }

    let git = GitService::open(&git_path)?;
    ui::info(format!("Starting merge workflow for: {:?}", git_path));

    // Phase 1: Pre-flight checks
    ui::info("Performing pre-flight checks...");
    git.fetch_all()?;
    let primary_branch = git.get_primary_branch()?;
    let branches = git.list_release_branches()?;

    if branches.is_empty() {
        return Err(anyhow!("No release branches found to merge."));
    }

    let release_branch = options.prompt_provider.select_release_branch(branches)?;

    // Update branches
    ui::info(format!(
        "Updating branches {} and {}...",
        release_branch, primary_branch
    ));
    git.update_branch(&release_branch)?;
    git.update_branch(&primary_branch)?;

    let merge_branch = format!("{}-merge", release_branch);
    ui::info(format!("Merge branch: {}", merge_branch));

    // Check if merge branch already exists natively
    if git.has_branch(&merge_branch)? {
        return Err(anyhow!(
            "Merge branch '{}' already exists! Please resolve this before proceeding.",
            merge_branch
        ));
    }

    // Set up worktree paths
    let worktree_base = if let Some(m) = &selected_module {
        project_dir.join("releases/vendor").join(m)
    } else {
        project_dir.join("releases")
    };

    let release_wt_path = worktree_base.join(&release_branch);
    let merge_wt_path = worktree_base.join(&merge_branch);

    std::fs::create_dir_all(&worktree_base)?;

    ui::info("Creating separate worktrees for source and merge branches...");
    let mode = if options.keep_worktree {
        CleanupMode::Keep
    } else {
        CleanupMode::Always
    };

    // Release worktree
    git.create_worktree(&release_branch, &release_wt_path, Some(&release_branch))?;
    let _release_cleanup = WorktreeCleanup::new(&git_path, release_wt_path.clone(), mode);

    // Merge worktree (created from primary branch)
    git.create_worktree(
        &merge_branch,
        &merge_wt_path,
        Some(&format!("origin/{}", primary_branch)),
    )?;
    let mut _merge_cleanup = WorktreeCleanup::new(&git_path, merge_wt_path.clone(), mode);

    let merge_wt_git = GitService::open(&merge_wt_path)?;

    // Get commits to cherry-pick
    let commits = git.get_commits_between(&primary_branch, &release_branch)?;

    if commits.is_empty() {
        ui::info("No new commits found to merge.");
        return Ok(());
    }

    ui::info(format!(
        "Found {} commits to cherry-pick into {}:",
        commits.len(),
        merge_branch
    ));
    for (hash, summary) in &commits {
        ui::info(format!("  • {} - {}", &hash[..8], summary));
    }

    if !options.prompt_provider.confirm_cherry_pick(commits.len())? {
        ui::info("Merge cancelled.");
        return Ok(());
    }

    for (hash, summary) in commits {
        ui::info(format!("Cherry-picking {} - {}...", &hash[..8], summary));

        // Cherry-pick natively if possible, but for interactive conflict resolution,
        // using Command on the worktree is safer and easier.
        let status = Command::new("git")
            .arg("-C")
            .arg(&merge_wt_path)
            .arg("cherry-pick")
            .arg(&hash)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;

        if !status.success() {
            ui::critical(format!(
                "Cherry-pick failed for commit {}. Conflict detected.",
                &hash[..8]
            ));

            loop {
                let choice = options.prompt_provider.select_conflict_resolution()?;

                match choice.as_str() {
                    "Start merge tool" => {
                        let _ = Command::new("git")
                            .arg("-C")
                            .arg(&merge_wt_path)
                            .arg("mergetool")
                            .stdin(Stdio::inherit())
                            .stdout(Stdio::inherit())
                            .stderr(Stdio::inherit())
                            .status();
                    }
                    "I have resolved it" => {
                        // Check if conflicts still exist
                        let output = Command::new("git")
                            .arg("-C")
                            .arg(&merge_wt_path)
                            .arg("status")
                            .arg("--porcelain")
                            .output()?;
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        if stdout.contains("UU") || stdout.contains("AA") || stdout.contains("DD") {
                            ui::warning("Conflicts still exist in the following files:");
                            for line in stdout.lines() {
                                if line.starts_with("UU")
                                    || line.starts_with("AA")
                                    || line.starts_with("DD")
                                {
                                    ui::info(format!("  • {}", &line[3..]));
                                }
                            }
                            continue;
                        }

                        // Commit the resolution
                        let continue_status = Command::new("git")
                            .arg("-C")
                            .arg(&merge_wt_path)
                            .arg("cherry-pick")
                            .arg("--continue")
                            .env("GIT_EDITOR", "true")
                            .stdin(Stdio::inherit())
                            .stdout(Stdio::inherit())
                            .stderr(Stdio::inherit())
                            .status()?;

                        if !continue_status.success() {
                            ui::critical("cherry-pick --continue failed. You may still have unresolved conflicts.");
                            continue;
                        }
                        break;
                    }
                    _ => {
                        let _ = Command::new("git")
                            .arg("-C")
                            .arg(&merge_wt_path)
                            .arg("cherry-pick")
                            .arg("--abort")
                            .status();
                        return Err(anyhow!("Merge aborted by user due to conflicts."));
                    }
                }
            }
        }
    }

    ui::success(format!(
        "Successfully prepared merge branch: {}",
        merge_branch
    ));

    if options.prompt_provider.confirm_push(&merge_branch)? {
        ui::info(format!(
            "Pushing merge branch {} to remote repository...",
            merge_branch
        ));

        if let Err(e) = merge_wt_git.push_branch(&merge_branch) {
            ui::critical(format!(
                "Failed to push merge branch to remote repository: {}",
                e
            ));
            ui::info("The local merge branch has been preserved for manual investigation:");
            ui::info(format!("  - Merge branch: {}", merge_branch));
            ui::info(format!("  - Worktree location: {:?}", merge_wt_path));

            // Cancel cleanup for merge worktree so user can investigate
            _merge_cleanup.cancel();
            return Err(anyhow!("Failed to push merge branch to remote."));
        } else {
            ui::success(format!(
                "Successfully pushed merge branch {} to remote repository",
                merge_branch
            ));
            ui::info("=== Merge Request Information ===");
            ui::info(format!(
                "Merge branch '{}' has been created and pushed to remote repository",
                merge_branch
            ));
            ui::info(format!("Source branch: {}", merge_branch));
            ui::info(format!("Target branch: {}", primary_branch));
            ui::info("\nNext steps:");
            ui::info("1. Go to your Git hosting service web interface");
            ui::info(format!(
                "2. Create a merge/pull request from '{}' to '{}'",
                merge_branch, primary_branch
            ));
            ui::info("3. Review the changes and merge when ready");

            // Cleanup on success: worktrees and local merge branch
            // _release_cleanup and merge_cleanup will remove worktrees on drop
            // We also need to remove the local branch from the main repo
            let _ = git.delete_branch(&merge_branch);
        }
    } else {
        ui::info("Push cancelled. Local merge branch preserved:");
        ui::info(format!("  - Merge branch: {}", merge_branch));
        ui::info(format!("  - Worktree location: {:?}", merge_wt_path));
        _merge_cleanup.cancel();
    }

    Ok(())
}
