use crate::ui;
use crate::utils::platform::PlatformInfo;
use anyhow::{Result, anyhow};
use bollard::query_parameters::ListContainersOptions;
use std::collections::HashMap;
use std::net::TcpStream as StdTcpStream;
use std::time::Duration;
use tokio::io::copy;
use tokio::net::{TcpListener, UnixStream};

pub async fn ensure_forwarding(info: &PlatformInfo) -> Result<()> {
    // 1. SSH Agent Forwarding
    ui::debug(format!(
        "Checking if port {}:{} is open",
        info.bind_ip,
        crate::SSH_AGENT_PORT
    ));
    if !is_port_open(&info.bind_ip, crate::SSH_AGENT_PORT) {
        ui::debug(format!(
            "Port {} is closed, attempting to start SSH agent forwarding",
            crate::SSH_AGENT_PORT
        ));
        if let Ok(ssh_auth_sock) = std::env::var("SSH_AUTH_SOCK") {
            ui::debug(format!("Using SSH_AUTH_SOCK: {}", ssh_auth_sock));
            ui::info(format!(
                "Starting SSH agent forwarding socket on {}:{}",
                info.bind_ip,
                crate::SSH_AGENT_PORT
            ));
            start_forwarding(crate::SSH_AGENT_PORT, &info.bind_ip, &ssh_auth_sock)?;

            // If we started a new forwarding, we might need to restart PHP containers
            if let Err(e) = restart_php_containers().await {
                ui::warning(format!("Failed to restart PHP containers: {}", e));
            }
        } else {
            ui::warning("SSH agent seems to not be running ($SSH_AUTH_SOCK is empty).");
        }
    } else {
        ui::debug(format!("Port {} is already open", crate::SSH_AGENT_PORT));
    }

    Ok(())
}

pub fn is_port_open(ip: &str, port: u16) -> bool {
    StdTcpStream::connect_timeout(
        &format!("{}:{}", ip, port).parse().unwrap(),
        Duration::from_millis(100),
    )
    .is_ok()
}

fn start_forwarding(port: u16, bind_ip: &str, unix_sock: &str) -> Result<()> {
    let bind_addr = format!("{}:{}", bind_ip, port);
    let target = unix_sock.to_string();

    tokio::spawn(async move {
        let listener = match TcpListener::bind(&bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                ui::critical(format!("Failed to bind to {}: {}", bind_addr, e));
                return;
            }
        };

        loop {
            match listener.accept().await {
                Ok((mut client_stream, _)) => {
                    let target = target.clone();
                    tokio::spawn(async move {
                        if std::env::consts::OS == "windows" && target.starts_with(r"\\.\pipe\") {
                            #[cfg(windows)]
                            {
                                use tokio::net::windows::named_pipe::ClientOptions;
                                match ClientOptions::new().open(&target) {
                                    Ok(mut server_stream) => {
                                        let (mut cr, mut cw) = client_stream.split();
                                        let (mut sr, mut sw) = tokio::io::split(server_stream);
                                        let _ = tokio::join!(
                                            copy(&mut cr, &mut sw),
                                            copy(&mut sr, &mut cw)
                                        );
                                    }
                                    Err(e) => ui::critical(format!(
                                        "Failed to connect to named pipe {}: {}",
                                        target, e
                                    )),
                                }
                            }
                            #[cfg(not(windows))]
                            {
                                ui::critical("Named pipes are only supported on Windows");
                            }
                        } else {
                            match UnixStream::connect(&target).await {
                                Ok(mut server_stream) => {
                                    let (mut cr, mut cw) = client_stream.split();
                                    let (mut sr, mut sw) = server_stream.split();
                                    let _ = tokio::join!(
                                        copy(&mut cr, &mut sw),
                                        copy(&mut sr, &mut cw)
                                    );
                                }
                                Err(e) => ui::critical(format!(
                                    "Failed to connect to unix socket {}: {}",
                                    target, e
                                )),
                            }
                        }
                    });
                }
                Err(e) => {
                    ui::critical(format!("Accept error: {}", e));
                }
            }
        }
    });

    // Give it a moment to start
    std::thread::sleep(Duration::from_millis(100));

    Ok(())
}

async fn restart_php_containers() -> Result<()> {
    let docker =
        crate::docker::connect().map_err(|e| anyhow!("Failed to connect to Docker: {}", e))?;

    let mut filters = HashMap::new();
    filters.insert(
        "label".to_string(),
        vec!["com.interligent.dockerplugin.service=php".to_string()],
    );

    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: false,
            filters: Some(filters),
            ..Default::default()
        }))
        .await
        .map_err(|e| anyhow!("Failed to list containers: {}", e))?;

    let mut projects = Vec::new();

    for container in containers {
        if let Some(id) = container.id {
            let inspect = docker.inspect_container(&id, None).await?;
            if let Some(config) = inspect.config
                && let Some(labels) = config.labels
            {
                let dir = labels
                    .get("com.interligent.dockerplugin.dir")
                    .or_else(|| labels.get("com.docker.compose.project.working_dir"));

                if let Some(dir) = dir
                    && !dir.is_empty()
                    && !projects.contains(dir)
                {
                    // Check if directory exists and has a compose file
                    let path = std::path::Path::new(dir);
                    if path.exists()
                        && (path.join("compose.yml").exists()
                            || path.join("docker-compose.yml").exists())
                    {
                        projects.push(dir.clone());
                    }
                }
            }
        }
    }

    if !projects.is_empty() {
        ui::info("restarting projects with PHP containers to connect to new ssh agent port");
        for project in projects {
            ui::info(format!("✓ {}", project));
            // Keep docker compose calls as requested
            let _ = std::process::Command::new("docker")
                .args(["compose", "--project-directory", &project, "down"])
                .current_dir(&project)
                .status();
            let _ = std::process::Command::new("docker")
                .args(["compose", "--project-directory", &project, "up", "-d"])
                .current_dir(&project)
                .status();
        }
    }

    Ok(())
}
