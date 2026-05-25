use crate::config::{DeployConfig, Environment};
use crate::git::{GitService, get_docker_user_id};
use crate::ssh;
use crate::ui;
use anyhow::{Context, Result, anyhow};
use inquire::{Confirm, Select};
use rhai::{AST, Engine, Scope};
use std::fs;
use std::path::Path;
use std::process::Command;

/// Wraps a string in single quotes and escapes any embedded single quotes so it
/// is safe to embed in a remote shell command string passed via `exec_ssh`.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub async fn execute(
    project_dir: &Path,
    env_name: String,
    release: Option<String>,
    maintenance_mode: String,
    yes: bool,
) -> Result<()> {
    ui::info(format!("Preparing deployment to: {}", env_name));

    let config = DeployConfig::load(project_dir)?;
    let env = config
        .environments
        .get(&env_name)
        .ok_or_else(|| anyhow!("Environment '{}' not found in config", env_name))?;

    let git_path = project_dir.join("htdocs");
    let git = GitService::open(&git_path)?;

    let release = if let Some(r) = release {
        r
    } else {
        ui::info("Fetching available releases...");
        // Fetch tags to ensure we have the latest
        if let Err(e) = git.fetch_tags() {
            ui::warning(format!("Git fetch tags failed: {}. Continuing anyway.", e));
        }

        let tags = git.list_tags()?;
        if tags.is_empty() {
            return Err(anyhow!(
                "No tags found in repository. Please create a release first."
            ));
        }

        Select::new("Select release to deploy", tags).prompt()?
    };
    ui::info(format!("Selected release: {}", release));

    let project_name = get_project_name(project_dir);
    let changelog = git.get_changelog(&release);

    // Confirm deployment
    if !yes
        && !Confirm::new(&format!(
            "Proceed with deployment of '{}' to '{}' environment?",
            release, env_name
        ))
        .with_default(false)
        .prompt()?
    {
        ui::info("Deployment cancelled");
        return Ok(());
    }

    // Teams notification: started
    if let Some(webhook) = &env.teams_webhook_url {
        let _ = send_teams_notification(
            webhook,
            &project_name,
            &env_name,
            &release,
            "started",
            &changelog,
        )
        .await;
    }

    ui::info(format!("Deploying to {}@{}...", env.user, env.domain));

    // Create deployment archive
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();
    let release_dir = format!("{}_{}", timestamp, release);
    let archive_name = format!("{}.7z", release_dir);
    let deployments_dir = project_dir.join("deployments");
    if !deployments_dir.exists() {
        fs::create_dir_all(&deployments_dir)?;
    }
    let archive_path = deployments_dir.join(&archive_name);

    // @todo: call pre_create_archive_hook

    if let Err(e) = create_deployment_archive(project_dir, &release, &archive_path).await {
        if let Some(webhook) = &env.teams_webhook_url {
            let _ = send_teams_notification(
                webhook,
                &project_name,
                &env_name,
                &release,
                "failed",
                &format!("Archive creation failed: {}", e),
            )
            .await;
        }
        return Err(e);
    }

    // Transfer and execute
    let server_root = env.service_root.as_deref().unwrap_or("/var/www/html");
    let console_command = env.console_command.as_deref().unwrap_or("bin/console");

    if let Err(e) = perform_deployment(DeploymentContext {
        project_dir,
        env,
        env_name: &env_name,
        archive_path: &archive_path,
        release_dir: &release_dir,
        server_root,
        console_command,
        maintenance_mode: &maintenance_mode,
        yes,
    })
    .await
    {
        let _ = fs::remove_file(&archive_path);
        if let Some(webhook) = &env.teams_webhook_url {
            let _ = send_teams_notification(
                webhook,
                &project_name,
                &env_name,
                &release,
                "failed",
                &format!("Deployment failed: {}", e),
            )
            .await;
        }
        return Err(e);
    }

    let _ = fs::remove_file(&archive_path);

    // Teams notification: success
    if let Some(webhook) = &env.teams_webhook_url {
        let _ = send_teams_notification(
            webhook,
            &project_name,
            &env_name,
            &release,
            "success",
            &changelog,
        )
        .await;
    }

    ui::success("Deployment completed successfully!");

    Ok(())
}

fn get_project_name(project_dir: &Path) -> String {
    get_env_var(project_dir, "PROJECTNAME").unwrap_or_else(|| {
        project_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    })
}

fn get_env_var(project_dir: &Path, key: &str) -> Option<String> {
    if let Ok(content) = fs::read_to_string(project_dir.join(".env")) {
        for line in content.lines() {
            if let Some(pos) = line.find('=') {
                let k = line[..pos].trim();
                let v = line[pos + 1..].trim();
                if k == key {
                    return Some(v.trim_matches('"').trim_matches('\'').to_string());
                }
            }
        }
    }
    None
}

async fn create_deployment_archive(
    project_dir: &Path,
    release: &str,
    archive_path: &Path,
) -> Result<()> {
    ui::info(format!("Creating deployment archive for {}...", release));

    let deployments_dir = project_dir.join("deployments");
    let temp_extract_dir = deployments_dir.join(format!("temp_{}", release));
    if temp_extract_dir.exists() {
        fs::remove_dir_all(&temp_extract_dir)?;
    }
    fs::create_dir_all(&temp_extract_dir)?;

    // Use git2 to extract files instead of git archive | tar
    let git_path = project_dir.join("htdocs");
    let repo = git2::Repository::open(&git_path)
        .context(format!("Failed to open git repository at {:?}", git_path))?;
    let obj = repo
        .revparse_single(release)
        .context(format!("Failed to find release '{}'", release))?;
    let _ = obj.peel_to_tree()?;

    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.target_dir(&temp_extract_dir);
    checkout.force();
    repo.checkout_tree(&obj, Some(&mut checkout))
        .context("Failed to checkout tree to temp directory")?;

    // Run composer install inside a container for parity with bash
    let php_version = get_env_var(project_dir, "PHP_VERSION").unwrap_or_else(|| "8.2".to_string());
    let ssh_auth_port = std::env::var("SSH_AUTH_PORT")
        .ok()
        .or_else(|| get_env_var(project_dir, "SSH_AUTH_PORT"))
        .unwrap_or_else(|| "host.docker.internal:2222".to_string());

    ui::info(format!(
        "Running composer install via Docker (PHP {})...",
        php_version
    ));

    let mut docker_cmd = Command::new("docker");
    docker_cmd
        .arg("run")
        .arg("--rm")
        .arg("-u")
        .arg(get_docker_user_id())
        .arg("--group-add")
        .arg("www-data")
        .arg("-e")
        .arg(format!("SSH_AUTH_PORT={}", ssh_auth_port));

    docker_cmd
        .arg("-e")
        .arg("SSH_AUTH_SOCK=/tmp/ssh-agent.sock")
        .arg("--add-host")
        .arg("host.docker.internal:host-gateway")
        .arg("-v")
        .arg(format!(
            "{}/volumes/composer-cache:/var/www/.composer/cache",
            project_dir.display()
        ))
        .arg("-v")
        .arg(format!("{}:/var/www/html", temp_extract_dir.display()))
        .arg(format!("fduarte42/docker-php:{}", php_version))
        .arg("bash")
        .arg("-c")
        .arg("git config --global --add safe.directory /var/www/html; /docker-php-init; composer i -o");

    let status = docker_cmd.status()?;
    if !status.success() {
        let _ = fs::remove_dir_all(&temp_extract_dir);
        return Err(anyhow!("Composer install failed"));
    }

    // 7z a <archive_path> <temp_dir>/*
    let status = Command::new("7z")
        .arg("a")
        .arg(archive_path)
        .arg(format!("{}/.", temp_extract_dir.display()))
        .status()?;

    if !status.success() {
        let _ = fs::remove_dir_all(&temp_extract_dir);
        return Err(anyhow!("7z compression failed"));
    }

    fs::remove_dir_all(&temp_extract_dir)?;

    Ok(())
}

struct DeploymentContext<'a> {
    project_dir: &'a Path,
    env: &'a Environment,
    env_name: &'a str,
    archive_path: &'a Path,
    release_dir: &'a str,
    server_root: &'a str,
    console_command: &'a str,
    maintenance_mode: &'a str,
    yes: bool,
}

async fn perform_deployment(ctx: DeploymentContext<'_>) -> Result<()> {
    let user = &ctx.env.user;
    let domain = &ctx.env.domain;
    let remote_releases = format!("{}/releases", ctx.server_root);
    let remote_archive = format!(
        "{}/{}",
        remote_releases,
        ctx.archive_path.file_name().unwrap().to_str().unwrap()
    );
    let remote_release_path = format!("{}/{}", remote_releases, ctx.release_dir);

    // 1. Ensure releases dir exists
    ssh::exec_ssh(user, domain, &format!("mkdir -p {}", sq(&remote_releases)))?;

    // 2. Transfer archive
    ui::info("Transferring archive...");
    ssh::copy_ssh(user, domain, ctx.archive_path, &remote_archive)?;

    // 3. Extract and remove archive
    ui::info("Extracting archive...");
    ssh::exec_ssh(
        user,
        domain,
        &format!("mkdir -p {}", sq(&remote_release_path)),
    )?;
    ssh::exec_ssh(
        user,
        domain,
        &format!(
            "7z x -o{} {}",
            sq(&remote_release_path),
            sq(&remote_archive)
        ),
    )?;
    ssh::exec_ssh(user, domain, &format!("rm -f {}", sq(&remote_archive)))?;

    // 4. Cleanup old releases (keep last 5)
    // ls -d1t $SERVER_ROOT/releases/* | grep -v $(readlink -f $SERVER_ROOT/current) | egrep "^$SERVER_ROOT/releases/[0-9]{14}_.+$" | tail -n +6 | xargs rm -rf
    let cleanup_cmd = format!(
        "bash -c 'ls -d1t {remote_releases}/* 2>/dev/null | grep -v $(readlink -f {}/current 2>/dev/null || echo \"none\") | grep -E \"{remote_releases}/[0-9]{{14}}_.+$\" | tail -n +6 | xargs rm -rf 2>/dev/null || true'",
        ctx.server_root
    );
    let _ = ssh::exec_ssh(user, domain, &cleanup_cmd);

    // 5. Shared paths
    ui::info("Handling shared paths...");
    if let Some(dirs) = &ctx.env.shared_directories {
        for dir in dirs {
            let shared_path = format!("{}/shared/{}", ctx.server_root, dir);
            let target_path = format!("{}/{}", remote_release_path, dir);
            ssh::exec_ssh(user, domain, &format!("mkdir -p {}", sq(&shared_path)))?;
            ssh::exec_ssh(
                user,
                domain,
                &format!(
                    "rm -rf {} && ln -sf {} {}",
                    sq(&target_path),
                    sq(&shared_path),
                    sq(&target_path)
                ),
            )?;
        }
    }
    if let Some(files) = &ctx.env.shared_files {
        for file in files {
            let shared_path = format!("{}/shared/{}", ctx.server_root, file);
            let target_path = format!("{}/{}", remote_release_path, file);
            let shared_dir = Path::new(&shared_path)
                .parent()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            ssh::exec_ssh(user, domain, &format!("mkdir -p {}", sq(&shared_dir)))?;
            ssh::exec_ssh(user, domain, &format!("touch {}", sq(&shared_path)))?;
            ssh::exec_ssh(
                user,
                domain,
                &format!(
                    "rm -f {} && ln -sf {} {}",
                    sq(&target_path),
                    sq(&shared_path),
                    sq(&target_path)
                ),
            )?;
        }
    }

    // 6. prepare console commands
    let (php_bin, php_cmd) = if ctx.console_command.starts_with("php ") {
        (
            "php",
            ctx.console_command["php ".len()..].trim_start_matches('/'),
        )
    } else {
        ("php", ctx.console_command.trim_start_matches('/'))
    };

    let console_current = format!(
        "{} {}/current/{}",
        php_bin,
        ctx.server_root.trim_end_matches('/'),
        php_cmd
    );
    let console_new = format!(
        "{} {}/{}",
        php_bin,
        remote_release_path.trim_end_matches('/'),
        php_cmd
    );

    // 7. Rhai setup
    let rhai_engine = setup_rhai_engine(user.to_string(), domain.to_string());
    let rhai_ast = find_hook_file(ctx.project_dir, ctx.env_name)
        .map(|path| compile_hook(&rhai_engine, &path))
        .transpose()?;

    let hook_ctx = HookContext {
        server_root: ctx.server_root,
        release_dir: ctx.release_dir,
        console_current: &console_current,
        console_new: &console_new,
    };

    // 8. Reload FPM
    ui::info("Reloading FPM...");
    let _ = ssh::exec_ssh(user, domain, "sudo php-fpm-reload.sh");

    // 9. Maintenance mode selection and activation
    let maintenance_mode = if ctx.yes {
        ctx.maintenance_mode.to_string()
    } else {
        Select::new("Select maintenance mode", vec!["hard", "soft"])
            .with_starting_cursor(if ctx.maintenance_mode == "soft" { 1 } else { 0 })
            .prompt()?
            .to_string()
    };
    ui::info(format!("Enabling maintenance mode ({})", maintenance_mode));

    // We try to enable maintenance on current if it exists, and on new.
    let _ = ssh::exec_ssh(
        user,
        domain,
        &format!(
            "{} shared:maintenance {}",
            console_current, maintenance_mode
        ),
    );
    let _ = ssh::exec_ssh(
        user,
        domain,
        &format!("{} shared:maintenance {}", console_new, maintenance_mode),
    );

    // 10. Hooks: pre_deploy
    execute_hook(&rhai_engine, &rhai_ast, &hook_ctx, "pre_deploy")?;

    // 11. Cache clearing, migrations, etc.
    ui::info("Executing deployment tasks...");
    ssh::exec_ssh(user, domain, &format!("{} shared:clear-opcc", console_new))?;
    ssh::exec_ssh(
        user,
        domain,
        &format!("{} orm:clear-cache:metadata", console_new),
    )?;
    ssh::exec_ssh(
        user,
        domain,
        &format!("{} orm:clear-cache:query", console_new),
    )?;

    if ctx.yes
        || Confirm::new("Clear result cache?")
            .with_default(false)
            .prompt()?
    {
        ssh::exec_ssh(
            user,
            domain,
            &format!("{} orm:clear-cache:result", console_new),
        )?;
    }

    if ctx.yes
        || Confirm::new("Execute migrations?")
            .with_default(true)
            .prompt()?
    {
        ssh::exec_ssh(
            user,
            domain,
            &format!("{} migrations:migrate --no-interaction", console_new),
        )?;
    }

    if ctx.yes
        || Confirm::new("Execute schema-tool?")
            .with_default(false)
            .prompt()?
    {
        ssh::exec_ssh(
            user,
            domain,
            &format!("{} orm:schema-tool:update --dump-sql", console_new),
        )?;
    }

    // 12. COPS Integration
    if ctx.env.cops_integration.unwrap_or(false) {
        ui::info("Executing COPS integration...");

        if let Err(e) = ssh::exec_ssh(user, domain, &format!("{} cops:outdated", console_new)) {
            if ctx.yes {
                return Err(anyhow!(
                    "COPS outdated check failed: {}. Deployment aborted.",
                    e
                ));
            }
            ui::warning(format!("COPS outdated command failed: {}", e));
            if !Confirm::new("Do you want to continue deployment despite COPS command failure?")
                .with_default(false)
                .prompt()?
            {
                // Disable maintenance mode before exiting
                let _ = ssh::exec_ssh(
                    user,
                    domain,
                    &format!("{} shared:maintenance off", console_new),
                );
                return Err(anyhow!(
                    "Deployment aborted due to COPS integration failure"
                ));
            }
        }

        if let Err(e) = ssh::exec_ssh(user, domain, &format!("{} cops:permissions", console_new)) {
            if ctx.yes {
                return Err(anyhow!(
                    "COPS permissions check failed: {}. Deployment aborted.",
                    e
                ));
            }
            ui::warning(format!("COPS permissions command failed: {}", e));
            if !Confirm::new("Do you want to continue deployment despite COPS command failure?")
                .with_default(false)
                .prompt()?
            {
                // Disable maintenance mode before exiting
                let _ = ssh::exec_ssh(
                    user,
                    domain,
                    &format!("{} shared:maintenance off", console_new),
                );
                return Err(anyhow!(
                    "Deployment aborted due to COPS integration failure"
                ));
            }
        }
    }

    // 10. Hooks: post_deploy
    execute_hook(&rhai_engine, &rhai_ast, &hook_ctx, "post_deploy")?;

    ui::info("Basic deployment done. You can now run custom commands on the server.");
    if !ctx.yes {
        ui::info("Press ENTER to continue and finish deployment...");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
    }

    // 11. Update symlink
    ui::info("Updating current symlink...");
    let current_symlink = format!("{}/current", ctx.server_root);
    let release_target = format!("releases/{}", ctx.release_dir);
    ssh::exec_ssh(
        user,
        domain,
        &format!(
            "rm -f {} && ln -s {} {}",
            sq(&current_symlink),
            sq(&release_target),
            sq(&current_symlink)
        ),
    )?;

    // 12. Maintenance mode OFF (on new release)
    ui::info("Disabling maintenance mode...");
    ssh::exec_ssh(
        user,
        domain,
        &format!("{} shared:maintenance off", console_new),
    )?;

    // 14. Final bytecode cache clear
    ui::info("Final bytecode cache clear...");
    ssh::exec_ssh(user, domain, &format!("{} shared:clear-opcc", console_new))?;

    // 15. Hooks: done_deploy
    execute_hook(&rhai_engine, &rhai_ast, &hook_ctx, "done_deploy")?;

    Ok(())
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static MOCK_SSH_COMMANDS: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
}

fn setup_rhai_engine(_user: String, _domain: String) -> Engine {
    let mut engine = Engine::new();

    #[cfg(not(test))]
    let user_clone = _user.clone();
    #[cfg(not(test))]
    let domain_clone = _domain.clone();
    engine.register_fn(
        "exec_ssh",
        move |command: &str| -> Result<(), Box<rhai::EvalAltResult>> {
            #[cfg(not(test))]
            if let Err(e) = ssh::exec_ssh(&user_clone, &domain_clone, command) {
                return Err(Box::new(rhai::EvalAltResult::ErrorRuntime(
                    format!("SSH command failed: {}", e).into(),
                    rhai::Position::NONE,
                )));
            }

            #[cfg(test)]
            MOCK_SSH_COMMANDS.with(|c| c.borrow_mut().push(command.to_string()));

            Ok(())
        },
    );

    engine
}

struct HookContext<'a> {
    server_root: &'a str,
    release_dir: &'a str,
    console_current: &'a str,
    console_new: &'a str,
}

fn find_hook_file(project_dir: &Path, env_name: &str) -> Option<std::path::PathBuf> {
    let rhai_file = format!("{}.rhai", env_name);
    let htdocs_scripts = project_dir
        .join("htdocs/.docker-control/deployment-scripts")
        .join(&rhai_file);
    if htdocs_scripts.exists() {
        return Some(htdocs_scripts);
    }
    let deployments_scripts = project_dir.join("deployments/scripts").join(&rhai_file);
    if deployments_scripts.exists() {
        return Some(deployments_scripts);
    }
    None
}

fn compile_hook(engine: &Engine, path: &Path) -> Result<AST> {
    engine
        .compile_file(path.to_path_buf())
        .map_err(|e| anyhow!("Failed to compile Rhai script: {}", e))
}

fn execute_hook(
    engine: &Engine,
    ast: &Option<AST>,
    ctx: &HookContext,
    function_name: &str,
) -> Result<()> {
    let ast = match ast {
        Some(ast) => ast,
        None => return Ok(()),
    };

    ui::info(format!("Executing hook: {}...", function_name));

    let mut scope = Scope::new();

    let result: Result<(), Box<rhai::EvalAltResult>> = engine.call_fn(
        &mut scope,
        ast,
        function_name,
        (
            ctx.console_current.to_string(),
            ctx.release_dir.to_string(),
            ctx.console_new.to_string(),
            ctx.server_root.to_string(),
        ),
    );

    if let Err(e) = result {
        if let rhai::EvalAltResult::ErrorFunctionNotFound(ref f, _) = *e
            && f.starts_with(function_name)
        {
            return Ok(());
        }
        return Err(anyhow!("Hook {} failed: {}", function_name, e));
    }

    Ok(())
}

async fn send_teams_notification(
    webhook_url: &str,
    project_name: &str,
    env_name: &str,
    release: &str,
    status: &str,
    changelog: &str,
) -> Result<()> {
    let client = reqwest::Client::new();
    let title = format!("Deployment {status} - {project_name} [{env_name}]");
    let color = match status {
        "started" => "0078D4",
        "success" => "00FF00",
        "failed" => "FF0000",
        _ => "CCCCCC",
    };

    let body = serde_json::json!({
        "@type": "MessageCard",
        "@context": "http://schema.org/extensions",
        "themeColor": color,
        "summary": title,
        "sections": [{
            "activityTitle": title,
            "facts": [
                { "name": "Project", "value": project_name },
                { "name": "Environment", "value": env_name },
                { "name": "Release", "value": release },
                { "name": "Status", "value": status }
            ],
            "text": changelog
        }]
    });

    let _ = client.post(webhook_url).json(&body).send().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_execute_hook() -> Result<()> {
        let dir = tempdir()?;
        let htdocs_scripts = dir.path().join("htdocs/.docker-control/deployment-scripts");
        fs::create_dir_all(&htdocs_scripts)?;

        let rhai_content = r#"
            fn pre_deploy(console_current, release_dir, console_new, server_root) {
                exec_ssh("echo pre_deploy " + release_dir);
            }
            fn post_deploy(console_current, release_dir, console_new, server_root) {
                exec_ssh("echo post_deploy " + console_new);
            }
        "#;
        fs::write(htdocs_scripts.join("test_env.rhai"), rhai_content)?;

        let ctx = HookContext {
            server_root: "/var/www",
            release_dir: "20240101_v1",
            console_new: "php bin/console",
            console_current: "php bin/console",
        };

        // Reset mock commands
        MOCK_SSH_COMMANDS.with(|c| c.borrow_mut().clear());

        let rhai_engine = setup_rhai_engine("user".to_string(), "example.com".to_string());
        let rhai_ast = find_hook_file(dir.path(), "test_env")
            .map(|path| compile_hook(&rhai_engine, &path).expect("Failed to compile"));

        execute_hook(&rhai_engine, &rhai_ast, &ctx, "pre_deploy")?;
        execute_hook(&rhai_engine, &rhai_ast, &ctx, "post_deploy")?;
        execute_hook(&rhai_engine, &rhai_ast, &ctx, "done_deploy")?; // Should do nothing as it's not in script

        MOCK_SSH_COMMANDS.with(|c| {
            let cmds = c.borrow();
            assert_eq!(cmds.len(), 2);
            assert_eq!(cmds[0], "echo pre_deploy 20240101_v1");
            assert_eq!(cmds[1], "echo post_deploy php bin/console");
        });

        Ok(())
    }
}
