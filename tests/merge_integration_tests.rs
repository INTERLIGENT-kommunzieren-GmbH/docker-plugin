mod common;

use anyhow::Result;
use common::TestRepo;
use docker_control::commands::merge::{MergeOptions, MergePromptProvider, execute};
use std::process::Command;

struct MockMergePromptProvider {
    module: Option<String>,
    release_branch: String,
    confirm_cherry_pick: bool,
    conflict_resolution: String,
    confirm_push: bool,
}

impl MergePromptProvider for MockMergePromptProvider {
    fn select_module(&self, _modules: Vec<String>) -> Result<String> {
        Ok(self
            .module
            .clone()
            .unwrap_or_else(|| "Main Project".to_string()))
    }
    fn select_release_branch(&self, _branches: Vec<String>) -> Result<String> {
        Ok(self.release_branch.clone())
    }
    fn confirm_cherry_pick(&self, _count: usize) -> Result<bool> {
        Ok(self.confirm_cherry_pick)
    }
    fn select_conflict_resolution(&self) -> Result<String> {
        Ok(self.conflict_resolution.clone())
    }
    fn confirm_push(&self, _branch: &str) -> Result<bool> {
        Ok(self.confirm_push)
    }
}

#[test]
fn test_successful_merge() -> Result<()> {
    let repo = TestRepo::new("merge_success")?;
    repo.setup_basic_project()?;

    let htdocs = repo.root.join("htdocs");

    // Create release branch and add commits
    TestRepo::git_run(&htdocs, &["checkout", "-b", "1.0.x"])?;
    repo.write_file("htdocs/feature1.txt", "content1")?;
    repo.commit_all("feat: feature 1")?;
    repo.write_file("htdocs/feature2.txt", "content2")?;
    repo.commit_all("feat: feature 2")?;
    TestRepo::git_run(&htdocs, &["push", "origin", "1.0.x"])?;

    // Go back to main
    TestRepo::git_run(&htdocs, &["checkout", "main"])?;

    let options = MergeOptions {
        prompt_provider: Box::new(MockMergePromptProvider {
            module: None,
            release_branch: "1.0.x".to_string(),
            confirm_cherry_pick: true,
            conflict_resolution: "Abort".to_string(),
            confirm_push: true,
        }),
        keep_worktree: true,
    };

    execute(&repo.root, None, options)?;

    // Verify branch was created and pushed to origin
    let remote_branches = repo_list_remote_branches(&htdocs)?;
    assert!(remote_branches.contains(&"origin/1.0.x-merge".to_string()));

    // Verify commits were cherry-picked (should have 2 commits ahead of origin/main on the merge branch)
    // We can't easily check local branch because it's deleted after push in the command logic
    // but we can check the remote origin.

    Ok(())
}

#[test]
fn test_merge_vendor_module() -> Result<()> {
    let repo = TestRepo::new("merge_vendor")?;
    repo.setup_basic_project()?;

    // Setup vendor module
    let temp_parent = repo.root.parent().unwrap();
    let vendor_origin = temp_parent.join("vendor_origin.git");
    std::fs::create_dir_all(&vendor_origin)?;
    TestRepo::git_run(&vendor_origin, &["init", "--bare", "--initial-branch=main"])?;

    let vendor_path = repo.root.join("htdocs/vendor/test/module");
    std::fs::create_dir_all(&vendor_path)?;
    TestRepo::git_run(&vendor_path, &["init", "--initial-branch=main"])?;
    TestRepo::git_run(&vendor_path, &["config", "user.email", "test@example.com"])?;
    TestRepo::git_run(&vendor_path, &["config", "user.name", "Test User"])?;
    TestRepo::git_run(
        &vendor_path,
        &["remote", "add", "origin", &vendor_origin.to_string_lossy()],
    )?;

    std::fs::write(
        vendor_path.join("composer.json"),
        r#"{"name": "test/module", "version": "1.0.0"}"#,
    )?;
    TestRepo::git_run(&vendor_path, &["add", "."])?;
    TestRepo::git_run(&vendor_path, &["commit", "-m", "Initial vendor commit"])?;
    TestRepo::git_run(&vendor_path, &["push", "origin", "main"])?;

    // Create release branch in vendor
    TestRepo::git_run(&vendor_path, &["checkout", "-b", "1.0.x"])?;
    std::fs::write(vendor_path.join("fix.txt"), "fixed")?;
    TestRepo::git_run(&vendor_path, &["add", "."])?;
    TestRepo::git_run(&vendor_path, &["commit", "-m", "fix: vendor fix"])?;
    TestRepo::git_run(&vendor_path, &["push", "origin", "1.0.x"])?;
    TestRepo::git_run(&vendor_path, &["checkout", "main"])?;

    let options = MergeOptions {
        prompt_provider: Box::new(MockMergePromptProvider {
            module: Some("test/module".to_string()),
            release_branch: "1.0.x".to_string(),
            confirm_cherry_pick: true,
            conflict_resolution: "Abort".to_string(),
            confirm_push: true,
        }),
        keep_worktree: true,
    };

    execute(&repo.root, Some("test/module".to_string()), options)?;

    let remote_branches = repo_list_remote_branches(&vendor_path)?;
    assert!(remote_branches.contains(&"origin/1.0.x-merge".to_string()));

    Ok(())
}

fn repo_list_remote_branches(path: &std::path::Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["branch", "-r", "--format=%(refname:short)"])
        .current_dir(path)
        .output()?;

    let branches = String::from_utf8(output.stdout)?
        .lines()
        .map(|s| s.to_string())
        .collect();
    Ok(branches)
}
