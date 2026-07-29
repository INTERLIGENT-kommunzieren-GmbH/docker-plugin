use crate::config::DeployConfig;
use crate::git::GitService;
use crate::ui;
use crate::utils;
use anyhow::Result;
use bollard::models::ContainerSummaryStateEnum;
use bollard::query_parameters::ListContainersOptions;
use std::collections::HashMap;
use std::path::Path;

pub async fn execute(project_dir: &Path) -> Result<()> {
    ui::info("Project Status");
    println!("  Project Directory: {}", project_dir.display());

    // 1. Plugin Management
    if utils::is_managed(project_dir) {
        ui::success("  Plugin Management: ✓ Managed by Docker Control Plugin");
    } else {
        ui::warning("  Plugin Management: ✗ Not managed by Docker Control Plugin");
        println!("    Run 'docker control init' to initialize this directory");
    }

    // 2. Git Status
    show_git_status(project_dir);

    // 3. Template Status
    show_template_status(project_dir);

    // 4. Deployment Status
    show_deployment_status(project_dir);

    // 5. Docker Status
    show_docker_status(project_dir).await;

    Ok(())
}

pub async fn get_summary(project_dir: &Path) -> String {
    let mut summary = Vec::new();

    // 1. Plugin Management
    if utils::is_managed(project_dir) {
        summary.push("✓ Managed".to_string());
    } else {
        summary.push("✗ Unmanaged".to_string());
    }

    // 2. Git Summary
    let git_path = project_dir.join("htdocs");
    if let Ok(git) = GitService::open(&git_path) {
        if let Ok(branch) = git.get_current_branch() {
            let mut git_info = format!("Git: {}", branch);
            if let Ok(repo) = git2::Repository::open(&git_path)
                && let Ok(statuses) = repo.statuses(None)
                && !statuses.is_empty()
            {
                git_info.push('*');
            }
            summary.push(git_info);
        } else {
            summary.push("Git: Unknown".to_string());
        }
    } else {
        summary.push("Git: None".to_string());
    }

    // 3. Docker Summary
    let docker_summary = async {
        let docker = match crate::docker::connect() {
            Ok(d) => d,
            Err(_) => return "Docker: error".to_string(),
        };

        let mut filters = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec!["com.interligent.dockerplugin.project".to_string()],
        );

        match docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
        {
            Ok(containers) => {
                let project_dir_str = project_dir.to_string_lossy().to_string();
                let project_dir_trimmed = project_dir_str.trim_end_matches('/');

                let project_containers: Vec<_> = containers
                    .into_iter()
                    .filter(|c| {
                        if let Some(labels) = &c.labels {
                            let plugin_dir = labels.get("com.interligent.dockerplugin.dir");
                            let compose_dir = labels.get("com.docker.compose.project.working_dir");

                            if let Some(d) = compose_dir {
                                d.trim_end_matches('/') == project_dir_trimmed
                            } else if let Some(d) = plugin_dir {
                                d.trim_end_matches('/') == project_dir_trimmed
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    })
                    .collect();

                let container_count = project_containers.len();
                let running_count = project_containers
                    .iter()
                    .filter(|c| c.state == Some(ContainerSummaryStateEnum::RUNNING))
                    .count();

                if container_count > 0 {
                    format!("Docker: {}/{} up", running_count, container_count)
                } else {
                    "Docker: down".to_string()
                }
            }
            Err(_) => "Docker: error".to_string(),
        }
    }
    .await;
    summary.push(docker_summary);

    summary.join(" | ")
}

fn show_git_status(project_dir: &Path) {
    let git_path = project_dir.join("htdocs");
    if let Ok(git) = GitService::open(&git_path) {
        match git.get_current_branch() {
            Ok(branch) => {
                let mut status_msg = format!("on branch {}", branch);

                // Check for dirty state
                if let Ok(repo) = git2::Repository::open(&git_path)
                    && let Ok(statuses) = repo.statuses(None)
                    && !statuses.is_empty()
                {
                    status_msg.push_str(" (uncommitted changes)");
                }

                ui::success(format!("  Git Repository: ✓ Initialized ({})", status_msg));
            }
            Err(_) => {
                ui::warning("  Git Repository: ✓ Initialized (detached HEAD or unknown state)")
            }
        }
    } else {
        ui::warning("  Git Repository: ✗ Not a git repository");
        println!("    Initialize with 'git init' in the htdocs directory");
    }
}

/// Reports whether the project is in sync with the current project template.
/// Unlike the start-up notice this also lists files with no recorded base, since
/// `status` is where the user is explicitly asking for detail.
fn show_template_status(project_dir: &Path) {
    // An unmanaged directory has no project to compare: every template file
    // would report as a missing "safe add", contradicting the line status just
    // printed about the directory not being managed at all.
    if !utils::is_managed(project_dir) {
        return;
    }

    let Ok(template_dir) = crate::template::resolve_dir() else {
        return;
    };

    let changes = match crate::template::diff(project_dir, &template_dir) {
        Ok(changes) => changes,
        Err(e) => {
            ui::warning(format!(
                "  Project Template: ✗ Could not be checked ({})",
                e
            ));
            return;
        }
    };

    let state = crate::template::TemplateState::load(project_dir);
    let summary = crate::template::Summary::from_changes(&changes);

    if summary.is_empty() {
        match &state {
            Some(state) => ui::success(format!(
                "  Project Template: ✓ Up to date (synced at {})",
                state.template_synced_at
            )),
            // No state file and nothing differing: the project matches the
            // current template, so there is nothing to record or report.
            None => ui::success("  Project Template: ✓ Matches the current template"),
        }
        return;
    }

    ui::warning("  Project Template: ○ Changes pending");
    summary.print(false);
    println!("    Run 'docker control update' to apply, or 'update --check' for diffs");
}

fn show_deployment_status(project_dir: &Path) {
    match DeployConfig::load(project_dir) {
        Ok(config) => {
            ui::success("  Deployment Config: ✓ Configured (JSON)");
            let envs: Vec<String> = config.environments.keys().cloned().collect();
            if !envs.is_empty() {
                println!("    Environments: {}", envs.join(", "));
            } else {
                println!("    No environments configured");
            }
        }
        Err(_) => {
            ui::warning("  Deployment Config: ✗ Not configured");
            println!("    Run 'docker control add-deploy-config' to add deployment environments");
        }
    }
}

async fn show_docker_status(project_dir: &Path) {
    let docker = match crate::docker::connect() {
        Ok(d) => d,
        Err(_) => {
            ui::warning("  Docker Status: ✗ Unable to connect to Docker");
            return;
        }
    };

    let mut filters = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec!["com.interligent.dockerplugin.project".to_string()],
    );

    match docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        }))
        .await
    {
        Ok(containers) => {
            let project_dir_str = project_dir.to_string_lossy().to_string();
            let project_dir_trimmed = project_dir_str.trim_end_matches('/');

            let project_containers: Vec<_> = containers
                .into_iter()
                .filter(|c| {
                    if let Some(labels) = &c.labels {
                        let plugin_dir = labels.get("com.interligent.dockerplugin.dir");
                        let compose_dir = labels.get("com.docker.compose.project.working_dir");

                        if let Some(d) = compose_dir {
                            d.trim_end_matches('/') == project_dir_trimmed
                        } else if let Some(d) = plugin_dir {
                            d.trim_end_matches('/') == project_dir_trimmed
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                })
                .collect();

            let container_count = project_containers.len();
            let running_count = project_containers
                .iter()
                .filter(|c| c.state == Some(ContainerSummaryStateEnum::RUNNING))
                .count();

            if container_count > 0 {
                if running_count > 0 {
                    ui::success(format!(
                        "  Docker Status: ✓ {}/{} containers running",
                        running_count, container_count
                    ));
                } else {
                    ui::warning(format!(
                        "  Docker Status: ○ {} containers stopped",
                        container_count
                    ));
                    println!("    Run 'docker control start' to start containers");
                }
            } else {
                ui::warning("  Docker Status: ○ No project containers found");
                println!("    Run 'docker control start' to create and start containers");
            }
        }
        Err(_) => ui::warning("  Docker Status: ✗ Unable to query container status"),
    }
}
