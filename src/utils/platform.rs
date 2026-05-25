use crate::ui;
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum Platform {
    Macos,
    Wsl,
    DockerDesktop,
    NativeLinux(Option<String>),
    Windows,
    Unknown,
}

pub struct PlatformInfo {
    pub platform: Platform,
    pub bind_ip: String,
}

pub fn detect_platform() -> PlatformInfo {
    let os = std::env::consts::OS;
    ui::debug(format!("OS detected: {}", os));

    if os == "macos" {
        ui::debug("Detected macOS platform");
        return PlatformInfo {
            platform: Platform::Macos,
            bind_ip: "127.0.0.1".to_string(),
        };
    }

    if os == "linux" {
        // Check for WSL
        if let Ok(version) = std::fs::read_to_string("/proc/version")
            && (version.to_lowercase().contains("microsoft")
                || version.to_lowercase().contains("wsl"))
        {
            ui::debug("Detected WSL platform");
            return PlatformInfo {
                platform: Platform::Wsl,
                bind_ip: "127.0.0.1".to_string(),
            };
        }

        // Check for Docker Desktop
        if let Ok(output) = Command::new("docker").arg("info").output() {
            let info = String::from_utf8_lossy(&output.stdout).to_lowercase();
            if info.contains("docker desktop")
                || info.contains("desktop-linux")
                || info.contains("com.docker.desktop")
            {
                ui::debug("Detected Docker Desktop platform");
                return PlatformInfo {
                    platform: Platform::DockerDesktop,
                    bind_ip: "127.0.0.1".to_string(),
                };
            }
        }

        // Native Linux - try to get docker0 IP
        let docker0_ip = get_docker0_ip();
        let bind_ip = docker0_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());

        ui::debug(format!(
            "Detected Native Linux platform, bind_ip: {}",
            bind_ip
        ));
        return PlatformInfo {
            platform: Platform::NativeLinux(docker0_ip),
            bind_ip,
        };
    }

    if os == "windows" {
        // On Windows, check for Docker Desktop
        if let Ok(output) = Command::new("docker").arg("info").output() {
            let info = String::from_utf8_lossy(&output.stdout).to_lowercase();
            if info.contains("docker desktop")
                || info.contains("windows-linux")
                || info.contains("com.docker.desktop")
            {
                return PlatformInfo {
                    platform: Platform::DockerDesktop,
                    bind_ip: "127.0.0.1".to_string(),
                };
            }
        }

        return PlatformInfo {
            platform: Platform::Windows,
            bind_ip: "127.0.0.1".to_string(),
        };
    }

    PlatformInfo {
        platform: Platform::Unknown,
        bind_ip: "localhost".to_string(),
    }
}

fn get_docker0_ip() -> Option<String> {
    if let Ok(interfaces) = get_if_addrs::get_if_addrs() {
        for interface in interfaces {
            if interface.name == "docker0"
                && let get_if_addrs::IfAddr::V4(addr) = interface.addr
            {
                let ip = addr.ip.to_string();
                if ip != "127.0.0.1" {
                    return Some(ip);
                }
            }
        }
    }

    None
}

pub fn get_brew_prefix() -> Option<String> {
    if let Ok(output) = Command::new("brew").arg("--prefix").output()
        && output.status.success()
    {
        let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !prefix.is_empty() {
            return Some(prefix);
        }
    }
    None
}
