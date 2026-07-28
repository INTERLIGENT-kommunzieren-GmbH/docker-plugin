use crate::assets::AssetManager;
use crate::ui;
use crate::utils;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

/// Canonical `build/capistrano/Dockerfile` shipped with this tool. Written during
/// migration (overriding whatever was copied from the backup) and refreshed by the
/// `update` command whenever a project already has a Capistrano build context, so
/// every managed project converges on this single definition.
pub const CAPISTRANO_DOCKERFILE: &str = r#"FROM ruby:3.3.8-slim

WORKDIR /app

RUN apt update && \
    apt install -y \
      openssh-client \
      git \
      bash \
      keychain \
      procps \
      curl \
      build-essential \
      ca-certificates \
    && update-ca-certificates

RUN gem install --no-document \
      capistrano:3.19 \
      ed25519:1.4 \
      bcrypt_pbkdf:1.1

RUN mkdir -p /root/.ssh && chmod 744 /root/.ssh

SHELL ["/bin/bash", "-c"]

RUN echo $'\n\
export PATH="/usr/local/bundle/bin:$PATH" \n\
if [ -e "/ssh/id_rsa.pub" ]; then \n\
    cp /ssh/id_rsa.pub ~/.ssh/id_rsa.pub \n\
    chmod 644 /root/.ssh/id_rsa.pub \n\
fi \n\
if [ -e "/ssh/id_rsa" ]; then \n\
    cp /ssh/id_rsa ~/.ssh/id_rsa \n\
    chmod 600 ~/.ssh/id_rsa \n\
    eval $(keychain --eval id_rsa) \n\
fi' >> /root/.profile
"#;

/// Refreshes an existing `<capistrano_build_dir>/Dockerfile` with
/// [`CAPISTRANO_DOCKERFILE`]. Does nothing if the file does not already exist —
/// Capistrano is opt-in per project, so this never creates one. Returns `true` only
/// when an existing file's contents actually changed, so callers can rebuild the
/// image just in that case.
pub fn write_capistrano_dockerfile(capistrano_build_dir: &Path) -> Result<bool> {
    let dockerfile = capistrano_build_dir.join("Dockerfile");
    if !dockerfile.exists() {
        return Ok(false);
    }
    let unchanged = fs::read_to_string(&dockerfile)
        .map(|existing| existing == CAPISTRANO_DOCKERFILE)
        .unwrap_or(false);
    if unchanged {
        return Ok(false);
    }
    fs::write(&dockerfile, CAPISTRANO_DOCKERFILE)?;
    Ok(true)
}

pub async fn execute(project_dir: &Path) -> Result<()> {
    ui::info("Migrating old docker-control project to current version...");

    let control_cmd = project_dir.join("control.cmd");
    if !control_cmd.exists() {
        return Err(anyhow!(
            "control.cmd not found in project directory. Migration aborted."
        ));
    }

    // 1. Stop the project using "control.cmd stop"
    ui::info("Stopping project using control.cmd stop...");
    let status = Command::new("bash")
        .arg("./control.cmd")
        .arg("stop")
        .current_dir(project_dir)
        .status()
        .context("Failed to execute control.cmd stop")?;

    if !status.success() {
        return Err(anyhow!("control.cmd stop failed. Migration aborted."));
    }

    // 2. Move project to backup subfolder using rsync with sudo
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    let backup_name = format!("backup_{}", now);
    let backup_dir = project_dir.join(&backup_name);

    ui::info(format!("Creating backup in {}...", backup_name));
    fs::create_dir_all(&backup_dir)?;
    utils::exclude_from_phpstorm(project_dir, &backup_name)?;

    // We use rsync with sudo to keep permissions and owner info
    // Excluding the backup directory itself and .git
    let status = Command::new("sudo")
        .arg("rsync")
        .arg("-a")
        .arg("--exclude")
        .arg(&backup_name)
        .arg("./")
        .arg(format!("{}/", backup_name))
        .current_dir(project_dir)
        .status()
        .context("Failed to execute rsync backup")?;

    if !status.success() {
        return Err(anyhow!("Backup failed with rsync."));
    }

    // 2.1 Empty the directory after backup (keeping backup folder)
    ui::info("Emptying project directory (preserving backup)...");
    for entry in fs::read_dir(project_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();

        if file_name == backup_name.as_str() {
            continue;
        }

        let status = Command::new("sudo")
            .arg("rm")
            .arg("-rf")
            .arg(&file_name)
            .current_dir(project_dir)
            .status()
            .context(format!("Failed to remove {:?}", file_name))?;

        if !status.success() {
            ui::warning(format!("Failed to remove {:?}", file_name));
        }
    }

    // 3. Copy template folder contents (including volumes/ and build/)
    ui::info("Applying new template...");
    let asset_manager = AssetManager::new()?;
    asset_manager.ensure_assets()?;
    let template_dir = asset_manager.get_template_dir();

    let status = Command::new("rsync")
        .arg("-a")
        .arg(format!("{}/", template_dir.to_string_lossy()))
        .arg("./")
        .current_dir(project_dir)
        .status()
        .context("Failed to apply template")?;

    if !status.success() {
        return Err(anyhow!("Failed to apply template."));
    }

    // 4. Restore folders/files from backup if present.
    //
    // Only items the template does *not* own belong here: `htdocs` is a separate repo,
    // `.idea`/`.claude` are user-owned and absent from the template. The project-root
    // `CLAUDE.md` is deliberately NOT restored — it ships with the template (like
    // `compose.yml`), so restoring it would shadow the updated copy and leave migrated
    // projects on a stale version forever. Project-specific instructions and memory
    // belong in `htdocs/CLAUDE.md`, which is preserved as part of `htdocs`. The previous
    // root copy is still recoverable from the backup folder.
    for item in ["htdocs", ".idea", ".claude"] {
        let backup_item = project_dir.join(&backup_name).join(item);
        if backup_item.exists() {
            ui::info(format!("Restoring {}...", item));
            let (src, dst) = if backup_item.is_dir() {
                (format!("{}/{}/", backup_name, item), format!("{}/", item))
            } else {
                (format!("{}/{}", backup_name, item), item.to_string())
            };
            let status = Command::new("sudo")
                .arg("rsync")
                .arg("-a")
                .arg(&src)
                .arg(&dst)
                .current_dir(project_dir)
                .status()
                .context(format!("Failed to restore {}", item))?;

            if !status.success() {
                ui::warning(format!("Failed to restore {}.", item));
            }
        }
    }

    // 5. Add compose.override.yml with capistrano service extracted from backup
    ui::info("Migrating Capistrano configuration...");
    if let Err(e) = migrate_capistrano(project_dir, &backup_name) {
        ui::warning(format!("Capistrano migration failed: {}", e));
    }

    // 6. Copy mariadb volume from backup container/mariadb to project volumes/db
    ui::info("Migrating database volumes...");
    let status = Command::new("sudo")
        .arg("rsync")
        .arg("-a")
        .arg(format!("{}/container/mariadb/", backup_name))
        .arg("volumes/db/")
        .current_dir(project_dir)
        .status()
        .context("Failed to migrate database volumes")?;

    if !status.success() {
        ui::warning("Failed to migrate database volumes from container/mariadb.");
    }

    // 7. Add cap.sh control script
    ui::info("Adding cap.sh control script...");
    let cap_sh_content = r#"#!/bin/bash
if [[ "$1" == "_desc_" ]]; then
    echo "Old capistrano deployment command"
    exit 0
fi

DC="docker compose --project-directory ${PROJECT_DIR:-.}"

if [ "$#" -le 1 ]; then
  # one or zero params -> run build verbosely
  $DC build capistrano
else
  # more than one param -> run build quietly
  $DC build -q capistrano
fi

$DC run --rm capistrano bash -l -i -c "cap $*"
"#;
    let cap_sh_path = project_dir.join("control-scripts/cap.sh");
    if let Some(parent) = cap_sh_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&cap_sh_path, cap_sh_content)?;

    // Make it executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&cap_sh_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&cap_sh_path, perms)?;
    }

    // 8. Copy .env-dist to .env and use values from backup .env
    ui::info("Restoring environment variables...");
    if let Err(e) = restore_env(project_dir, &backup_name) {
        ui::warning(format!("Environment restoration failed: {}", e));
    }

    ui::success("Migration completed successfully!");
    ui::info("Please review your .env and compose.override.yml files.");

    Ok(())
}

fn migrate_capistrano(project_dir: &Path, backup_name: &str) -> Result<()> {
    let dev_compose_path = project_dir
        .join(backup_name)
        .join("docker-compose/docker-compose.development.yml");
    if !dev_compose_path.exists() {
        ui::info("Old docker-compose.development.yml not found, skipping Capistrano migration.");
        return Ok(());
    }

    let content = fs::read_to_string(&dev_compose_path)?;
    let lines: Vec<&str> = content.lines().collect();

    let mut final_cap_block = Vec::new();
    let mut in_cap_service = false;

    for line in lines {
        if line.trim() == "capistrano:" {
            in_cap_service = true;
            final_cap_block.push(line.to_string());
            continue;
        }

        if in_cap_service {
            // Check if we hit another service or end of services
            // Usually services are indented by 2 spaces
            if !line.is_empty() && !line.starts_with("  ") && !line.starts_with("   ") {
                in_cap_service = false;
                continue;
            }
            // Also check for another service at same indentation
            if !line.is_empty()
                && line.starts_with("  ")
                && !line.starts_with("    ")
                && line.ends_with(':')
            {
                in_cap_service = false;
                continue;
            }

            let mut processed_line = line.to_string();
            // Perform replacements as requested
            processed_line =
                processed_line.replace("./container/capistrano/", "./volumes/capistrano/");
            processed_line =
                processed_line.replace("docker-compose/build/capistrano", "build/capistrano");

            final_cap_block.push(processed_line);
        }
    }

    if !final_cap_block.is_empty() {
        let mut override_content = String::from("services:\n");
        for line in final_cap_block {
            override_content.push_str(&line);
            override_content.push('\n');
        }
        fs::write(project_dir.join("compose.override.yml"), override_content)?;

        // Copy context from backup build/capistrano
        ui::info("Copying Capistrano build context...");
        let _ = Command::new("sudo")
            .arg("rsync")
            .arg("-a")
            .arg(format!("{}/docker-compose/build/capistrano/", backup_name))
            .arg("build/capistrano/")
            .current_dir(project_dir)
            .status();

        // Refresh the copied Dockerfile with the current canonical version so
        // migrated projects don't carry forward an outdated build definition, and
        // rebuild the image only if that actually changed the file.
        if write_capistrano_dockerfile(&project_dir.join("build/capistrano"))? {
            ui::info("Rebuilding Capistrano image (Dockerfile changed)...");
            let _ = crate::docker::execute_compose(project_dir, &["build", "capistrano"]);
        }

        // Copy configuration from backup container/capistrano
        ui::info("Copying Capistrano configuration...");
        let _ = Command::new("sudo")
            .arg("rsync")
            .arg("-a")
            .arg(format!("{}/container/capistrano/", backup_name))
            .arg("volumes/capistrano/")
            .current_dir(project_dir)
            .status();
    }

    Ok(())
}

fn restore_env(project_dir: &Path, backup_name: &str) -> Result<()> {
    let env_dist = project_dir.join(".env-dist");
    let backup_env = project_dir.join(backup_name).join(".env");

    if !env_dist.exists() || !backup_env.exists() {
        ui::info("Either .env-dist or backup .env not found, skipping environment restoration.");
        return Ok(());
    }

    let dist_content = fs::read_to_string(&env_dist)?;
    let backup_content = fs::read_to_string(&backup_env)?;

    let mut backup_vars = std::collections::HashMap::new();
    for line in backup_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            backup_vars.insert(key.trim(), value.trim());
        }
    }

    if !backup_vars.contains_key("DB_HOST_PORT")
        && let Some(port) = backup_vars
            .get("MARIADB_PORT")
            .or(backup_vars.get("MYSQL_PORT"))
    {
        backup_vars.insert("DB_HOST_PORT", port);
    }

    let mut new_env_content = String::new();
    for line in dist_content.lines() {
        let trimmed = line.trim();
        if trimmed.contains('=')
            && !trimmed.starts_with('#')
            && let Some((key, _)) = trimmed.split_once('=')
        {
            let key = key.trim();
            if let Some(value) = backup_vars.get(key) {
                new_env_content.push_str(&format!("{}={}\n", key, value));
                continue;
            }
        }
        new_env_content.push_str(line);
        new_env_content.push('\n');
    }

    fs::write(project_dir.join(".env"), new_env_content)?;

    Ok(())
}
