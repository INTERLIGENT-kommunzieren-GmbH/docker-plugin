use crate::ui;
use crate::utils::platform;
use anyhow::{Context, Result, anyhow};
use bollard::Docker;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn connect() -> Result<Docker> {
    match Docker::connect_with_local_defaults() {
        Ok(d) => Ok(d),
        Err(e) => {
            // Check for Docker Desktop for Mac per-user socket if default fails
            if cfg!(target_os = "macos")
                && let Some(mac_socket) = directories::BaseDirs::new()
                    .map(|b| b.home_dir().join(".docker/run/docker.sock"))
                    .filter(|p| p.exists())
            {
                let socket_path = format!("unix://{}", mac_socket.to_string_lossy());
                return Docker::connect_with_unix(&socket_path, 120, bollard::API_DEFAULT_VERSION)
                    .map_err(|e| anyhow!("Failed to connect to Docker on macOS: {}", e));
            }
            Err(anyhow!("Failed to connect to Docker: {}", e))
        }
    }
}

pub fn execute_compose(project_dir: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("docker");
    cmd.arg("compose")
        .arg("--project-directory")
        .arg(project_dir)
        .args(args)
        .current_dir(project_dir);

    let status = cmd.status().context("Failed to execute docker compose")?;

    if !status.success() {
        return Err(anyhow!("docker compose failed with status {}", status));
    }

    Ok(())
}

pub fn execute_ingress_compose(args: &[&str]) -> Result<()> {
    let ingress_dir = find_ingress_dir()?;
    let compose_file = ingress_dir.join("compose.yml");

    if !compose_file.exists() {
        return Err(anyhow!(
            "Ingress compose file not found at {:?}",
            compose_file
        ));
    }

    let brew_prefix = std::env::var("HOMEBREW_PREFIX")
        .ok()
        .or_else(platform::get_brew_prefix)
        .unwrap_or_else(|| "/usr/local".to_string());

    ui::debug(format!("Using HOMEBREW_PREFIX: {}", brew_prefix));

    // Ensure ingress volumes are up to date when starting
    if args.contains(&"up")
        && let Err(e) = ensure_ingress_volumes(&brew_prefix)
    {
        ui::warning(format!("Failed to ensure ingress volumes: {}", e));
    }

    let mut cmd = Command::new("docker");
    cmd.arg("compose")
        .arg("--project-directory")
        .arg(&ingress_dir)
        .arg("-f")
        .arg(&compose_file)
        .env("HOMEBREW_PREFIX", brew_prefix)
        .args(args);

    let status = cmd
        .status()
        .context("Failed to execute docker compose for ingress")?;

    if !status.success() {
        return Err(anyhow!(
            "docker compose ingress failed with status {}",
            status
        ));
    }

    Ok(())
}

fn ensure_ingress_volumes(brew_prefix: &str) -> Result<()> {
    let prefix = PathBuf::from(brew_prefix);

    // Source: prefix/share/docker-control/ingress/volumes
    let src = prefix
        .join("share")
        .join("docker-control")
        .join("ingress")
        .join("volumes");

    // Target: prefix/etc/docker-control/ingress/volumes
    let dst = prefix
        .join("etc")
        .join("docker-control")
        .join("ingress")
        .join("volumes");

    if src.exists() {
        ui::debug(format!(
            "Syncing ingress volumes from {:?} to {:?}",
            src, dst
        ));
        crate::utils::copy_dir_all(&src, &dst)?;
    } else {
        ui::debug(format!(
            "Source ingress volumes directory not found at {:?}",
            src
        ));
    }

    Ok(())
}

pub struct ImageStatus {
    pub image: String,
    pub outdated: bool,
}

/// Best-effort check of whether the images referenced by the project's compose
/// file are stale compared to the registry. Never fails: any error along the
/// way (offline, missing buildx, locally-built image, etc.) just causes that
/// image to be skipped rather than aborting the check.
pub fn check_outdated_images(project_dir: &Path) -> Vec<ImageStatus> {
    if std::env::var("DOCKER_CONTROL_SKIP_IMAGE_CHECK").is_ok() {
        return Vec::new();
    }

    let images = match list_compose_images(project_dir) {
        Ok(images) => images,
        Err(e) => {
            ui::debug(format!("Could not list images to check for updates: {}", e));
            return Vec::new();
        }
    };

    images
        .into_iter()
        .filter_map(|image| {
            check_image_outdated(&image).map(|outdated| ImageStatus { image, outdated })
        })
        .collect()
}

fn list_compose_images(project_dir: &Path) -> Result<Vec<String>> {
    let output = Command::new("docker")
        .arg("compose")
        .arg("--project-directory")
        .arg(project_dir)
        .arg("config")
        .arg("--images")
        .current_dir(project_dir)
        .output()
        .context("Failed to run docker compose config --images")?;

    if !output.status.success() {
        return Err(anyhow!(
            "docker compose config --images failed with status {}",
            output.status
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Returns `Some(true)` if outdated, `Some(false)` if up to date, or `None`
/// when the image can't be checked (locally-built image with no registry
/// digest, or the remote lookup failed/is unreachable).
fn check_image_outdated(image: &str) -> Option<bool> {
    let local_digest = get_local_repo_digest(image);
    let remote_digest = get_remote_digest(image);

    match (local_digest, remote_digest) {
        (LocalDigest::Missing, Some(_)) => Some(true),
        (LocalDigest::Present(local), Some(remote)) => Some(local != remote),
        _ => None,
    }
}

enum LocalDigest {
    /// Image not present locally at all.
    Missing,
    /// Image present with a registry digest.
    Present(String),
    /// Image present but with no registry digest (e.g. locally built).
    Unknown,
}

fn get_local_repo_digest(image: &str) -> LocalDigest {
    let output = Command::new("docker")
        .arg("image")
        .arg("inspect")
        .arg("--format")
        .arg("{{json .RepoDigests}}")
        .arg(image)
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return LocalDigest::Missing,
    };

    let digests: Vec<String> = match serde_json::from_slice(&output.stdout) {
        Ok(d) => d,
        Err(_) => return LocalDigest::Unknown,
    };

    digests
        .first()
        .and_then(|d| d.rsplit_once('@'))
        .map(|(_, digest)| LocalDigest::Present(digest.to_string()))
        .unwrap_or(LocalDigest::Unknown)
}

fn get_remote_digest(image: &str) -> Option<String> {
    let output = Command::new("docker")
        .arg("buildx")
        .arg("imagetools")
        .arg("inspect")
        .arg(image)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("Digest:"))
        .map(|s| s.trim().to_string())
}

pub fn is_running(project_dir: &Path) -> bool {
    let output = Command::new("docker")
        .arg("compose")
        .arg("--project-directory")
        .arg(project_dir)
        .arg("ps")
        .arg("--services")
        .arg("--filter")
        .arg("status=running")
        .current_dir(project_dir)
        .output();

    match output {
        Ok(out) if out.status.success() => !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        _ => false,
    }
}

pub fn console(project_dir: &Path, container: Option<String>) -> Result<()> {
    let service = container.unwrap_or_else(|| "php".to_string());

    if service == "help" {
        ui::info("Available containers:");
        let output = Command::new("docker")
            .arg("compose")
            .arg("--project-directory")
            .arg(project_dir)
            .arg("ps")
            .arg("--services")
            .current_dir(project_dir)
            .output()?;

        if output.status.success() {
            let services = String::from_utf8_lossy(&output.stdout);
            for s in services.lines() {
                ui::info(format!("  - {}", s));
            }
        }
        return Ok(());
    }

    let mut cmd = Command::new("docker");
    cmd.arg("compose")
        .arg("--project-directory")
        .arg(project_dir)
        .arg("exec")
        .current_dir(project_dir);

    if service == "php" {
        cmd.arg("-itu").arg("www-data");
    }

    cmd.arg(&service).arg("bash");

    let status = cmd
        .status()
        .context("Failed to execute docker compose exec")?;

    if !status.success() {
        // Fallback or retry? Original bash doesn't seem to have much fallback
        // but it might fail if bash is not available.
    }

    Ok(())
}

pub fn exec_as_root(project_dir: &Path, service: &str, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("docker");
    cmd.arg("compose")
        .arg("--project-directory")
        .arg(project_dir)
        .arg("exec")
        .arg("-T")
        .arg("-u")
        .arg("root")
        .current_dir(project_dir)
        .arg(service)
        .args(args);

    let status = cmd
        .status()
        .context("Failed to execute docker compose exec (root)")?;

    if !status.success() {
        return Err(anyhow!(
            "docker compose exec (root) failed with status {}",
            status
        ));
    }

    Ok(())
}

fn find_ingress_dir() -> Result<PathBuf> {
    // 1. Check environment variable
    if let Ok(env_path) = std::env::var("DOCKER_CONTROL_INGRESS_DIR") {
        let path = PathBuf::from(env_path);
        if path.exists() {
            return Ok(path);
        }
    }

    // 2. Check user config directory (AssetManager)
    if let Ok(asset_manager) = crate::assets::AssetManager::new() {
        let path = asset_manager.get_ingress_dir();
        if path.exists() {
            return Ok(path);
        }
    }

    // 3. Check relative to binary
    if let Ok(exe_path) = std::env::current_exe() {
        let real_exe_path = exe_path.canonicalize().unwrap_or(exe_path);
        if let Some(exe_dir) = real_exe_path.parent() {
            // Check for direct ingress/ folder
            let path = exe_dir.join("ingress");
            if path.exists() {
                return Ok(path);
            }
            // Check one level up (if binary is in a bin/ folder)
            if let Some(parent) = exe_dir.parent() {
                // Check parent/ingress
                let path = parent.join("ingress");
                if path.exists() {
                    return Ok(path);
                }
                // Check parent/share/docker-control/ingress (Homebrew standard)
                let path = parent.join("share").join("docker-control").join("ingress");
                if path.exists() {
                    return Ok(path);
                }
            }
        }
    }

    // 3. Check current directory (for development)
    let path = PathBuf::from("ingress");
    if path.exists() {
        return Ok(path);
    }

    Err(anyhow!("Could not find ingress directory"))
}
