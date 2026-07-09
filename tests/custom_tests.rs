use docker_control::commands::custom::{
    self, ClashChoice, ClashPromptProvider, InteractiveClashPromptProvider,
};
use std::fs;
use tempfile::TempDir;

fn write_script(dir: &std::path::Path, relative_path: &str, content: &str) {
    let full_path = dir.join(relative_path);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(full_path, content).unwrap();
}

#[test]
fn find_script_path_finds_root_control_script() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "control-scripts/build.sh",
        "#!/bin/bash\nexit 0\n",
    );

    let found = custom::find_script_path(temp.path(), "build").unwrap();
    assert_eq!(found, temp.path().join("control-scripts/build.sh"));
}

#[test]
fn find_script_path_finds_htdocs_control_script() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "htdocs/.docker-control/control-scripts/build.sh",
        "#!/bin/bash\nexit 0\n",
    );

    let found = custom::find_script_path(temp.path(), "build").unwrap();
    assert_eq!(
        found,
        temp.path()
            .join("htdocs/.docker-control/control-scripts/build.sh")
    );
}

#[test]
fn find_script_path_prefers_htdocs_over_root() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "control-scripts/build.sh",
        "#!/bin/bash\nexit 0\n",
    );
    write_script(
        temp.path(),
        "htdocs/.docker-control/control-scripts/build.sh",
        "#!/bin/bash\nexit 0\n",
    );

    let found = custom::find_script_path(temp.path(), "build").unwrap();
    assert_eq!(
        found,
        temp.path()
            .join("htdocs/.docker-control/control-scripts/build.sh")
    );
}

#[test]
fn find_script_path_returns_none_when_missing() {
    let temp = TempDir::new().unwrap();
    assert!(custom::find_script_path(temp.path(), "build").is_none());
}

#[test]
fn get_override_true_when_script_echoes_true() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "control-scripts/build.sh",
        "#!/bin/bash\nif [[ \"$1\" == \"_override_\" ]]; then\n    echo \"true\"\n    exit 0\nfi\nexit 0\n",
    );

    let path = custom::find_script_path(temp.path(), "build").unwrap();
    assert!(custom::get_override(temp.path(), &path));
}

#[test]
fn get_override_false_when_block_absent() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "control-scripts/build.sh",
        "#!/bin/bash\nexit 0\n",
    );

    let path = custom::find_script_path(temp.path(), "build").unwrap();
    assert!(!custom::get_override(temp.path(), &path));
}

#[test]
fn get_override_false_when_script_exits_nonzero() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "control-scripts/build.sh",
        "#!/bin/bash\nif [[ \"$1\" == \"_override_\" ]]; then\n    echo \"true\"\n    exit 1\nfi\nexit 0\n",
    );

    let path = custom::find_script_path(temp.path(), "build").unwrap();
    assert!(!custom::get_override(temp.path(), &path));
}

#[test]
fn get_override_never_executes_script_lacking_the_override_marker() {
    let temp = TempDir::new().unwrap();
    let marker = temp.path().join("ran.marker");
    write_script(
        temp.path(),
        "control-scripts/start.sh",
        &format!(
            "#!/bin/bash\ntouch \"{}\"\nexit 0\n",
            marker.to_string_lossy()
        ),
    );

    let path = custom::find_script_path(temp.path(), "start").unwrap();
    assert!(!custom::get_override(temp.path(), &path));
    assert!(
        !marker.exists(),
        "get_override must not run a script that never mentions `_override_`"
    );
}

#[test]
fn get_override_sets_project_dir_env_and_cwd() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "control-scripts/build.sh",
        "#!/bin/bash\nif [[ \"$1\" == \"_override_\" ]]; then\n    [[ \"$PROJECT_DIR\" != \"\" && -f \"$PROJECT_DIR/marker.txt\" ]] && echo \"true\"\n    exit 0\nfi\nexit 0\n",
    );
    write_script(temp.path(), "marker.txt", "present");

    let path = custom::find_script_path(temp.path(), "build").unwrap();
    assert!(custom::get_override(temp.path(), &path));
}

struct MockClashPromptProvider {
    choice: ClashChoice,
}

impl ClashPromptProvider for MockClashPromptProvider {
    fn resolve(&self, _command_name: &str) -> anyhow::Result<ClashChoice> {
        Ok(self.choice)
    }
}

struct PanicIfCalledPromptProvider;

impl ClashPromptProvider for PanicIfCalledPromptProvider {
    fn resolve(&self, _command_name: &str) -> anyhow::Result<ClashChoice> {
        panic!("prompt should not be invoked when the script declares _override_");
    }
}

#[test]
fn resolve_clash_returns_none_when_no_script_exists() {
    let temp = TempDir::new().unwrap();
    let result = custom::resolve_clash(temp.path(), "build", &PanicIfCalledPromptProvider).unwrap();
    assert!(result.is_none());
}

#[test]
fn resolve_clash_returns_script_path_when_override_true_without_prompting() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "control-scripts/build.sh",
        "#!/bin/bash\nif [[ \"$1\" == \"_override_\" ]]; then\n    echo \"true\"\n    exit 0\nfi\nexit 0\n",
    );

    let result = custom::resolve_clash(temp.path(), "build", &PanicIfCalledPromptProvider).unwrap();
    assert_eq!(result, Some(temp.path().join("control-scripts/build.sh")));
}

#[test]
fn resolve_clash_prompts_and_returns_none_on_builtin_choice() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "control-scripts/build.sh",
        "#!/bin/bash\nexit 0\n",
    );

    let provider = MockClashPromptProvider {
        choice: ClashChoice::Builtin,
    };
    let result = custom::resolve_clash(temp.path(), "build", &provider).unwrap();
    assert!(result.is_none());
}

#[test]
fn resolve_clash_prompts_and_returns_script_path_on_custom_choice() {
    let temp = TempDir::new().unwrap();
    write_script(
        temp.path(),
        "control-scripts/build.sh",
        "#!/bin/bash\nexit 0\n",
    );

    let provider = MockClashPromptProvider {
        choice: ClashChoice::Custom,
    };
    let result = custom::resolve_clash(temp.path(), "build", &provider).unwrap();
    assert_eq!(result, Some(temp.path().join("control-scripts/build.sh")));
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn split_leading_subcommand_plain_case() {
    let args = strings(&["docker-control", "build", "--no-cache"]);
    let (name, trailing) = custom::split_leading_subcommand(&args, &[]).unwrap();
    assert_eq!(name, "build");
    assert_eq!(trailing, vec!["--no-cache".to_string()]);
}

#[test]
fn split_leading_subcommand_skips_dir_flag_space_form() {
    let args = strings(&[
        "docker-control",
        "--dir",
        "/tmp/proj",
        "build",
        "--no-cache",
    ]);
    let (name, trailing) = custom::split_leading_subcommand(&args, &[]).unwrap();
    assert_eq!(name, "build");
    assert_eq!(trailing, vec!["--no-cache".to_string()]);
}

#[test]
fn split_leading_subcommand_skips_dir_flag_equals_form() {
    let args = strings(&["docker-control", "--dir=/tmp/proj", "build", "--no-cache"]);
    let (name, trailing) = custom::split_leading_subcommand(&args, &[]).unwrap();
    assert_eq!(name, "build");
    assert_eq!(trailing, vec!["--no-cache".to_string()]);
}

#[test]
fn split_leading_subcommand_skips_short_dir_flag_equals_form() {
    let args = strings(&["docker-control", "-d=/tmp/proj", "build"]);
    let (name, trailing) = custom::split_leading_subcommand(&args, &[]).unwrap();
    assert_eq!(name, "build");
    assert!(trailing.is_empty());
}

#[test]
fn split_leading_subcommand_skips_global_flag_before_subcommand() {
    let args = strings(&["docker-control", "--debug", "status", "--no-cache"]);
    let global_flags = strings(&["--debug"]);
    let (name, trailing) = custom::split_leading_subcommand(&args, &global_flags).unwrap();
    assert_eq!(name, "status");
    assert_eq!(trailing, vec!["--no-cache".to_string()]);
}

#[test]
fn split_leading_subcommand_strips_global_flag_after_subcommand() {
    let args = strings(&["docker-control", "status", "--debug"]);
    let global_flags = strings(&["--debug"]);
    let (name, trailing) = custom::split_leading_subcommand(&args, &global_flags).unwrap();
    assert_eq!(name, "status");
    assert!(
        trailing.is_empty(),
        "a global flag placed after the subcommand must not be forwarded as a script arg"
    );
}

#[test]
fn split_leading_subcommand_strips_global_flag_interleaved_with_real_args() {
    let args = strings(&["docker-control", "build", "--no-cache", "--debug", "foo"]);
    let global_flags = strings(&["--debug"]);
    let (name, trailing) = custom::split_leading_subcommand(&args, &global_flags).unwrap();
    assert_eq!(name, "build");
    assert_eq!(trailing, vec!["--no-cache".to_string(), "foo".to_string()]);
}

#[test]
fn split_leading_subcommand_returns_none_when_only_flags_given() {
    let args = strings(&["docker-control", "--debug"]);
    let global_flags = strings(&["--debug"]);
    assert!(custom::split_leading_subcommand(&args, &global_flags).is_none());
}

#[test]
fn split_leading_subcommand_returns_none_for_bare_binary() {
    let args = strings(&["docker-control"]);
    assert!(custom::split_leading_subcommand(&args, &[]).is_none());
}

#[test]
fn split_leading_subcommand_empty_when_no_extra_tokens() {
    let args = strings(&["docker-control", "build"]);
    let (name, trailing) = custom::split_leading_subcommand(&args, &[]).unwrap();
    assert_eq!(name, "build");
    assert!(trailing.is_empty());
}

#[test]
fn interactive_clash_prompt_provider_is_constructible() {
    // Smoke test only: the provider is exercised interactively in main.rs and can't
    // be driven headlessly here without a TTY. This just guards the type/trait wiring.
    let _provider: &dyn ClashPromptProvider = &InteractiveClashPromptProvider;
}
