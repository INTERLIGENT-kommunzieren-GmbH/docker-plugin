#![allow(clippy::collapsible_if)]

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use daemonize::Daemonize;
use inquire::Confirm;
use std::io::IsTerminal;
use std::path::PathBuf;
use tokio::signal;

use docker_control::{SSH_AGENT_PORT, assets, commands, docker, template, ui, utils};

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
    /// Open a shell inside a container, or run a one-shot command in it after `--`
    Console {
        /// Container name (defaults to 'php')
        container: Option<String>,

        /// Command to run instead of opening a shell, e.g. `console -- composer install`
        // `last = true` confines this to values after a literal `--`, which is what keeps it
        // unambiguous against `container`: without it, `console ls -la` would have to guess
        // whether `ls` names a service or a command.
        #[arg(last = true, allow_hyphen_values = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Clean up old local backup_* folders created by update/migrate
    CleanupBackups {
        /// Number of most-recent backups to keep (default 5)
        #[arg(short, long, conflicts_with_all = ["older_than", "all"])]
        keep: Option<usize>,

        /// Delete backups older than this many days
        #[arg(long, conflicts_with_all = ["keep", "all"])]
        older_than: Option<u64>,

        /// Remove all backup folders
        #[arg(long, conflicts_with_all = ["keep", "older_than"])]
        all: bool,

        /// List backups that would be deleted without deleting them
        #[arg(long)]
        dry_run: bool,

        /// Skip interactive confirmation
        #[arg(short, long)]
        yes: bool,
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
    /// Check that /var/www is readable/writable by the container's www-data user
    Doctor {
        /// Repair any inaccessible paths (mkdir Composer/XDG homes + re-apply ACL)
        #[arg(long)]
        fix: bool,
    },
    /// Initialize an empty directory with the project template
    Init,
    /// Install Claude Code using Anthropic's official installer
    InstallClaude,
    /// Install all Homebrew-installable dependencies, including optional ones
    InstallDeps,
    /// Merge release branch to main using selective cherry-pick workflow
    Merge {
        /// Optional module name
        module: Option<String>,
    },
    /// Manage vendor modules checked out for local development
    Module {
        #[command(subcommand)]
        action: commands::module::ModuleAction,
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
    /// Trust the ingress CA certificate on this host
    TrustCa,
    /// Migrate from old docker-control project
    #[command(hide = true)]
    Migrate,
    /// Update the project with the current template
    Update {
        /// Skip the confirmation prompt (required for non-interactive use)
        #[arg(short, long)]
        yes: bool,
        /// Report pending template changes and exit without modifying anything
        /// (exits non-zero when changes are pending)
        #[arg(long)]
        check: bool,
        /// Overwrite every template-owned file, ignoring local modifications
        #[arg(long)]
        force_template: bool,
    },
    /// Upgrade docker-control itself via Homebrew
    Upgrade,
    /// Open the user manual (PDF) in your default PDF application
    UserManual,
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
    let raw_args: Vec<String> = std::env::args().collect();
    // Only the part of argv before a standalone `--` is docker-control's own; the rest
    // belongs to the command `console -- <cmd>` runs in the container (or to a custom
    // script). Scanning all of argv here would let `console -- foo --stop-ssh-agent`
    // stop the agent instead of passing the flag along to `foo`.
    let args = commands::custom::args_before_separator(&raw_args);

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
    let no_ssh_needed = args.iter().any(|a: &String| {
        a == "--help"
            || a == "-h"
            || a == "--version"
            || a == "-V"
            || a == "docker-cli-plugin-metadata"
            || a == "upgrade"
            || a == "install-claude"
            || a == "install-deps"
            || a == "user-manual"
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
    // Docker-control's own flags are only the ones before a standalone `--` — see
    // [`commands::custom::args_before_separator`]. `args` itself stays intact, because the
    // custom-script dispatch below has to forward everything the user typed.
    let own_args = commands::custom::args_before_separator(&args);

    // Determine whether help was requested at the *top level* (the custom project-status
    // help) versus for a specific subcommand (`<command> --help`, handled by clap for
    // built-ins or by the custom-script `_help_` hook further below). Only a leading
    // `help`/`--help`/`-h` — before any subcommand token — counts as top-level, so
    // `docker control deploy --help` shows deploy's help rather than the global one.
    let leading_token =
        commands::custom::split_leading_subcommand(&args, &global_flag_tokens(&Cli::command()))
            .map(|(token, _)| token);
    let is_help = matches!(leading_token.as_deref(), Some("help" | "--help" | "-h"));

    // `--version`/`-V` counts only as the leading token, for the same reason as `is_help`
    // above: anywhere else it belongs to a subcommand, not to docker-control. Scanning all
    // of argv swallowed both `console -- php --version` (PHP's flag) and
    // `module link <m> --version <v>` (that subcommand's own argument), printing
    // docker-control's version and exiting instead of running the command.
    if matches!(leading_token.as_deref(), Some("--version" | "-V")) {
        println!("docker-control {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Manually parse dir for metadata and help
    let mut project_dir = std::env::current_dir().expect("Failed to get current directory");
    for i in 0..own_args.len() {
        if (own_args[i] == "--dir" || own_args[i] == "-d") && i + 1 < own_args.len() {
            project_dir = PathBuf::from(&own_args[i + 1]);
            break;
        }
        if let Some(value) = own_args[i]
            .strip_prefix("--dir=")
            .or_else(|| own_args[i].strip_prefix("-d="))
        {
            project_dir = PathBuf::from(value);
            break;
        }
    }

    let project_dir = if project_dir.exists() {
        project_dir.canonicalize().unwrap_or(project_dir)
    } else {
        project_dir
    };

    // Return early if metadata is requested, no dependencies needed
    if own_args
        .iter()
        .any(|arg| arg == "docker-cli-plugin-metadata")
    {
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

    // Apply --debug, dependency checks, and asset init before the clash-resolution
    // block below (which can dispatch a custom script and return early) so every path
    // gets the same setup a normal built-in dispatch gets.
    if own_args.iter().any(|arg| arg == "--debug") {
        ui::set_debug(true);
    }

    // `install-deps` exists precisely to install missing dependencies, so it must not be
    // gated behind the dependency check that would abort on the very deps it installs.
    // `user-manual` just opens a PDF and needs none of the external tools, so don't force
    // a full dependency check (e.g. Docker) on it either.
    let skip_dependency_check = own_args
        .iter()
        .any(|a| a == "install-claude" || a == "install-deps" || a == "user-manual");
    if !skip_dependency_check && std::env::var("DOCKER_CONTROL_SKIP_DEPENDENCY_CHECK").is_err() {
        utils::dependencies::check_dependencies()?;
    }

    if let Ok(asset_manager) = assets::AssetManager::new() {
        if let Err(e) = asset_manager.ensure_assets() {
            ui::warning(format!(
                "Failed to ensure assets: {}. Falling back to local/env paths.",
                e
            ));
        }
    }

    // Built once and reused below for the clash check and the real parse, instead of
    // re-deriving the whole subcommand/arg tree from scratch multiple times.
    let cmd = Cli::command().styles(get_help_styles());

    // Resolve a name clash between a built-in command and a same-named custom script
    // BEFORE clap validates the built-in's own argument schema, so the clash check
    // isn't blocked by a strict built-in (e.g. `Deploy`) rejecting args that were actually
    // meant for the custom script. Only `Build`, and `Console` after a `--`, accept
    // arbitrary extra args themselves, so waiting until after parsing would make this
    // feature unusable for every other built-in whenever extra/unrecognized args are passed.
    if let Some((subcommand_name, trailing_args)) =
        commands::custom::split_leading_subcommand(&args, &global_flag_tokens(&cmd))
    {
        // Resolve to the subcommand's canonical name so an alias is gated the same way
        // invoking it by its real name would be.
        let canonical_name = cmd
            .get_subcommands()
            .find(|sub| {
                sub.get_name() == subcommand_name
                    || sub.get_all_aliases().any(|alias| alias == subcommand_name)
            })
            .map(|sub| sub.get_name().to_string());

        if let Some(canonical_name) = canonical_name {
            if let Some(script_path) = commands::custom::resolve_clash(
                &project_dir,
                &subcommand_name,
                &commands::custom::InteractiveClashPromptProvider,
            )? {
                // Mirror the managed-project gate the built-in itself would have
                // enforced; keep this list in sync with the `check_managed` calls in
                // the match arms below.
                if command_requires_managed_project(&canonical_name) {
                    check_managed(&project_dir);
                }
                // `<command> --help` shows the script's own help via its `_help_` hook
                // instead of executing it.
                if commands::custom::wants_help(&trailing_args) {
                    return commands::custom::print_help(
                        &project_dir,
                        &subcommand_name,
                        &script_path,
                    );
                }
                ui::info(format!("Executing custom script: {:?}", script_path));
                commands::custom::run_script(&project_dir, &script_path, &trailing_args)?;
                return Ok(());
            }
        }
    }

    // Runs after the custom-script clash resolution above (which can dispatch a
    // custom script and return early) so a self-upgrade prompt never interrupts
    // running a custom script.
    maybe_offer_self_upgrade();

    // `e.exit()` renders clap's own output: for a subcommand `--help`/`--version` it prints
    // the generated help/version page to stdout and exits 0; for a genuine parse error it
    // prints the formatted error to stderr and exits 2. Using `?` here would instead wrap
    // the help text in an "Error:" and exit non-zero, so handle it explicitly.
    let matches = cmd.try_get_matches().unwrap_or_else(|e| e.exit());
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

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
        Commands::Console { container, command } => {
            check_managed(&project_dir);
            if command.is_empty() {
                docker::console(&project_dir, container)?;
            } else {
                // Exit with the container command's own status rather than returning Ok, so
                // `dc2 console -- <cmd>` is usable in a script or an `&&` chain. Bypassing
                // the `Err` path also keeps the output free of an "Error:" line the inner
                // command already explained itself.
                let code = docker::console_exec(&project_dir, container, &command)?;
                std::process::exit(code);
            }
        }
        Commands::CleanupBackups {
            keep,
            older_than,
            all,
            dry_run,
            yes,
        } => {
            commands::cleanup_backups::execute(&project_dir, keep, older_than, all, yes, dry_run)?;
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
        Commands::Doctor { fix } => {
            check_managed(&project_dir);
            commands::doctor::execute(&project_dir, fix)?;
        }
        Commands::Init => {
            commands::init::execute(&project_dir).await?;
        }
        Commands::InstallClaude => {
            commands::install_claude::execute()?;
        }
        Commands::InstallDeps => {
            commands::install_deps::execute()?;
        }
        Commands::Merge { module } => {
            commands::merge::execute(
                &project_dir,
                module,
                commands::merge::MergeOptions::default(),
            )?;
        }
        Commands::Module { action } => {
            check_managed(&project_dir);
            commands::module::execute(
                &project_dir,
                action,
                commands::module::ModuleOptions::default(),
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
            utils::dependencies::require_acl_tools()?;
            maybe_report_template_drift(&project_dir);
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
            utils::dependencies::require_acl_tools()?;
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
            utils::dependencies::require_acl_tools()?;
            maybe_report_template_drift(&project_dir);
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
        Commands::TrustCa => {
            commands::trust_ca::execute()?;
        }
        Commands::Migrate => {
            commands::migrate::execute(&project_dir).await?;
        }
        Commands::Update {
            yes,
            check,
            force_template,
        } => {
            check_managed(&project_dir);
            commands::update::execute(
                &project_dir,
                commands::update::UpdateOptions {
                    yes,
                    check,
                    force_template,
                },
            )?;
        }
        Commands::Upgrade => {
            commands::upgrade::execute()?;
        }
        Commands::UserManual => {
            commands::user_manual::execute()?;
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

    match commands::custom::find_script_path(project_dir, command_name) {
        Some(path) => {
            // `<command> --help` shows the script's own help via its `_help_` hook.
            if commands::custom::wants_help(command_args) {
                return commands::custom::print_help(project_dir, command_name, &path);
            }
            ui::info(format!("Executing custom script: {:?}", path));
            commands::custom::run_script(project_dir, &path, command_args)
        }
        None => Err(anyhow::anyhow!("Unknown command: {}", command_name)),
    }
}

/// The clap-registered names of built-ins whose match arm calls `check_managed`. Used
/// to apply the same gate when a custom script wins a name clash against one of them.
fn command_requires_managed_project(name: &str) -> bool {
    matches!(
        name,
        "build"
            | "console"
            | "doctor"
            | "module"
            | "pull"
            | "setacl"
            | "start"
            | "stop"
            | "restart"
            | "update"
    )
}

/// The long/short flag tokens for every `global = true` argument on `Cli`, derived
/// from clap itself so this can't silently drift out of sync with the `Cli` struct.
fn global_flag_tokens(cmd: &clap::Command) -> Vec<String> {
    cmd.get_arguments()
        .filter(|arg| arg.is_global_set())
        .flat_map(|arg| {
            let mut tokens = Vec::new();
            if let Some(long) = arg.get_long() {
                tokens.push(format!("--{long}"));
            }
            if let Some(short) = arg.get_short() {
                tokens.push(format!("-{short}"));
            }
            tokens
        })
        .collect()
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

/// Best-effort, throttled-to-weekly check of whether docker-control itself
/// has a newer Homebrew release available, offering to run the existing
/// `upgrade` command. No-op when stdin isn't a terminal, so unattended/CI
/// runs never block on a prompt.
fn maybe_offer_self_upgrade() {
    maybe_offer_self_upgrade_with(&commands::upgrade::InteractiveUpgradePromptProvider)
}

fn maybe_offer_self_upgrade_with(prompt: &dyn commands::upgrade::UpgradePromptProvider) {
    if !std::io::stdin().is_terminal() {
        return;
    }

    let Some(true) = commands::upgrade::check_outdated() else {
        return;
    };

    ui::warning("A newer version of docker-control is available.");
    if prompt.confirm_upgrade() {
        if let Err(e) = commands::upgrade::execute() {
            ui::warning(format!("Failed to upgrade docker-control: {}", e));
        }
    }
}

/// Tells the user when the project template has actually moved since this
/// project last synced, and stays quiet otherwise. Best-effort: a failure to
/// read the template or the project's state must never block the command.
///
/// Not throttled, unlike the self-update and image checks — those shell out to
/// `brew` or hit a registry, whereas this is a local hash comparison whose fast
/// path is a single string compare.
///
/// This deliberately does not live in `upgrade`: that command shells out to
/// `brew upgrade`, so the still-running process holds the *old* embedded
/// template and would compare against stale bytes. The notice lands on the next
/// invocation instead.
fn maybe_report_template_drift(project_dir: &std::path::Path) {
    let Ok(template_dir) = template::resolve_dir() else {
        return;
    };

    let changes = match template::diff(project_dir, &template_dir) {
        Ok(changes) => changes,
        Err(e) => {
            ui::debug(format!("Skipping template drift check: {}", e));
            return;
        }
    };

    let summary = template::Summary::from_changes(&changes);
    // `unknown` alone is not worth a notice: a project predating the state file
    // can't clear those without running `update`, so it would never go away.
    // Everything else that `update` can act on belongs here.
    if summary.safe.is_empty()
        && summary.conflicts.is_empty()
        && summary.removed.is_empty()
        && summary.env_keys.is_empty()
        && summary.gitignore_entries.is_empty()
    {
        return;
    }

    // `template_synced_at`, not `initialized_with`: the last sync is the version
    // the comparison is actually against. `initialized_with` is frozen at `init`,
    // so using it would name a version whose changes were long since applied.
    let since = template::TemplateState::load(project_dir)
        .map(|s| s.template_synced_at)
        .unwrap_or_else(|| "an untracked version".to_string());

    // Naming both versions is only informative when they differ; a template
    // edited without a version bump would otherwise read "since X (now X)".
    let current = env!("CARGO_PKG_VERSION");
    ui::warning(if since == current {
        format!(
            "The project template has changed since your last sync ({}).",
            since
        )
    } else {
        format!(
            "The project template has changed since {} (now {}).",
            since, current
        )
    });
    summary.print(true);
    ui::info("  Run `docker control update` to apply, or `update --check` to see the diffs.");
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
