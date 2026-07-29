mod common;

use anyhow::Result;
use common::TestRepo;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn create_fake_bin(dir: &Path, name: &str, log_file: &Path) -> Result<()> {
    let bin_path = dir.join(name);
    // Use full path to bash to avoid PATH issues in the script itself
    // Added logic to fail if FAIL_SSH or FAIL_SCP is set
    let content = format!(
        r#"#!/bin/bash
echo "{} $@ (FAIL_SSH=$FAIL_SSH)" >> "{}"
if [ "{}" = "ssh" ] && [ -n "$FAIL_SSH" ]; then
  exit 1
fi
if [ "{}" = "scp" ] && [ -n "$FAIL_SCP" ]; then
  exit 1
fi
exit 0
"#,
        name,
        log_file.to_string_lossy(),
        name,
        name
    );
    fs::write(&bin_path, content)?;
    let mut perms = fs::metadata(&bin_path)
        .map_err(|e| {
            eprintln!("Failed to get metadata for {:?}: {}", bin_path, e);
            e
        })?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&bin_path, perms)?;
    Ok(())
}

#[tokio::test]
async fn test_successful_deploy() -> Result<()> {
    let repo = TestRepo::new("deploy-success")?;
    repo.setup_mezzio_project()?;

    // Create a release tag
    TestRepo::git_run(&repo.root.join("htdocs"), &["tag", "v1.0.0"])?;
    TestRepo::git_run(&repo.root.join("htdocs"), &["push", "origin", "v1.0.0"])?;

    // Setup deploy config
    repo.write_file(
        ".deploy.json",
        r#"{
        "version": "1.0",
        "environments": {
            "prod": {
                "user": "deploy-user",
                "domain": "example.com",
                "serviceRoot": "/var/www/my-app"
            }
        }
    }"#,
    )?;

    // Create fake bin directory
    let bin_dir = repo.root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let log_file = repo.root.join("ssh_commands.log");
    create_fake_bin(&bin_dir, "ssh", &log_file)?;
    create_fake_bin(&bin_dir, "scp", &log_file)?;
    create_fake_bin(&bin_dir, "7z", &log_file)?;
    create_fake_bin(&bin_dir, "docker", &log_file)?;

    // Run the binary directly to ensure environment variables are passed to the plugin process
    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_docker-control"));

    println!("Spawning {:?} with --release and --yes", bin_path);
    let child = Command::new(&bin_path)
        .arg("deploy")
        .arg("prod")
        .arg("--release")
        .arg("v1.0.0")
        .arg("--yes")
        .current_dir(&repo.root)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.to_string_lossy(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("DOCKER_CONTROL_SKIP_SSH_AGENT", "1")
        .env("DOCKER_CONTROL_SKIP_DEPENDENCY_CHECK", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            eprintln!(
                "Failed to spawn {:?}: {}. PATH={}",
                bin_path,
                e,
                std::env::var("PATH").unwrap_or_default()
            );
            e
        })?;

    println!("Waiting for cargo to finish...");
    let output = child.wait_with_output().map_err(|e| {
        eprintln!("Failed to wait for output: {}", e);
        e
    })?;

    println!("Stdout: {}", String::from_utf8_lossy(&output.stdout));
    println!("Stderr: {}", String::from_utf8_lossy(&output.stderr));

    assert!(
        output.status.success(),
        "Deploy command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify SSH commands were called
    println!("Checking log file: {:?}", log_file);
    let log_content = fs::read_to_string(&log_file).map_err(|e| {
        eprintln!("Failed to read log file {:?}: {}", log_file, e);
        e
    })?;
    println!("Log content: \n{}", log_content);

    assert!(log_content.contains("ssh -o LogLevel=QUIET -o StrictHostKeyChecking=accept-new -tA deploy-user@example.com -- mkdir -p '/var/www/my-app/releases'"));
    assert!(log_content.contains("scp -o StrictHostKeyChecking=accept-new -A"));
    assert!(log_content.contains("v1.0.0.7z deploy-user@example.com:/var/www/my-app/releases/"));
    assert!(log_content.contains("ssh -o LogLevel=QUIET -o StrictHostKeyChecking=accept-new -tA deploy-user@example.com -- mkdir -p '/var/www/my-app/releases/"));
    assert!(log_content.contains("7z x -o'/var/www/my-app/releases/"));
    assert!(log_content.contains("rm -f '/var/www/my-app/releases/"));
    assert!(log_content.contains("shared:maintenance hard"));
    assert!(log_content.contains("migrations:migrate --no-interaction"));

    Ok(())
}

#[tokio::test]
async fn test_failed_deploy_ssh_error() -> Result<()> {
    let repo = TestRepo::new("deploy-fail")?;
    repo.setup_mezzio_project()?;

    // Create a release tag
    TestRepo::git_run(&repo.root.join("htdocs"), &["tag", "v1.0.0"])?;
    TestRepo::git_run(&repo.root.join("htdocs"), &["push", "origin", "v1.0.0"])?;

    // Setup deploy config
    repo.write_file(
        ".deploy.json",
        r#"{
        "version": "1.0",
        "environments": {
            "prod": {
                "user": "deploy-user",
                "domain": "example.com",
                "serviceRoot": "/var/www/my-app"
            }
        }
    }"#,
    )?;

    // Create fake bin directory
    let bin_dir = repo.root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let log_file = repo.root.join("ssh_commands.log");
    create_fake_bin(&bin_dir, "ssh", &log_file)?;
    create_fake_bin(&bin_dir, "scp", &log_file)?;
    create_fake_bin(&bin_dir, "7z", &log_file)?;
    create_fake_bin(&bin_dir, "docker", &log_file)?;

    // Run the binary directly to ensure environment variables are passed to the plugin process
    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_docker-control"));

    println!(
        "Spawning {:?} (expecting failure) with --release and --yes",
        bin_path
    );
    let child = Command::new(&bin_path)
        .arg("deploy")
        .arg("prod")
        .arg("--release")
        .arg("v1.0.0")
        .arg("--yes")
        .current_dir(&repo.root)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.to_string_lossy(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("DOCKER_CONTROL_SKIP_SSH_AGENT", "1")
        .env("DOCKER_CONTROL_SKIP_DEPENDENCY_CHECK", "1")
        .env("FAIL_SSH", "1") // Trigger SSH failure
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let output = child.wait_with_output()?;

    // Verify SSH commands were called
    let log_content = fs::read_to_string(&log_file).unwrap_or_default();
    println!("Log content (failure test): \n{}", log_content);

    // Deploy command should fail
    assert!(
        !output.status.success(),
        "Deploy command should have failed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error: SSH command failed")
            || stderr.contains("Error: Failed to ensure releases dir exists")
    );

    Ok(())
}

#[tokio::test]
async fn test_failed_deploy_cops_error_non_interactive() -> Result<()> {
    let repo = TestRepo::new("deploy-fail-cops")?;
    repo.setup_mezzio_project()?;

    // Create a release tag
    TestRepo::git_run(&repo.root.join("htdocs"), &["tag", "v1.0.0"])?;
    TestRepo::git_run(&repo.root.join("htdocs"), &["push", "origin", "v1.0.0"])?;

    // Setup deploy config with COPS enabled
    repo.write_file(
        ".deploy.json",
        r#"{
        "version": "1.0",
        "environments": {
            "prod": {
                "user": "deploy-user",
                "domain": "example.com",
                "serviceRoot": "/var/www/my-app",
                "cops_integration": true
            }
        }
    }"#,
    )?;

    // Create fake bin directory
    let bin_dir = repo.root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let log_file = repo.root.join("ssh_commands.log");
    create_fake_bin(&bin_dir, "ssh", &log_file)?;
    create_fake_bin(&bin_dir, "scp", &log_file)?;
    create_fake_bin(&bin_dir, "7z", &log_file)?;
    create_fake_bin(&bin_dir, "docker", &log_file)?;

    // Run the binary directly
    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_docker-control"));

    println!(
        "Spawning {:?} (expecting failure due to COPS) with --release and --yes",
        bin_path
    );
    let child = Command::new(&bin_path)
        .arg("deploy")
        .arg("prod")
        .arg("--release")
        .arg("v1.0.0")
        .arg("--yes")
        .current_dir(&repo.root)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.to_string_lossy(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("DOCKER_CONTROL_SKIP_SSH_AGENT", "1")
        .env("DOCKER_CONTROL_SKIP_DEPENDENCY_CHECK", "1")
        .env("FAIL_SSH", "1") // We'll use FAIL_SSH to make the COPS command fail
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let output = child.wait_with_output()?;

    // Verify COPS failure caused abort
    assert!(
        !output.status.success(),
        "Deploy command should have failed due to COPS"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("COPS outdated check failed")
            || stderr.contains("Error: SSH command failed")
    );

    Ok(())
}

#[tokio::test]
async fn test_deploy_hooks() -> Result<()> {
    let repo = TestRepo::new("deploy-hooks")?;
    repo.setup_mezzio_project()?;

    // Create a release tag
    TestRepo::git_run(&repo.root.join("htdocs"), &["tag", "v1.0.0"])?;
    TestRepo::git_run(&repo.root.join("htdocs"), &["push", "origin", "v1.0.0"])?;

    // Setup deploy config
    repo.write_file(
        ".deploy.json",
        r#"{
        "version": "1.0",
        "environments": {
            "prod": {
                "user": "deploy-user",
                "domain": "example.com",
                "serviceRoot": "/var/www/my-app"
            }
        }
    }"#,
    )?;

    // Create hook script
    let hooks_dir = repo.root.join("deployments/scripts");
    fs::create_dir_all(&hooks_dir)?;
    let hook_content = r#"
fn pre_deploy(console_current, release_dir, console_new, server_root) {
    exec_ssh("custom-pre-hook-command " + release_dir + " " + console_new + " " + server_root);
}
fn post_deploy(console_current, release_dir, console_new, server_root) {
    exec_ssh("custom-post-hook-command " + release_dir + " " + console_new + " " + server_root);
}
fn done_deploy(console_current, release_dir, console_new, server_root) {
    exec_ssh("custom-done-hook-command " + release_dir + " " + console_new + " " + server_root);
}
"#;
    fs::write(hooks_dir.join("prod.rhai"), hook_content)?;

    // Create fake bin directory
    let bin_dir = repo.root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let ssh_log = repo.root.join("ssh_commands.log");
    create_fake_bin(&bin_dir, "ssh", &ssh_log)?;
    create_fake_bin(&bin_dir, "scp", &ssh_log)?;
    create_fake_bin(&bin_dir, "7z", &ssh_log)?;
    create_fake_bin(&bin_dir, "docker", &ssh_log)?;

    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_docker-control"));

    let output = Command::new(&bin_path)
        .arg("deploy")
        .arg("prod")
        .arg("--release")
        .arg("v1.0.0")
        .arg("--yes")
        .current_dir(&repo.root)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.to_string_lossy(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("DOCKER_CONTROL_SKIP_SSH_AGENT", "1")
        .env("DOCKER_CONTROL_SKIP_DEPENDENCY_CHECK", "1")
        .output()?;

    assert!(
        output.status.success(),
        "Deploy command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify SSH commands were called from hook
    let ssh_log_content = fs::read_to_string(&ssh_log)?;
    println!("SSH log content:\n{}", ssh_log_content);

    // Order should be: release_dir console_new server_root
    // release_dir matches timestamp_v1.0.0
    assert!(ssh_log_content.contains("custom-pre-hook-command "));
    assert!(ssh_log_content.contains("custom-post-hook-command "));
    assert!(ssh_log_content.contains("custom-done-hook-command "));

    // It should match something like 20240326153000_v1.0.0 php /var/www/my-app/releases/20240326153000_v1.0.0/bin/console /var/www/my-app
    assert!(ssh_log_content.contains("_v1.0.0 php /var/www/my-app/releases/"));
    assert!(ssh_log_content.contains("/var/www/my-app"));

    Ok(())
}

#[tokio::test]
async fn test_deploy_bugfixes() -> Result<()> {
    let repo = TestRepo::new("deploy-bugfixes")?;
    repo.setup_mezzio_project()?;

    // Create a release tag
    TestRepo::git_run(&repo.root.join("htdocs"), &["tag", "v1.0.0"])?;
    TestRepo::git_run(&repo.root.join("htdocs"), &["push", "origin", "v1.0.0"])?;

    // Setup deploy config with custom console command and shared paths
    repo.write_file(
        ".deploy.json",
        r#"{
        "version": "1.0",
        "environments": {
            "prod": {
                "user": "deploy-user",
                "domain": "example.com",
                "serviceRoot": "/var/www/my-app",
                "console_command": "php bin/console",
                "sharedDirectories": ["uploads"],
                "sharedFiles": [".env.local"]
            }
        }
    }"#,
    )?;

    // Create fake bin directory
    let bin_dir = repo.root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let log_file = repo.root.join("ssh_commands.log");
    create_fake_bin(&bin_dir, "ssh", &log_file)?;
    create_fake_bin(&bin_dir, "scp", &log_file)?;
    create_fake_bin(&bin_dir, "7z", &log_file)?;
    create_fake_bin(&bin_dir, "docker", &log_file)?;

    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_docker-control"));

    let output = Command::new(&bin_path)
        .arg("deploy")
        .arg("prod")
        .arg("--release")
        .arg("v1.0.0")
        .arg("--yes")
        .current_dir(&repo.root)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.to_string_lossy(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("DOCKER_CONTROL_SKIP_SSH_AGENT", "1")
        .env("DOCKER_CONTROL_SKIP_DEPENDENCY_CHECK", "1")
        .output()?;

    assert!(
        output.status.success(),
        "Deploy command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log_content = fs::read_to_string(&log_file)?;
    println!("SSH log content:\n{}", log_content);

    // 1. Check for double php prefix
    // It should NOT contain "php php bin/console"
    assert!(
        !log_content.contains("php php bin/console"),
        "Double php prefix detected"
    );

    // 2. Check console_current command
    // It should NOT contain "public/index.php" if console_command is "php bin/console"
    assert!(
        !log_content.contains("public/index.php"),
        "Incorrect console_current command"
    );
    assert!(
        log_content.contains("bin/console shared:maintenance"),
        "Correct console command not found"
    );

    // 3. Check order of shared paths and symlink update
    let lines: Vec<&str> = log_content.lines().collect();
    let shared_paths_idx = lines
        .iter()
        .position(|l| l.contains("mkdir -p '/var/www/my-app/shared/uploads'"))
        .expect("Shared paths command not found");
    let symlink_update_idx = lines
        .iter()
        .position(|l| l.contains("rm -f '/var/www/my-app/current' && ln -s 'releases/"))
        .expect("Symlink update command not found");

    assert!(
        shared_paths_idx < symlink_update_idx,
        "Shared paths should be handled BEFORE symlink update"
    );

    // 4. Check that shared paths use remote_release_path, not 'current'
    // Before fix, it uses 'current'
    assert!(
        !lines[shared_paths_idx..]
            .iter()
            .any(|l| l.contains("/var/www/my-app/current/uploads")),
        "Shared paths should NOT use 'current' before symlink update"
    );

    Ok(())
}

#[tokio::test]
async fn test_deploy_order_full_verification() -> Result<()> {
    let repo = TestRepo::new("deploy-order")?;
    repo.setup_mezzio_project()?;

    // Create a release tag
    TestRepo::git_run(&repo.root.join("htdocs"), &["tag", "v1.0.0"])?;
    TestRepo::git_run(&repo.root.join("htdocs"), &["push", "origin", "v1.0.0"])?;

    // Setup deploy config with shared paths
    repo.write_file(
        ".deploy.json",
        r#"{
        "version": "1.0",
        "environments": {
            "prod": {
                "user": "deploy-user",
                "domain": "example.com",
                "serviceRoot": "/var/www/my-app",
                "sharedDirectories": ["uploads"]
            }
        }
    }"#,
    )?;

    // Create hooks
    let hooks_dir = repo.root.join("deployments/scripts");
    fs::create_dir_all(&hooks_dir)?;
    let hook_content = r#"
fn pre_deploy(console_current, release_dir, console_new, server_root) {
    exec_ssh("custom-pre-hook-command");
}
fn post_deploy(console_current, release_dir, console_new, server_root) {
    exec_ssh("custom-post-hook-command");
}
"#;
    fs::write(hooks_dir.join("prod.rhai"), hook_content)?;

    // Create fake bin directory
    let bin_dir = repo.root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let log_file = repo.root.join("ssh_commands.log");
    create_fake_bin(&bin_dir, "ssh", &log_file)?;
    create_fake_bin(&bin_dir, "scp", &log_file)?;
    create_fake_bin(&bin_dir, "7z", &log_file)?;
    create_fake_bin(&bin_dir, "docker", &log_file)?;

    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_docker-control"));

    let output = Command::new(&bin_path)
        .arg("deploy")
        .arg("prod")
        .arg("--release")
        .arg("v1.0.0")
        .arg("--yes")
        .current_dir(&repo.root)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.to_string_lossy(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("DOCKER_CONTROL_SKIP_SSH_AGENT", "1")
        .env("DOCKER_CONTROL_SKIP_DEPENDENCY_CHECK", "1")
        .output()?;

    assert!(output.status.success());

    let log_content = fs::read_to_string(&log_file)?;
    let lines: Vec<&str> = log_content.lines().collect();

    let shared_paths_idx = lines
        .iter()
        .position(|l| l.contains("mkdir -p '/var/www/my-app/shared/uploads'"))
        .expect("Shared paths command not found");

    let maintenance_idx = lines
        .iter()
        .position(|l| l.contains("shared:maintenance hard"))
        .expect("Maintenance command not found");

    let pre_hook_idx = lines
        .iter()
        .position(|l| l.contains("custom-pre-hook-command"))
        .expect("Pre-deploy hook command not found");

    let migrations_idx = lines
        .iter()
        .position(|l| l.contains("migrations:migrate"))
        .expect("Migrations command not found");

    let post_hook_idx = lines
        .iter()
        .position(|l| l.contains("custom-post-hook-command"))
        .expect("Post-deploy hook command not found");

    let symlink_update_idx = lines
        .iter()
        .position(|l| l.contains("rm -f '/var/www/my-app/current' && ln -s 'releases/"))
        .expect("Symlink update command not found");

    // Verify order: Shared paths -> Maintenance -> Pre-hook -> Migrations -> Post-hook -> Symlink
    assert!(
        shared_paths_idx < maintenance_idx,
        "Shared paths should be before maintenance"
    );
    assert!(
        maintenance_idx < pre_hook_idx,
        "Maintenance should be before pre-hook"
    );
    assert!(
        pre_hook_idx < migrations_idx,
        "Pre-hook should be before migrations"
    );
    assert!(
        migrations_idx < post_hook_idx,
        "Migrations should be before post-hook"
    );
    assert!(
        post_hook_idx < symlink_update_idx,
        "Post-hook should be before symlink update"
    );

    Ok(())
}
