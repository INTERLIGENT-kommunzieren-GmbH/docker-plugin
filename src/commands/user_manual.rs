use crate::assets::AssetManager;
use crate::ui;
use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::Command;

/// Opens the bundled user manual PDF in the host's default PDF application.
///
/// The manual is resolved via [`AssetManager`]: an installed `share/` copy is used when
/// present, otherwise the copy embedded in the binary is written to the config dir first.
pub fn execute() -> Result<()> {
    let assets = AssetManager::new()?;
    let manual_path = assets.ensure_manual()?;

    ui::info(format!("Opening user manual: {:?}", manual_path));
    open_in_default_app(&manual_path)
}

/// Whether we're running under WSL, detected via `/proc/version` (cheap, no external
/// processes). Mirrors the check in `utils::platform::detect_platform`.
fn is_wsl() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let v = v.to_lowercase();
            v.contains("microsoft") || v.contains("wsl")
        })
        .unwrap_or(false)
}

/// Opens `path` in the platform's default application for its type.
///
/// - macOS: `open`
/// - Windows: `cmd /C start`
/// - WSL: the *Windows* default app (via `wslpath` + `cmd.exe /C start`, falling back to
///   `explorer.exe`), so the PDF opens in the user's Windows PDF viewer.
/// - other Linux: `xdg-open`
fn open_in_default_app(path: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        return run_opener("open", &[path.as_os_str()], path);
    }

    if cfg!(target_os = "windows") {
        // `start` treats the first quoted arg as a window title, hence the empty "".
        return run_opener(
            "cmd",
            &[
                "/C".as_ref(),
                "start".as_ref(),
                "".as_ref(),
                path.as_os_str(),
            ],
            path,
        );
    }

    if is_wsl() {
        return open_on_wsl(path);
    }

    run_opener("xdg-open", &[path.as_os_str()], path)
}

/// Opens `path` using the Windows default application from within WSL. Converts the Linux
/// path to a Windows path with `wslpath -w`, then launches it via `cmd.exe /C start`,
/// falling back to `explorer.exe` if that fails.
fn open_on_wsl(path: &Path) -> Result<()> {
    let win_path = Command::new("wslpath")
        .arg("-w")
        .arg(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "Could not translate {:?} to a Windows path via `wslpath`; is WSL interop enabled?",
                path
            )
        })?;

    // `cmd.exe /C start "" <winpath>` opens the file with the Windows default handler.
    let started = Command::new("cmd.exe")
        .args(["/C", "start", "", &win_path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if started {
        return Ok(());
    }

    // Fallback: explorer.exe also opens a file with its default handler. Its exit code is
    // unreliable (often non-zero even on success), so treat a successful spawn as success.
    Command::new("explorer.exe")
        .arg(&win_path)
        .status()
        .map(|_| ())
        .context("Failed to open the user manual via cmd.exe/explorer.exe on WSL")
}

/// Runs an opener command and maps a non-zero exit or spawn failure to a helpful error that
/// still tells the user where the file is, so they can open it manually.
fn run_opener(program: &str, args: &[&std::ffi::OsStr], path: &Path) -> Result<()> {
    let status = Command::new(program).args(args).status().with_context(|| {
        format!(
            "Failed to launch `{}` to open the user manual. Open it manually: {:?}",
            program, path
        )
    })?;

    if !status.success() {
        return Err(anyhow!(
            "`{}` exited with {} while opening the user manual. Open it manually: {:?}",
            program,
            status,
            path
        ));
    }

    Ok(())
}
