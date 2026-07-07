#![allow(clippy::collapsible_if)]

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use daemonize::Daemonize;
use inquire::Confirm;
use std::path::PathBuf;
use tokio::signal;

use docker_control::{SSH_AGENT_PORT, assets, commands, docker, ui, utils};

#[derive(Parser)]
#[command(name = "docker-control")]
#[command(about = "IK Docker Control CLI Plugin", long_about = None)]
#[command(version)]
struct Cli {
    /// Specify the project directory (default: current directory)
    #[arg(short, long, value_name = "DIRECTORY")]
    dir: Option<PathBuf>,

    /// Enable debug output
    #[arg(long, global = true)]
    debug: bool,

    /// Start SSH agent forwarding daemon
    #[arg(long)]
    start_ssh_agent: bool,

    /// Stop SSH agent forwarding daemon
    #[arg(long)]
    stop_ssh_agent: bool,

    /// Restart SSH agent forwarding daemon
    #[arg(long)]
    restart_ssh_agent: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Add deployment configuration for environments
    AddDeployConfig,
    /// Build the Docker containers for the project
    Build {
        /// Pass additional arguments to docker-compose build
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Open a shell inside a container
    Console {
        /// Container name (defaults to 'php')
        container: Option<String>,
    },
    /// Create a custom control script
    CreateControlScript {
        /// Name of the control script
        name: String,
    },
    /// Deploy a selected release to the specified environment
    Deploy {
        /// Target environment (e.g., production, staging)
        env: String,

        /// Specific release to deploy (skips interactive selection)
        #[arg(short, long)]
        release: Option<String>,

        /// Maintenance mode to use when --yes is specified (hard|soft)
        #[arg(long, default_value = "hard")]
        maintenance_mode: String,

        /// Skip all interactive prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Initialize an empty directory with the project template
    Init,
    /// Merge release branch to main using selective cherry-pick workflow
    Merge {
        /// Optional module name
        module: Option<String>,
    },
    /// Pull the latest Docker images for the project
    Pull,
    /// Pull the latest ingress-related Docker images
    PullIngress,
    /// Create a new release branch
    Release {
        /// Optional module name
        module: Option<String>,
    },
    /// Restart the project containers
    Restart,
    /// Restart the ingress containers
    RestartIngress,
    /// Fix host and container ACL permissions on htdocs
    #[command(name = "setacl")]
    SetAcl,
    /// Show all running projects managed by the Docker plugin
    ShowRunning,
    /// Start the project containers
    Start,
    /// Start the ingress containers
    StartIngress,
    /// Show the status of the project containers
    Status,
    /// Show the status of the ingress containers
    StatusIngress,
    /// Stop the project containers
    Stop,
    /// Stop the ingress containers
    StopIngress,
    /// Migrate from old docker-control project
    #[command(hide = true)]
    Migrate,
    /// Update the project with the current template
    Update,
    /// Upgrade docker-control itself via Homebrew
    Upgrade,
    /// Return metadata for Docker CLI plugin
    #[command(name = "docker-cli-plugin-metadata", hide = true)]
    Metadata,
    /// Execute a custom control script
    #[command(external_subcommand)]
    External(Vec<String>),
}

fn get_help_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .usage(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Handle stop synchronously
    if args.contains(&"--stop-ssh-agent".to_string()) {
        if let Err(e) = docker_control::utils::stop_ssh_agent() {
            eprintln!("Failed to stop SSH agent: {}", e);
            std::process::exit(1);
        } else {
            println!("SSH agent forwarding stopped.");
        }
        return;
    }

    // Handle restart: stop then start
    if args.contains(&"--restart-ssh-agent".to_string()) {
        if let Err(e) = docker_control::utils::stop_ssh_agent() {
            eprintln!("Warning: Failed to stop SSH agent: {}", e);
        } else {
            // Wait for port to close
            let platform_info = utils::platform::detect_platform();
            for _ in 0..50 {
                // wait up to 5 seconds
                if !utils::forwarding::is_port_open(&platform_info.bind_ip, SSH_AGENT_PORT) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        // Fall through to start
    }

    // Handle start or restart
    if args.contains(&"--start-ssh-agent".to_string())
        || args.contains(&"--restart-ssh-agent".to_string())
    {
        let pid_file = "/tmp/docker-control-ssh-agent.pid";
        let stdout_file = "/tmp/docker-control-ssh-agent.log";
        let stderr_file = "/tmp/docker-control-ssh-agent.err";

        let daemonize = Daemonize::new()
            .pid_file(pid_file)
            .stdout(std::fs::File::create(stdout_file).unwrap())
            .stderr(std::fs::File::create(stderr_file).unwrap());

        if let Err(e) = daemonize.start() {
            // Check whether the daemon is genuinely running by reading the pid file
            // and verifying the process is alive, rather than showing a cryptic error.
            let running_pid = std::fs::read_to_string(pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .filter(|&pid| {
                    // /proc/<pid> exists on Linux; on macOS fall back to kill -0
                    #[cfg(target_os = "linux")]
                    {
                        std::path::Path::new(&format!("/proc/{}", pid)).exists()
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
                    }
                });

            if let Some(pid) = running_pid {
                eprintln!("SSH agent daemon is already running (PID: {}).", pid);
            } else {
                eprintln!("Failed to start SSH agent daemon: {}", e);
                std::process::exit(1);
            }
            return;
        }

        // This code runs only in the daemon child process.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let platform_info = utils::platform::detect_platform();
            if let Err(e) = utils::forwarding::ensure_forwarding(&platform_info).await {
                eprintln!("Forwarding setup failed: {}", e);
                std::process::exit(1);
            }
            eprintln!("SSH agent forwarding started in daemon mode.");
            signal::ctrl_c().await.unwrap();
        });
        return;
    }

    // Ensure the SSH agent forwarding daemon is running and publish SSH_AUTH_PORT
    // before creating the Tokio runtime so set_var is single-threaded (safe).
    // Skip for commands that don't use SSH and would otherwise block on `docker info`
    // inside detect_platform() before the user sees any output.
    let no_ssh_needed = args.iter().any(|a| {
        a == "--help"
            || a == "-h"
            || a == "--version"
            || a == "-V"
            || a == "docker-cli-plugin-metadata"
            || a == "upgrade"
    });
    if !no_ssh_needed && std::env::var("DOCKER_CONTROL_SKIP_SSH_AGENT").is_err() {
        let platform_info = utils::platform::detect_platform();
        if !utils::forwarding::is_port_open(&platform_info.bind_ip, SSH_AGENT_PORT) {
            eprintln!("Starting SSH agent forwarding daemon...");
            match std::env::current_exe() {
                Ok(exe) => {
                    match std::process::Command::new(&exe)
                        .arg("--start-ssh-agent")
                        .spawn()
                    {
                        Ok(_) => {
                            for _ in 0..50 {
                                if utils::forwarding::is_port_open(
                                    &platform_info.bind_ip,
                                    SSH_AGENT_PORT,
                                ) {
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                        }
                        Err(e) => {
                            eprintln!("Warning: Failed to spawn SSH agent daemon: {}", e);
                        }
                    }
                }
                Err(_) => {
                    eprintln!(
                        "Warning: Could not determine executable path to start SSH agent daemon"
                    );
                }
            }
        }

        if utils::forwarding::is_port_open(&platform_info.bind_ip, SSH_AGENT_PORT) {
            // SAFETY: called before the Tokio runtime is created, so no other
            // threads exist yet and there are no concurrent env reads.
            unsafe {
                std::env::set_var(
                    "SSH_AUTH_PORT",
                    format!("{}:{}", platform_info.bind_ip, SSH_AGENT_PORT),
                );
            }
        } else {
            eprintln!(
                "Warning: SSH agent forwarding is not available. SSH keys may not be accessible."
            );
        }
    }

    // Normal path
    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Err(e) = rt.block_on(async_main()) {
        ui::critical(format!("Error: {}", e));
        std::process::exit(1);
    }
}

async fn async_main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Check for help flags early to show status in help
    let is_help = args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "help");

    // Check for version early
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("docker-control {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Manually parse dir for metadata and help
    let mut project_dir = std::env::current_dir().expect("Failed to get current directory");
    for i in 0..args.len() {
        if (args[i] == "--dir" || args[i] == "-d") && i + 1 < args.len() {
            project_dir = PathBuf::from(&args[i + 1]);
            break;
        }
    }

    let project_dir = if project_dir.exists() {
        project_dir.canonicalize().unwrap_or(project_dir)
    } else {
        project_dir
    };

    // Return early if metadata is requested, no dependencies needed
    if args.iter().any(|arg| arg == "docker-cli-plugin-metadata") {
        let metadata = serde_json::json!({
            "SchemaVersion": "0.1.0",
            "Vendor": "INTERLIGENT kommunizieren GmbH",
            "Version": env!("CARGO_PKG_VERSION"),
            "ShortDescription": "IK Docker Control CLI Plugin"
        });
        println!("{}", serde_json::to_string(&metadata).unwrap());
        return Ok(());
    }

    if is_help {
        let summary = commands::status::get_summary(&project_dir).await;
        let status_line = format!("{}: {}", ui::yellow("Project Status"), ui::cyan(summary));
        println!("{}\n", status_line);

        let custom_commands = commands::custom::get_custom_commands(&project_dir);
        let mut custom_help = String::new();
        if project_dir.join("control.cmd").exists() {
            custom_help.push_str(&format!("\n{}\n", ui::yellow("Migration Command:")));
            custom_help.push_str(&format!(
                "  {:22} {}\n",
                ui::cyan("migrate"),
                "Migrate from old docker-control project"
            ));
        }
        if !custom_commands.is_empty() {
            custom_help.push_str(&format!("\n{}\n", ui::yellow("Custom Commands:")));
            for cmd in custom_commands {
                custom_help.push_str(&format!(
                    "  {:22} {}\n",
                    ui::cyan(&cmd.name),
                    cmd.description
                ));
            }
        }

        let help_template = format!(
            "{{before-help}}{{name}} {{version}}\n{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{}\n{{subcommands}}\n{}\n{}\n{{options}}",
            ui::yellow("Commands:"),
            custom_help,
            ui::yellow("Options:")
        );

        // Use the factory to get a new command with the template and print help
        Cli::command()
            .styles(get_help_styles())
            .help_template(help_template)
            .print_help()
            .unwrap();
        println!();
        return Ok(());
    }

    let cmd = Cli::command().styles(get_help_styles());
    let matches = cmd.try_get_matches()?;
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    if cli.debug {
        ui::set_debug(true);
    }

    // Check external dependencies
    if std::env::var("DOCKER_CONTROL_SKIP_DEPENDENCY_CHECK").is_err() {
        utils::dependencies::check_dependencies()?;
    }

    // Initialize assets
    if let Ok(asset_manager) = assets::AssetManager::new() {
        if let Err(e) = asset_manager.ensure_assets() {
            ui::warning(format!(
                "Failed to ensure assets: {}. Falling back to local/env paths.",
                e
            ));
        }
    }

    let project_dir = cli
        .dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory"));

    let project_dir = if project_dir.exists() {
        project_dir
            .canonicalize()
            .unwrap_or_else(|_| project_dir.clone())
    } else {
        project_dir
    };

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            // If no command is provided, show status and help summary
            if let Err(e) = commands::status::execute(&project_dir).await {
                ui::critical(format!("Error showing status: {}", e));
                return Err(e);
            }
            println!("\nRun 'docker control --help' for a list of available commands.");
            return Ok(());
        }
    };

    match command {
        Commands::Metadata => unreachable!(),
        Commands::AddDeployConfig => {
            commands::add_deploy_config::execute(&project_dir)?;
        }
        Commands::Build { args } => {
            check_managed(&project_dir);
            let mut all_args = vec!["build"];
            for arg in &args {
                all_args.push(arg);
            }
            docker::execute_compose(&project_dir, &all_args)?;
        }
        Commands::Console { container } => {
            check_managed(&project_dir);
            docker::console(&project_dir, container)?;
        }
        Commands::CreateControlScript { name } => {
            commands::create_script::execute(&project_dir, &name)?;
        }
        Commands::Deploy {
            env,
            release,
            maintenance_mode,
            yes,
        } => {
            commands::deploy::execute(&project_dir, env, release, maintenance_mode, yes).await?;
        }
        Commands::Init => {
            commands::init::execute(&project_dir).await?;
        }
        Commands::Merge { module } => {
            commands::merge::execute(
                &project_dir,
                module,
                commands::merge::MergeOptions::default(),
            )?;
        }
        Commands::Pull => {
            check_managed(&project_dir);
            docker::execute_compose(&project_dir, &["pull"])?;
        }
        Commands::PullIngress => {
            docker::execute_ingress_compose(&["pull"])?;
        }
        Commands::Release { module } => {
            commands::release::execute(
                &project_dir,
                module,
                commands::release::ReleaseOptions::default(),
            )?;
        }
        Commands::Restart => {
            check_managed(&project_dir);
            maybe_offer_image_pull(&project_dir);
            docker::execute_compose(&project_dir, &["down"])?;
            docker::execute_compose(&project_dir, &["up", "-d"])?;
            if let Err(e) = utils::acl::apply_host_acl(&project_dir) {
                ui::warning(format!(
                    "Could not set host ACL permissions on htdocs: {}",
                    e
                ));
            }
            if let Err(e) = utils::acl::apply_container_acl(&project_dir) {
                ui::warning(format!(
                    "Could not set container ACL permissions on htdocs: {}",
                    e
                ));
            }
        }
        Commands::RestartIngress => {
            docker::execute_ingress_compose(&["down"])?;
            docker::execute_ingress_compose(&["up", "-d"])?;
        }
        Commands::SetAcl => {
            check_managed(&project_dir);
            if !docker::is_running(&project_dir) {
                ui::critical(
                    "Project containers are not running. Start the project first with `docker-control start`.",
                );
                std::process::exit(1);
            }
            if let Err(e) = utils::acl::apply_host_acl(&project_dir) {
                ui::warning(format!(
                    "Could not set host ACL permissions on htdocs: {}",
                    e
                ));
            }
            if let Err(e) = utils::acl::apply_container_acl(&project_dir) {
                ui::warning(format!(
                    "Could not set container ACL permissions on htdocs: {}",
                    e
                ));
            }
        }
        Commands::ShowRunning => {
            commands::show_running::execute().await?;
        }
        Commands::Start => {
            check_managed(&project_dir);
            maybe_offer_image_pull(&project_dir);
            docker::execute_compose(&project_dir, &["up", "-d"])?;
            if let Err(e) = utils::acl::apply_host_acl(&project_dir) {
                ui::warning(format!(
                    "Could not set host ACL permissions on htdocs: {}",
                    e
                ));
            }
            if let Err(e) = utils::acl::apply_container_acl(&project_dir) {
                ui::warning(format!(
                    "Could not set container ACL permissions on htdocs: {}",
                    e
                ));
            }
        }
        Commands::StartIngress => {
            docker::execute_ingress_compose(&["up", "-d"])?;
        }
        Commands::Status => {
            commands::status::execute(&project_dir).await?;
            // Also show docker compose ps as it was before
            let _ = docker::execute_compose(&project_dir, &["ps"]);
        }
        Commands::StatusIngress => {
            docker::execute_ingress_compose(&["ps"])?;
        }
        Commands::Stop => {
            check_managed(&project_dir);
            docker::execute_compose(&project_dir, &["down"])?;
        }
        Commands::StopIngress => {
            docker::execute_ingress_compose(&["down"])?;
        }
        Commands::Migrate => {
            commands::migrate::execute(&project_dir).await?;
        }
        Commands::Update => {
            commands::update::execute(&project_dir)?;
        }
        Commands::Upgrade => {
            commands::upgrade::execute()?;
        }
        Commands::External(args) => {
            execute_external_script(&project_dir, args)?;
        }
    }

    Ok(())
}

fn execute_external_script(project_dir: &std::path::Path, args: Vec<String>) -> anyhow::Result<()> {
    if args.is_empty() {
        return Err(anyhow::anyhow!("No command provided"));
    }

    let command_name = &args[0];
    let command_args = &args[1..];

    utils::sanitize_command_name(command_name)?;

    let mut paths = vec![
        project_dir.join(format!(
            "htdocs/.docker-control/control-scripts/{}",
            command_name
        )),
        project_dir.join(format!("control-scripts/{}", command_name)),
    ];

    if !command_name.ends_with(".sh") {
        paths.push(project_dir.join(format!(
            "htdocs/.docker-control/control-scripts/{}.sh",
            command_name
        )));
        paths.push(project_dir.join(format!("control-scripts/{}.sh", command_name)));
    }

    for path in paths {
        if path.exists() {
            ui::info(format!("Executing custom script: {:?}", path));
            let mut cmd = std::process::Command::new("bash");
            cmd.arg(&path)
                .args(command_args)
                .current_dir(project_dir)
                .env("PROJECT_DIR", project_dir);

            // Set environment variables for the script if needed
            // original bash script has access to LIB_DIR, PROJECT_DIR, etc.

            let status = cmd.status()?;
            if !status.success() {
                return Err(anyhow::anyhow!(
                    "Custom script failed with status {}",
                    status
                ));
            }
            return Ok(());
        }
    }

    Err(anyhow::anyhow!("Unknown command: {}", command_name))
}

fn check_managed(project_dir: &std::path::Path) {
    if !utils::is_managed(project_dir) {
        ui::critical(format!(
            "{:?} not managed by docker control plugin",
            project_dir
        ));
        std::process::exit(1);
    }
}

fn maybe_offer_image_pull(project_dir: &std::path::Path) {
    ui::info("Checking container images for updates...");
    let outdated: Vec<_> = docker::check_outdated_images(project_dir)
        .into_iter()
        .filter(|s| s.outdated)
        .collect();

    if outdated.is_empty() {
        return;
    }

    ui::warning("The following images appear to be outdated:");
    for status in &outdated {
        ui::warning(format!("  - {}", status.image));
    }

    if Confirm::new("Pull the latest images before starting?")
        .with_default(true)
        .prompt()
        .unwrap_or(false)
    {
        if let Err(e) = docker::execute_compose(project_dir, &["pull"]) {
            ui::warning(format!("Failed to pull images: {}", e));
        }
    }
}
