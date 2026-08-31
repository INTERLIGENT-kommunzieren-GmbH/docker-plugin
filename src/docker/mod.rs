use crate::ui;
use crate::utils::{platform, throttle_cache};
use anyhow::{Context, Result, anyhow};
use bollard::Docker;
use std::hash::{Hash, Hasher};
use std::io::IsTerminal;
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

/// Resolves `HOMEBREW_PREFIX`: env var → `brew --prefix` → `/usr/local` fallback.
pub fn resolve_brew_prefix() -> String {
    std::env::var("HOMEBREW_PREFIX")
        .ok()
        .or_else(platform::get_brew_prefix)
        .unwrap_or_else(|| "/usr/local".to_string())
}

/// Host path where the ingress companion writes the CA and per-domain certs.
pub fn ingress_tls_dir() -> PathBuf {
    PathBuf::from(resolve_brew_prefix())
        .join("etc")
        .join("docker-control")
        .join("ingress")
        .join("volumes")
        .join("tls")
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

    let brew_prefix = resolve_brew_prefix();

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

/// Minimum time between registry checks for a given project. The check hits
/// the network for every image, so it's throttled rather than run on every
/// start/restart.
const IMAGE_CHECK_INTERVAL: chrono::Duration = chrono::Duration::days(7);

/// Best-effort check of whether the images referenced by the project's compose
/// file are stale compared to the registry. Never fails: any error along the
/// way (offline, missing buildx, locally-built image, etc.) just causes that
/// image to be skipped rather than aborting the check.
pub fn check_outdated_images(project_dir: &Path) -> Vec<ImageStatus> {
    if std::env::var("DOCKER_CONTROL_SKIP_IMAGE_CHECK").is_ok() {
        return Vec::new();
    }

    let cache_path = image_check_cache_path(project_dir);

    if !throttle_cache::is_due(cache_path.as_deref(), IMAGE_CHECK_INTERVAL) {
        ui::debug("Skipping image update check (last checked within the past week)".to_string());
        return Vec::new();
    }

    let images = match list_compose_images(project_dir) {
        Ok(images) => images,
        Err(e) => {
            ui::debug(format!("Could not list images to check for updates: {}", e));
            return Vec::new();
        }
    };

    // Whether any image was actually checked against the registry, as
    // opposed to every image coming back `None` (offline, no buildx,
    // etc.). Only a check that reached the registry at least once counts
    // as "done" for throttling purposes — otherwise a single offline run
    // would silently suppress the real check for a week.
    let mut checked_any = false;
    let statuses: Vec<ImageStatus> = images
        .into_iter()
        .filter_map(|image| {
            let outdated = check_image_outdated(&image);
            checked_any |= outdated.is_some();
            outdated.map(|outdated| ImageStatus { image, outdated })
        })
        .collect();

    if checked_any {
        throttle_cache::record(cache_path.as_deref());
    }

    statuses
}

/// Cache file recording when a project's images were last checked against
/// the registry, so the (network-bound) check can be throttled to once a
/// week per project. Lives in the OS config dir, keyed by a hash of the
/// project's canonicalized path, since it's local machine state and
/// shouldn't be committed alongside the project.
fn image_check_cache_path(project_dir: &Path) -> Option<PathBuf> {
    let proj_dirs = directories::ProjectDirs::from("com", "interligent", "docker-control")?;
    let canonical = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    Some(
        proj_dirs
            .config_dir()
            .join("image-check-cache")
            .join(format!("{:016x}.json", hasher.finish())),
    )
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

/// Opens an interactive `bash` shell in `container` (default `php`), or lists the available
/// services when `container` is `help`. See [`console_exec`] for the one-shot variant.
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

/// The `docker compose exec` flags — everything between `exec` and the service name —
/// used by [`console_exec`].
///
/// `php` is special-cased the same way [`console`] special-cases it: the command runs as
/// `www-data` in the application mount. `COMPOSER_HOME` has to be passed explicitly for
/// the same reason [`exec_as_user`] documents — `/var/www/.bashrc` sets it, and only
/// interactive shells read that file, so the value the interactive [`console`] shell gets
/// would silently differ from a one-shot `exec`.
///
/// A TTY is allocated only when this process has one on both ends (compose allocates one
/// by default, hence `-T` to opt out): without that check, `dc2 console -- cat foo > out`
/// would write CRLF line endings into `out`.
fn console_exec_flags(service: &str, tty: bool) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();

    if !tty {
        flags.push("-T".to_string());
    }

    if service == "php" {
        flags.extend(["-u", "www-data"].map(String::from));
        flags.extend(["-w", "/var/www/html"].map(String::from));
        flags.extend(["-e", "COMPOSER_HOME=/var/www/.composer"].map(String::from));
    }

    flags
}

/// Runs `command` inside `service` instead of opening the interactive [`console`] shell,
/// and returns the command's own exit code so the caller can propagate it.
///
/// `command` is passed as argv, not through a shell, so a pipeline or a redirect has to be
/// spelled out explicitly (`console -- bash -lc '…'`) rather than being silently
/// misinterpreted here.
pub fn console_exec(
    project_dir: &Path,
    container: Option<String>,
    command: &[String],
) -> Result<i32> {
    let service = container.unwrap_or_else(|| "php".to_string());

    // `console help` lists the services rather than naming one, so there is nothing to run
    // a command in. Say so, instead of letting compose fail with "no such service: help".
    if service == "help" {
        return Err(anyhow!(
            "`help` lists the available services rather than naming one — \
             run `docker-control console help` on its own, or pass a real service name"
        ));
    }

    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let mut cmd = Command::new("docker");
    cmd.arg("compose")
        .arg("--project-directory")
        .arg(project_dir)
        .arg("exec")
        .args(console_exec_flags(&service, tty))
        .arg(&service)
        .args(command)
        .current_dir(project_dir);

    let status = cmd
        .status()
        .context("Failed to execute docker compose exec")?;

    // 130 mirrors the shell's convention for a command killed by SIGINT, which is what a
    // Ctrl-C'd container process reports as a signal rather than an exit code.
    Ok(status.code().unwrap_or(130))
}

/// The `docker compose exec` flags shared by [`exec_as_user`], [`exec_as_user_output`] and
/// [`exec_interactive`] — everything between `exec` and the service name.
///
/// `tty` false emits `-T`; compose allocates a pseudo-TTY by default, and every
/// non-interactive caller here needs it off so output stays pipe-clean.
fn exec_flags(user: &str, workdir: Option<&str>, env: &[(&str, &str)], tty: bool) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    if !tty {
        flags.push("-T".to_string());
    }
    flags.extend(["-u".to_string(), user.to_string()]);
    if let Some(dir) = workdir {
        flags.extend(["-w".to_string(), dir.to_string()]);
    }
    for (key, value) in env {
        flags.extend(["-e".to_string(), format!("{}={}", key, value)]);
    }
    flags
}

/// Like [`exec_as_user`], but with a TTY so the command can *prompt* — `composer init` asks
/// its questions this way. The TTY is allocated only when this process has one on both ends,
/// so a piped or redirected run still behaves like a plain non-interactive exec.
///
/// Returns the command's own exit code instead of an `anyhow::Error`, the pattern
/// [`console_exec`] established: a program that already explained its own failure
/// interactively should not also be reported as an `Error:` line by us.
pub fn exec_interactive(
    project_dir: &Path,
    service: &str,
    user: &str,
    workdir: Option<&str>,
    env: &[(&str, &str)],
    args: &[&str],
) -> Result<i32> {
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let status = Command::new("docker")
        .arg("compose")
        .arg("--project-directory")
        .arg(project_dir)
        .arg("exec")
        .args(exec_flags(user, workdir, env, tty))
        .arg(service)
        .args(args)
        .current_dir(project_dir)
        .status()
        .context(format!("Failed to execute docker compose exec ({})", user))?;

    Ok(status.code().unwrap_or(130))
}

pub fn exec_as_root(project_dir: &Path, service: &str, args: &[&str]) -> Result<()> {
    exec_as_user(project_dir, service, "root", None, &[], args)
}

/// Runs `args` in `service` as `user`, non-interactively, streaming stdout/stderr
/// to this process and failing on a non-zero exit status.
///
/// Unlike [`exec_as_user_output`] the output is inherited rather than captured, so
/// long-running commands (Composer, in particular) report progress as they go.
///
/// `workdir` sets `--workdir` and `env` adds repeated `--env KEY=VALUE` pairs.
/// Both matter for Composer: the image ships `COMPOSER_HOME=/composer`, and the
/// override to `/var/www/.composer` — where `config/composer.config.json` is
/// mounted — lives only in `/var/www/.bashrc`, so it applies to the interactive
/// [`console`] shell but *not* to a non-interactive `exec`. Without an explicit
/// `COMPOSER_HOME` the project's private repositories are invisible and the
/// `volumes/composer-cache` mount is bypassed.
pub fn exec_as_user(
    project_dir: &Path,
    service: &str,
    user: &str,
    workdir: Option<&str>,
    env: &[(&str, &str)],
    args: &[&str],
) -> Result<()> {
    let mut cmd = Command::new("docker");
    cmd.arg("compose")
        .arg("--project-directory")
        .arg(project_dir)
        .arg("exec")
        .args(exec_flags(user, workdir, env, false))
        .current_dir(project_dir)
        .arg(service)
        .args(args);

    let status = cmd
        .status()
        .context(format!("Failed to execute docker compose exec ({})", user))?;

    if !status.success() {
        return Err(anyhow!(
            "docker compose exec ({}) failed with status {}",
            user,
            status
        ));
    }

    Ok(())
}

/// Runs `args` in `service` as `user`, non-interactively, capturing stdout/stderr.
///
/// Unlike [`exec_as_root`], the caller inspects the captured [`Output`] itself
/// rather than the helper failing on a non-zero exit status — useful for probe
/// commands (e.g. `find`) whose exit code is not a reliable success signal.
pub fn exec_as_user_output(
    project_dir: &Path,
    service: &str,
    user: &str,
    args: &[&str],
) -> Result<std::process::Output> {
    Command::new("docker")
        .arg("compose")
        .arg("--project-directory")
        .arg(project_dir)
        .arg("exec")
        .args(exec_flags(user, None, &[], false))
        .current_dir(project_dir)
        .arg(service)
        .args(args)
        .output()
        .context("Failed to execute docker compose exec")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_exec_flags_match_the_interactive_shell_for_php() {
        // Same user as `console`, plus the workdir and COMPOSER_HOME that a
        // non-interactive exec doesn't inherit from /var/www/.bashrc.
        assert_eq!(
            console_exec_flags("php", false),
            vec![
                "-T",
                "-u",
                "www-data",
                "-w",
                "/var/www/html",
                "-e",
                "COMPOSER_HOME=/var/www/.composer",
            ]
        );
    }

    #[test]
    fn console_exec_flags_leave_other_services_alone() {
        // No -u/-w/-e: only the php service is special-cased, matching `console`.
        assert_eq!(console_exec_flags("db", false), vec!["-T"]);
    }

    #[test]
    fn exec_flags_match_the_previous_hand_built_argv() {
        // Pins what exec_as_user/exec_as_user_output emitted before the builder was factored out.
        assert_eq!(
            exec_flags("root", None, &[], false),
            vec!["-T", "-u", "root"]
        );
        assert_eq!(
            exec_flags(
                "www-data",
                Some("/var/www/html"),
                &[("COMPOSER_HOME", "/var/www/.composer")],
                false
            ),
            vec![
                "-T",
                "-u",
                "www-data",
                "-w",
                "/var/www/html",
                "-e",
                "COMPOSER_HOME=/var/www/.composer",
            ]
        );
    }

    #[test]
    fn exec_flags_drop_no_tty_when_a_tty_is_requested() {
        let flags = exec_flags("www-data", Some("/m"), &[], true);
        assert!(!flags.contains(&"-T".to_string()));
        assert_eq!(flags, vec!["-u", "www-data", "-w", "/m"]);
    }

    #[test]
    fn console_exec_rejects_the_help_pseudo_service() {
        // Would otherwise become `docker compose exec help <cmd>`.
        let err = console_exec(
            Path::new("."),
            Some("help".to_string()),
            &["ls".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("console help"), "unexpected message: {err}");
    }

    #[test]
    fn console_exec_flags_omit_no_tty_when_a_tty_is_available() {
        assert!(!console_exec_flags("php", true).contains(&"-T".to_string()));
        assert!(console_exec_flags("db", true).is_empty());
    }
}
