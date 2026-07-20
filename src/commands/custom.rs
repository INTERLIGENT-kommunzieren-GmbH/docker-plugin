use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
}

/// Locates a custom script for `name`, checking htdocs before the project root and
/// trying both the bare name and `<name>.sh`. Shared by dispatch and clash detection.
pub fn find_script_path(project_dir: &Path, name: &str) -> Option<PathBuf> {
    let mut candidates = vec![
        project_dir.join(format!("htdocs/.docker-control/control-scripts/{}", name)),
        project_dir.join(format!("control-scripts/{}", name)),
    ];

    if !name.ends_with(".sh") {
        candidates.push(project_dir.join(format!(
            "htdocs/.docker-control/control-scripts/{}.sh",
            name
        )));
        candidates.push(project_dir.join(format!("control-scripts/{}.sh", name)));
    }

    candidates.into_iter().find(|p| p.exists())
}

/// Runs a resolved custom script with the given arguments.
pub fn run_script(project_dir: &Path, script_path: &Path, args: &[String]) -> Result<()> {
    let status = Command::new("bash")
        .arg(script_path)
        .args(args)
        .current_dir(project_dir)
        .env("PROJECT_DIR", project_dir)
        .status()?;

    if !status.success() {
        return Err(anyhow::anyhow!(
            "Custom script '{}' exited with {}",
            script_path.display(),
            status
        ));
    }

    Ok(())
}

/// Mirrors `_desc_`: a script opts into overriding a same-named built-in command by
/// printing "true" when invoked with `_override_` as its first argument. As a guard
/// against invoking scripts written before this feature existed, we only probe scripts
/// that contain a quoted `_override_` (as the convention's `"$1" == "_override_"` check
/// would produce) rather than any occurrence of the bare word, which could appear
/// incidentally in a comment or description and would otherwise cause the script's
/// real body to run as a side effect of the probe.
pub fn get_override(project_dir: &Path, path: &Path) -> bool {
    match fs::read_to_string(path) {
        Ok(contents)
            if contents.contains("\"_override_\"") || contents.contains("'_override_'") => {}
        _ => return false,
    }

    match Command::new("bash")
        .arg(path)
        .arg("_override_")
        .current_dir(project_dir)
        .env("PROJECT_DIR", project_dir)
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim() == "true",
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClashChoice {
    Builtin,
    Custom,
}

pub trait ClashPromptProvider {
    fn resolve(&self, command_name: &str) -> Result<ClashChoice>;
}

pub struct InteractiveClashPromptProvider;

impl ClashPromptProvider for InteractiveClashPromptProvider {
    fn resolve(&self, command_name: &str) -> Result<ClashChoice> {
        const BUILTIN: &str = "Built-in command";
        const CUSTOM: &str = "Custom script";

        let choice = inquire::Select::new(
            &format!(
                "'{}' matches both a built-in command and a custom script. Which should run?",
                command_name
            ),
            vec![BUILTIN, CUSTOM],
        )
        .prompt()
        .unwrap_or(BUILTIN);

        Ok(if choice == CUSTOM {
            ClashChoice::Custom
        } else {
            ClashChoice::Builtin
        })
    }
}

/// Resolves a name clash between a built-in command and a custom script of the same
/// name. Returns `None` if there's no clash (built-in should proceed as usual), or
/// `Some(script_path)` if the custom script should run instead.
pub fn resolve_clash(
    project_dir: &Path,
    command_name: &str,
    prompt_provider: &dyn ClashPromptProvider,
) -> Result<Option<PathBuf>> {
    let Some(script_path) = find_script_path(project_dir, command_name) else {
        return Ok(None);
    };

    if get_override(project_dir, &script_path) {
        return Ok(Some(script_path));
    }

    match prompt_provider.resolve(command_name)? {
        ClashChoice::Custom => Ok(Some(script_path)),
        ClashChoice::Builtin => Ok(None),
    }
}

/// Locates the subcommand token in raw argv and everything typed after it, before any
/// clap parsing happens. `--dir`/`-d` (which always precede the subcommand and always
/// take a value, in either `--dir value` or `--dir=value` form) are skipped explicitly;
/// any other flag in `global_flags` is treated as a global toggle for `docker-control`
/// itself and stripped out wherever it appears (clap's `global = true` args are valid
/// both before and after the subcommand). Returns `None` if no subcommand token is
/// found (e.g. only global flags were given).
pub fn split_leading_subcommand(
    args: &[String],
    global_flags: &[String],
) -> Option<(String, Vec<String>)> {
    let mut i = 1; // skip binary name
    while i < args.len() {
        let token = args[i].as_str();
        if token == "--dir" || token == "-d" {
            i += 2;
            continue;
        }
        if token.starts_with("--dir=") || token.starts_with("-d=") {
            i += 1;
            continue;
        }
        if global_flags.iter().any(|flag| flag == token) {
            i += 1;
            continue;
        }
        break;
    }

    let name = args.get(i)?.clone();
    let trailing = args[i + 1..]
        .iter()
        .filter(|token| !global_flags.iter().any(|flag| flag == token.as_str()))
        .cloned()
        .collect();

    Some((name, trailing))
}

pub fn get_custom_commands(project_dir: &Path) -> Vec<CustomCommand> {
    let mut commands = Vec::new();
    let mut search_paths = Vec::new();

    // Check both possible locations for control scripts
    let htdocs_path = project_dir.join("htdocs/.docker-control/control-scripts");
    if htdocs_path.exists() {
        search_paths.push(htdocs_path);
    }

    let root_path = project_dir.join("control-scripts");
    if root_path.exists() {
        search_paths.push(root_path);
    }

    for path in search_paths {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("sh")
                    && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                {
                    let description = get_description(&path);
                    commands.push(CustomCommand {
                        name: name.to_string(),
                        description,
                    });
                }
            }
        }
    }

    // Sort by name for consistent output
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

fn get_description(path: &PathBuf) -> String {
    let output = Command::new("bash").arg(path).arg("_desc_").output();

    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "No description available".to_string(),
    }
}

/// True if `args` request help for a custom command, i.e. contain the `--help`/`-h` flag.
/// Only the flags are treated as help triggers (not a bare `help`), so a script is free to
/// use `help` as one of its own arguments — matching how clap handles built-in subcommands.
pub fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

/// Prints a custom script's own help text by invoking its `_help_` hook (mirrors `_desc_`).
/// Falls back to the script's `_desc_` description, then to a generic line, so `<command>
/// --help` always shows *something* even for scripts that don't implement `_help_`.
pub fn print_help(project_dir: &Path, name: &str, path: &Path) -> Result<()> {
    let help = get_help(project_dir, path);
    if help.is_empty() {
        let description = get_description(&path.to_path_buf());
        println!("{name} - {description}");
    } else {
        println!("{help}");
    }
    Ok(())
}

/// Invokes a script's `_help_` hook and returns its trimmed output, or an empty string if
/// the hook is absent/fails. As a guard against scripts written before this feature existed
/// (which would otherwise run their real body when probed with `_help_`), we only probe
/// scripts that contain a quoted `_help_` — mirroring the same guard in [`get_override`].
fn get_help(project_dir: &Path, path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(contents) if contents.contains("\"_help_\"") || contents.contains("'_help_'") => {}
        _ => return String::new(),
    }

    let output = Command::new("bash")
        .arg(path)
        .arg("_help_")
        .current_dir(project_dir)
        .env("PROJECT_DIR", project_dir)
        .output();

    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn wants_help_detects_flags_only() {
        assert!(wants_help(&["--help".to_string()]));
        assert!(wants_help(&["foo".to_string(), "-h".to_string()]));
        // A bare `help` is a normal argument, not a help trigger (matches clap).
        assert!(!wants_help(&["help".to_string()]));
        assert!(!wants_help(&["foo".to_string()]));
        assert!(!wants_help(&[]));
    }

    #[test]
    fn get_help_runs_help_hook_when_present() {
        let dir = tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "cmd.sh",
            "#!/bin/bash\nif [[ \"$1\" == \"_help_\" ]]; then echo \"custom help\"; exit 0; fi\necho ran body\n",
        );
        assert_eq!(get_help(dir.path(), &script), "custom help");
    }

    #[test]
    fn get_help_skips_probe_for_script_without_marker() {
        // A script with no `_help_` guard must NOT have its body executed by the probe.
        let dir = tempdir().unwrap();
        let script = write_script(dir.path(), "cmd.sh", "#!/bin/bash\necho ran body\n");
        assert_eq!(get_help(dir.path(), &script), "");
    }
}
