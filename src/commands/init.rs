use crate::ui;
use crate::utils::is_managed;
use anyhow::{Result, anyhow};
use inquire::{Confirm, Select, Text};
use std::fs;
use std::net::TcpListener;
use std::path::Path;

pub async fn execute(project_dir: &Path) -> Result<()> {
    ui::info("Initializing new project...");

    if project_dir.exists() && fs::read_dir(project_dir)?.next().is_some() {
        if is_managed(project_dir) {
            ui::warning("Directory is already managed by docker-control.");
            return Ok(());
        }

        ui::warning("Directory is not empty. Existing files may be overwritten.");
        if !Confirm::new("Continue initializing in this non-empty directory?")
            .with_default(false)
            .prompt()?
        {
            ui::info("Initialization cancelled.");
            return Ok(());
        }
    }

    // Find template directory. Shared with `update`/`status` so the state this
    // stamps below describes the same template they will compare against.
    let template_dir = crate::template::resolve_dir()?;
    ui::info(format!("Using template from: {:?}", template_dir));

    // Copy template files
    copy_dir_contents(&template_dir, project_dir)?;

    // Record the template this project starts from, so later runs can tell
    // whether the template has actually moved rather than just guessing from
    // the version number.
    crate::template::stamp(project_dir, &template_dir, true)?;

    // Rename .gitignore-dist to .gitignore
    let gitignore_dist = project_dir.join(".gitignore-dist");
    let gitignore = project_dir.join(".gitignore");
    if gitignore_dist.exists() {
        fs::rename(gitignore_dist, gitignore)?;
    }

    // Create htdocs directory
    let htdocs_dir = project_dir.join("htdocs");
    fs::create_dir_all(&htdocs_dir)?;

    if let Err(e) = crate::utils::acl::apply_host_acl(project_dir) {
        ui::warning(format!("Could not set ACL permissions on htdocs: {}", e));
    }

    // Prompts
    let project_name = Text::new("Project name:")
        .with_default(
            &project_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        )
        .prompt()?;

    let sanitized_name = sanitize_name(&project_name);

    let php_versions = vec!["7.4", "7.4-oci", "8.2", "8.2-oci", "8.5", "8.5-oci"];
    let php_version = Select::new("PHP Version", php_versions).prompt()?;

    // Find free port for DB
    let db_port = find_free_port(33060, 33099)?;
    ui::info(format!(
        "Automatically selected DB_HOST_PORT {} as it seems to be free.",
        db_port
    ));

    // Create .env file
    let env_content = format!(
        "BASE_DOMAIN={name}.lvh.me\n\
         ENVIRONMENT=development\n\
         DB_HOST_PORT={port}\n\
         PHP_VERSION={php_version}\n\
         PROJECTNAME={name}\n\
         XDEBUG_IP=host.docker.internal\n\
         IDE_KEY={name}.lvh.me\n",
        name = sanitized_name,
        port = db_port,
        php_version = php_version
    );

    let env_file = project_dir.join(".env");
    fs::write(&env_file, env_content)?;
    ui::success(format!("Created {:?}", env_file));

    // Optional checkout
    let checkout = Confirm::new("Do you want to checkout a project into htdocs folder?")
        .with_default(false)
        .prompt()?;

    if checkout {
        let clone_url = Text::new("Clone URL (SSH recommended):").prompt()?;
        ui::info(format!("Cloning {} into htdocs...", clone_url));

        // Clone through git2 using the shared auth_callbacks, which handle SSH-agent
        // auth and trust-on-first-use host-key verification (prompting for unknown hosts
        // and pinning accepted keys in ~/.ssh/known_hosts).
        let mut fetch_options = git2::FetchOptions::new();
        fetch_options.remote_callbacks(crate::git::GitService::auth_callbacks());

        let mut repo_builder = git2::build::RepoBuilder::new();
        repo_builder.fetch_options(fetch_options);

        match repo_builder.clone(&clone_url, &htdocs_dir) {
            Ok(_) => ui::success("Repository cloned successfully."),
            Err(e) => ui::critical(format!("Failed to clone repository: {}", e)),
        }
    }

    ui::success("Project initialized successfully!");
    Ok(())
}

fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());

        if ty.is_dir() {
            copy_dir_contents(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

fn sanitize_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if "/\\.:,- ".contains(c) { '_' } else { c })
        .collect()
}

fn find_free_port(start: u16, end: u16) -> Result<u16> {
    for port in start..=end {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(anyhow!("No free ports found in range {}-{}", start, end))
}
