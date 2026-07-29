mod common;

use anyhow::Result;
use common::TestRepo;
use docker_control::commands::release::{PromptProvider, ReleaseOptions, execute};
use std::fs;

struct MockPromptProvider {
    breaking: bool,
    feature: bool,
    patch_branch: Option<String>,
    module: Option<String>,
}

impl PromptProvider for MockPromptProvider {
    fn confirm_breaking_change(&self) -> Result<bool> {
        Ok(self.breaking)
    }
    fn confirm_new_feature(&self) -> Result<bool> {
        Ok(self.feature)
    }
    fn select_patch_branch(&self, _branches: Vec<String>) -> Result<String> {
        Ok(self
            .patch_branch
            .clone()
            .unwrap_or_else(|| "1.0.x".to_string()))
    }
    fn select_module(&self, _modules: Vec<String>) -> Result<String> {
        Ok(self
            .module
            .clone()
            .unwrap_or_else(|| "Main Project".to_string()))
    }
}

#[test]
fn test_initial_release_with_changelog() -> Result<()> {
    let repo = TestRepo::new("initial")?;
    repo.setup_basic_project()?;

    let options = ReleaseOptions {
        prompt_provider: Box::new(MockPromptProvider {
            breaking: false,
            feature: false,
            patch_branch: None,
            module: None,
        }),
        skip_composer: true,
        keep_worktree: true,
    };

    execute(&repo.root, None, options)?;

    // Verify changelog
    let changelog_content = fs::read_to_string(repo.root.join("releases/1.0.x/CHANGELOG.md"))?;
    assert!(changelog_content.contains("## Release 1.0.x"));
    assert!(changelog_content.contains("Initial commit"));

    Ok(())
}

#[test]
fn test_major_release_with_changelog() -> Result<()> {
    let repo = TestRepo::new("major")?;
    repo.setup_basic_project()?;

    // Create initial release branch and push it to origin
    let htdocs = repo.root.join("htdocs");
    TestRepo::git_run(&htdocs, &["branch", "1.0.x"])?;
    TestRepo::git_run(&htdocs, &["push", "origin", "1.0.x"])?;

    // Add a breaking change commit
    repo.write_file("htdocs/breaking.txt", "breaking")?;
    repo.commit_all("feat!: breaking change")?;
    TestRepo::git_run(&htdocs, &["push", "origin", "main"])?;

    let options = ReleaseOptions {
        prompt_provider: Box::new(MockPromptProvider {
            breaking: true,
            feature: false,
            patch_branch: None,
            module: None,
        }),
        skip_composer: true,
        keep_worktree: true,
    };

    execute(&repo.root, None, options)?;

    let changelog_content = fs::read_to_string(repo.root.join("releases/2.0.x/CHANGELOG.md"))?;
    assert!(changelog_content.contains("## Release 2.0.x"));
    assert!(changelog_content.contains("feat!: breaking change"));

    Ok(())
}

#[test]
fn test_minor_release_with_changelog() -> Result<()> {
    let repo = TestRepo::new("minor")?;
    repo.setup_basic_project()?;
    let htdocs = repo.root.join("htdocs");
    TestRepo::git_run(&htdocs, &["branch", "1.0.x"])?;
    TestRepo::git_run(&htdocs, &["push", "origin", "1.0.x"])?;

    // Add a feature commit
    repo.write_file("htdocs/feature.txt", "feature")?;
    repo.commit_all("feat: new feature")?;
    TestRepo::git_run(&htdocs, &["push", "origin", "main"])?;

    let options = ReleaseOptions {
        prompt_provider: Box::new(MockPromptProvider {
            breaking: false,
            feature: true,
            patch_branch: None,
            module: None,
        }),
        skip_composer: true,
        keep_worktree: true,
    };

    execute(&repo.root, None, options)?;

    let changelog_content = fs::read_to_string(repo.root.join("releases/1.1.x/CHANGELOG.md"))?;
    assert!(changelog_content.contains("## Release 1.1.x"));
    assert!(changelog_content.contains("feat: new feature"));

    Ok(())
}

#[test]
fn test_patch_release_with_changelog() -> Result<()> {
    let repo = TestRepo::new("patch")?;
    repo.setup_basic_project()?;

    // Setup existing release branch and a tag
    let htdocs = repo.root.join("htdocs");
    TestRepo::git_run(&htdocs, &["branch", "1.0.x"])?;
    TestRepo::git_run(&htdocs, &["tag", "1.0.0"])?;
    TestRepo::git_run(&htdocs, &["push", "origin", "1.0.x", "--tags"])?;

    // Add a fix commit to the release branch
    TestRepo::git_run(&htdocs, &["checkout", "1.0.x"])?;
    repo.write_file("htdocs/fix.txt", "fix")?;
    repo.commit_all("fix: patch fix")?;
    TestRepo::git_run(&htdocs, &["push", "origin", "1.0.x"])?;
    TestRepo::git_run(&htdocs, &["checkout", "main"])?;

    let options = ReleaseOptions {
        prompt_provider: Box::new(MockPromptProvider {
            breaking: false,
            feature: false,
            patch_branch: Some("1.0.x".to_string()),
            module: None,
        }),
        skip_composer: true,
        keep_worktree: true,
    };

    execute(&repo.root, None, options)?;

    // Patch doesn't create a new directory, it creates a tag on the branch worktree
    let changelog_content = fs::read_to_string(repo.root.join("releases/1.0.x/CHANGELOG.md"))?;
    assert!(changelog_content.contains("## Release 1.0.1"));
    assert!(changelog_content.contains("fix: patch fix"));

    Ok(())
}

#[test]
fn test_vendor_module_release() -> Result<()> {
    let repo = TestRepo::new("vendor")?;
    repo.setup_basic_project()?;

    // Setup a vendor module with its own origin
    let temp_parent = repo.root.parent().unwrap();
    let vendor_origin = temp_parent.join("vendor_origin.git");
    fs::create_dir_all(&vendor_origin)?;
    TestRepo::git_run(&vendor_origin, &["init", "--bare", "--initial-branch=main"])?;

    let vendor_path = repo.root.join("htdocs/vendor/test/module");
    fs::create_dir_all(&vendor_path)?;
    TestRepo::git_run(&vendor_path, &["init", "--initial-branch=main"])?;
    TestRepo::git_run(&vendor_path, &["config", "user.email", "test@example.com"])?;
    TestRepo::git_run(&vendor_path, &["config", "user.name", "Test User"])?;
    TestRepo::git_run(
        &vendor_path,
        &["remote", "add", "origin", &vendor_origin.to_string_lossy()],
    )?;

    fs::write(
        vendor_path.join("composer.json"),
        r#"{"name": "test/module", "version": "1.0.0"}"#,
    )?;
    TestRepo::git_run(&vendor_path, &["add", "."])?;
    TestRepo::git_run(&vendor_path, &["commit", "-m", "Initial vendor commit"])?;
    TestRepo::git_run(&vendor_path, &["push", "origin", "main"])?;

    let options = ReleaseOptions {
        prompt_provider: Box::new(MockPromptProvider {
            breaking: false,
            feature: false,
            patch_branch: None,
            module: Some("test/module".to_string()),
        }),
        skip_composer: true,
        keep_worktree: true,
    };

    execute(&repo.root, Some("test/module".to_string()), options)?;

    // Verify branch was created in vendor repo
    let branches = repo_list_branches(&vendor_path)?;
    assert!(branches.contains(&"1.0.x".to_string()));

    Ok(())
}

fn repo_list_branches(path: &std::path::Path) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(path)
        .output()?;

    let branches = String::from_utf8(output.stdout)?
        .lines()
        .map(|s| s.to_string())
        .collect();
    Ok(branches)
}
