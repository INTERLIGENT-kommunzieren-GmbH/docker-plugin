mod common;

use anyhow::Result;
use common::TestRepo;
use docker_control::commands::module::{
    ModuleAction, ModuleOptions, ModulePromptProvider, execute, installed_version, linked_modules,
};
use docker_control::utils;
use std::fs;
use std::path::Path;

/// Canned answers, one field per prompt method — the pattern used by the release
/// and merge integration tests.
#[derive(Default)]
struct MockPrompts {
    link: Option<String>,
    unlink: Option<String>,
    confirm_purge: bool,
}

impl ModulePromptProvider for MockPrompts {
    fn select_module_to_link(&self, modules: Vec<String>) -> Result<String> {
        Ok(self
            .link
            .clone()
            .unwrap_or_else(|| modules.first().cloned().unwrap_or_default()))
    }

    fn select_module_to_unlink(&self, modules: Vec<String>) -> Result<String> {
        Ok(self
            .unlink
            .clone()
            .unwrap_or_else(|| modules.first().cloned().unwrap_or_default()))
    }

    fn confirm_purge(&self, _module: &str) -> Result<bool> {
        Ok(self.confirm_purge)
    }
}

fn options(prompts: MockPrompts) -> ModuleOptions {
    ModuleOptions {
        prompt_provider: Box::new(prompts),
        // Composer is never invoked in tests; these assertions cover the Rust side
        // only. The Composer interaction is exercised manually.
        skip_composer: true,
    }
}

/// A project with `htdocs/composer.json`, a `composer.lock` recording
/// `test/module` at `1.0.x-dev`, and a source-installed `htdocs/vendor/test/module`
/// git clone with its own origin.
fn setup(name: &str) -> Result<TestRepo> {
    let repo = TestRepo::new(name)?;

    repo.write_file(
        "htdocs/composer.json",
        // Deliberately not alphabetical and 4-space indented, so a reflow is visible.
        "{\n    \"name\": \"test/project\",\n    \"require\": {\n        \"test/module\": \"^1.0\"\n    },\n    \"license\": \"proprietary\"\n}\n",
    )?;
    repo.write_file(
        "htdocs/composer.lock",
        r#"{"packages": [{"name": "test/module", "version": "1.0.x-dev"}], "packages-dev": []}"#,
    )?;
    repo.write_file(".env", "PHP_VERSION=8.2")?;
    repo.write_file(".managed-by-docker-control", "")?;

    let temp_parent = repo.root.parent().unwrap();
    let module_origin = temp_parent.join("module_origin.git");
    fs::create_dir_all(&module_origin)?;
    TestRepo::git_run(&module_origin, &["init", "--bare", "--initial-branch=main"])?;

    let vendor_path = repo.root.join("htdocs/vendor/test/module");
    fs::create_dir_all(&vendor_path)?;
    TestRepo::git_run(&vendor_path, &["init", "--initial-branch=main"])?;
    TestRepo::git_run(&vendor_path, &["config", "user.email", "test@example.com"])?;
    TestRepo::git_run(&vendor_path, &["config", "user.name", "Test User"])?;
    TestRepo::git_run(
        &vendor_path,
        &["remote", "add", "origin", &module_origin.to_string_lossy()],
    )?;
    fs::write(
        vendor_path.join("composer.json"),
        r#"{"name": "test/module", "version": "1.0.x-dev"}"#,
    )?;
    TestRepo::git_run(&vendor_path, &["add", "."])?;
    TestRepo::git_run(&vendor_path, &["commit", "-m", "Initial module commit"])?;
    TestRepo::git_run(&vendor_path, &["push", "origin", "main"])?;

    Ok(repo)
}

fn link_action(module: &str) -> ModuleAction {
    ModuleAction::Link {
        module: Some(module.to_string()),
        version: None,
        composer_args: Vec::new(),
    }
}

/// Stands in for what Composer does on a successful `link`, so `unlink` and `list`
/// can be tested against a realistic on-disk state.
fn fake_composer_link(htdocs: &Path, relative: &str, pin: &str) -> Result<()> {
    let composer_path = htdocs.join("composer.json");
    let mut value: serde_json::Value = serde_json::from_str(&fs::read_to_string(&composer_path)?)?;

    // Array form with a `name`, matching what `composer config` actually writes
    // into a real project (whose `repositories` is an array).
    let entry = serde_json::json!({
        "name": format!("dc2-{}", relative.replace('/', "-")),
        "type": "path",
        "url": format!("modules/{}", relative),
        "options": {
            "symlink": true,
            "versions": { relative: pin }
        }
    });
    value["repositories"] = serde_json::json!([entry]);
    fs::write(&composer_path, serde_json::to_string_pretty(&value)?)?;

    let vendor_path = htdocs.join("vendor").join(relative);
    fs::create_dir_all(vendor_path.parent().unwrap())?;
    std::os::unix::fs::symlink(format!("../../modules/{}", relative), &vendor_path)?;
    Ok(())
}

#[test]
fn link_moves_the_module_out_of_vendor_preserving_git_history() -> Result<()> {
    let repo = setup("link_move")?;

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;

    let dev_path = repo.root.join("htdocs/modules/test/module");
    assert!(dev_path.is_dir(), "module should be moved to modules/");
    assert!(dev_path.join(".git").exists(), "git history must survive");
    assert!(
        !repo.root.join("htdocs/vendor/test/module").exists(),
        "vendor copy should be gone (composer recreates it as a symlink)"
    );

    // The pin comes from composer.lock, and the branch it implies is checked out.
    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&dev_path)
        .output()?;
    assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "main");

    Ok(())
}

#[test]
fn link_appends_modules_to_gitignore_exactly_once() -> Result<()> {
    let repo = setup("link_gitignore")?;
    repo.write_file("htdocs/.gitignore", "vendor/\n")?;

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;

    let gitignore = fs::read_to_string(repo.root.join("htdocs/.gitignore"))?;
    assert_eq!(
        gitignore.matches("/modules/").count(),
        1,
        "gitignore: {:?}",
        gitignore
    );
    assert!(gitignore.contains("vendor/"), "existing entries preserved");

    // Re-linking must not add a second entry.
    fake_composer_link(&repo.root.join("htdocs"), "test/module", "1.0.x-dev")?;
    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;

    let gitignore = fs::read_to_string(repo.root.join("htdocs/.gitignore"))?;
    assert_eq!(gitignore.matches("/modules/").count(), 1);

    Ok(())
}

#[test]
fn link_writes_an_ownership_marker_above_its_gitignore_entry() -> Result<()> {
    let repo = setup("gitignore_marker")?;
    let htdocs = repo.root.join("htdocs");
    // No trailing newline: the append must not rewrite the previous last line.
    fs::write(htdocs.join(".gitignore"), "vendor/\ndata/cache")?;

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;

    let written = fs::read_to_string(htdocs.join(".gitignore"))?;
    assert!(
        written.contains("# docker-control"),
        "the entry needs an ownership marker so unlink can remove only its own: {:?}",
        written
    );
    // The marker must sit directly above the entry — that adjacency is what
    // `remove_modules_gitignore` matches on.
    let lines: Vec<&str> = written.lines().collect();
    let marker_at = lines
        .iter()
        .position(|l| l.starts_with("# docker-control"))
        .expect("marker present");
    assert_eq!(lines[marker_at + 1].trim(), "/modules/");
    assert!(!written.ends_with('\n'), "trailing-newline state preserved");
    assert!(
        written.starts_with("vendor/\ndata/cache\n"),
        "prior content kept"
    );

    // Byte-for-byte restoration is covered by the unit tests in
    // src/commands/module.rs: reaching it here would need Composer to have run
    // `config --unset`, which `skip_composer` suppresses.
    Ok(())
}

#[test]
fn unlink_never_removes_a_gitignore_entry_the_project_already_had() -> Result<()> {
    let repo = setup("gitignore_preexisting")?;
    let htdocs = repo.root.join("htdocs");
    // The project ignores modules/ itself — unmarked, so not ours to delete.
    fs::write(htdocs.join(".gitignore"), "vendor/\n/modules/\n")?;
    let before = fs::read_to_string(htdocs.join(".gitignore"))?;

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;
    assert_eq!(
        before,
        fs::read_to_string(htdocs.join(".gitignore"))?,
        "link must not touch an existing entry"
    );

    fake_composer_link(&htdocs, "test/module", "1.0.x-dev")?;
    execute(
        &repo.root,
        ModuleAction::Unlink {
            module: Some("test/module".to_string()),
            purge: true,
            yes: true,
        },
        options(MockPrompts::default()),
    )?;

    assert_eq!(
        before,
        fs::read_to_string(htdocs.join(".gitignore"))?,
        "unlink must not delete the project's own entry"
    );
    Ok(())
}

#[test]
fn unlink_keeps_the_gitignore_entry_while_a_checkout_remains() -> Result<()> {
    let repo = setup("gitignore_still_needed")?;
    let htdocs = repo.root.join("htdocs");

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;
    fake_composer_link(&htdocs, "test/module", "1.0.x-dev")?;

    // Unlink without --purge: the checkout stays, so the entry must stay too.
    execute(
        &repo.root,
        ModuleAction::Unlink {
            module: Some("test/module".to_string()),
            purge: false,
            yes: false,
        },
        options(MockPrompts::default()),
    )?;

    assert!(htdocs.join("modules/test/module").exists());
    assert!(
        fs::read_to_string(htdocs.join(".gitignore"))?.contains("/modules/"),
        "an un-purged checkout must stay ignored"
    );
    Ok(())
}

#[test]
fn link_is_idempotent_once_the_path_repository_exists() -> Result<()> {
    let repo = setup("link_idempotent")?;
    let htdocs = repo.root.join("htdocs");

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;
    fake_composer_link(&htdocs, "test/module", "1.0.x-dev")?;

    let before = fs::read_to_string(htdocs.join("composer.json"))?;
    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;
    let after = fs::read_to_string(htdocs.join("composer.json"))?;

    assert_eq!(before, after, "second link must be a no-op");
    Ok(())
}

#[test]
fn link_rejects_an_unknown_module_and_changes_nothing() -> Result<()> {
    let repo = setup("link_unknown")?;
    let before = fs::read_to_string(repo.root.join("htdocs/composer.json"))?;

    let err = execute(
        &repo.root,
        link_action("nope/missing"),
        options(MockPrompts::default()),
    )
    .unwrap_err();

    assert!(err.to_string().contains("not found"), "got: {}", err);
    assert_eq!(
        before,
        fs::read_to_string(repo.root.join("htdocs/composer.json"))?
    );
    assert!(!repo.root.join("htdocs/modules").exists());
    Ok(())
}

#[test]
fn link_requires_an_explicit_version_when_the_lock_has_none() -> Result<()> {
    let repo = setup("link_no_lock")?;
    fs::remove_file(repo.root.join("htdocs/composer.lock"))?;

    let err = execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )
    .unwrap_err();
    assert!(err.to_string().contains("--version"), "got: {}", err);

    // The explicit pin unblocks it.
    execute(
        &repo.root,
        ModuleAction::Link {
            module: Some("test/module".to_string()),
            version: Some("1.0.x-dev".to_string()),
            composer_args: Vec::new(),
        },
        options(MockPrompts::default()),
    )?;
    assert!(repo.root.join("htdocs/modules/test/module").is_dir());
    Ok(())
}

#[test]
fn installed_version_reads_both_package_lists() -> Result<()> {
    let repo = setup("lock_read")?;
    let htdocs = repo.root.join("htdocs");

    assert_eq!(
        installed_version(&htdocs, "test/module")?.as_deref(),
        Some("1.0.x-dev")
    );
    assert_eq!(installed_version(&htdocs, "other/thing")?, None);

    fs::write(
        htdocs.join("composer.lock"),
        r#"{"packages": [], "packages-dev": [{"name": "dev/only", "version": "3.1.0"}]}"#,
    )?;
    assert_eq!(
        installed_version(&htdocs, "dev/only")?.as_deref(),
        Some("3.1.0")
    );
    Ok(())
}

#[test]
fn linked_modules_matches_on_shape_not_on_key_name() -> Result<()> {
    let repo = setup("linked_parse")?;
    let htdocs = repo.root.join("htdocs");

    // Object form, a hand-written key, plus entries that must be ignored.
    repo.write_file(
        "htdocs/composer.json",
        r#"{
    "repositories": {
        "hand-written-name": {
            "type": "path",
            "url": "modules/test/module",
            "options": { "symlink": true, "versions": { "test/module": "1.0.x-dev" } }
        },
        "private": { "type": "composer", "url": "https://example.test/packages/" },
        "elsewhere": { "type": "path", "url": "../outside/thing" }
    }
}"#,
    )?;

    let linked = linked_modules(&htdocs)?;
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].key.as_deref(), Some("hand-written-name"));
    assert_eq!(linked[0].package, "test/module");
    assert_eq!(linked[0].version.as_deref(), Some("1.0.x-dev"));
    assert_eq!(linked[0].relative_path(), "test/module");

    // Array form — what a real project looks like. The key must come from the
    // entry's `name`, which is what Composer writes and addresses entries by.
    // The array INDEX is not usable: `composer config --unset repositories.1`
    // silently does nothing and still exits 0.
    repo.write_file(
        "htdocs/composer.json",
        r#"{
    "repositories": [
        { "type": "composer", "url": "https://example.test/packages/" },
        { "name": "dc2-test-module", "type": "path", "url": "modules/test/module",
          "options": { "symlink": true, "versions": { "test/module": "2.0.x-dev" } } }
    ]
}"#,
    )?;

    let linked = linked_modules(&htdocs)?;
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].key.as_deref(), Some("dc2-test-module"));
    assert_eq!(linked[0].version.as_deref(), Some("2.0.x-dev"));

    // An array entry with no `name` is unaddressable, so the key must be None.
    repo.write_file(
        "htdocs/composer.json",
        r#"{
    "repositories": [
        { "type": "path", "url": "modules/test/module",
          "options": { "symlink": true, "versions": { "test/module": "2.0.x-dev" } } }
    ]
}"#,
    )?;

    let linked = linked_modules(&htdocs)?;
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].key, None);
    Ok(())
}

#[test]
fn unlink_refuses_an_entry_composer_config_cannot_address() -> Result<()> {
    let repo = setup("unlink_unaddressable")?;

    // A hand-written array entry with no `name`: `composer config --unset` would
    // report success without removing it, so unlink must refuse up front rather
    // than leave the project half-changed.
    repo.write_file(
        "htdocs/composer.json",
        r#"{
    "repositories": [
        { "type": "path", "url": "modules/test/module",
          "options": { "symlink": true, "versions": { "test/module": "1.0.x-dev" } } }
    ]
}"#,
    )?;
    let before = fs::read_to_string(repo.root.join("htdocs/composer.json"))?;

    let err = execute(
        &repo.root,
        ModuleAction::Unlink {
            module: Some("test/module".to_string()),
            purge: false,
            yes: false,
        },
        options(MockPrompts::default()),
    )
    .unwrap_err();

    assert!(err.to_string().contains("\"name\""), "got: {}", err);
    assert_eq!(
        before,
        fs::read_to_string(repo.root.join("htdocs/composer.json"))?,
        "nothing may change when unlink refuses"
    );
    Ok(())
}

#[test]
fn failed_link_leaves_no_empty_modules_directories() -> Result<()> {
    let repo = setup("link_rollback_dirs")?;

    // Force the rollback path: no composer.lock entry and no --version.
    fs::write(
        repo.root.join("htdocs/composer.lock"),
        r#"{"packages": [], "packages-dev": []}"#,
    )?;
    assert!(
        execute(
            &repo.root,
            link_action("test/module"),
            options(MockPrompts::default()),
        )
        .is_err()
    );

    assert!(
        !repo.root.join("htdocs/modules").exists(),
        "a failed link must not leave modules/ behind"
    );
    assert!(
        repo.root.join("htdocs/vendor/test/module/.git").exists(),
        "the module must still be in vendor/"
    );
    Ok(())
}

#[test]
fn unlink_removes_only_the_symlink_and_keeps_the_checkout() -> Result<()> {
    let repo = setup("unlink_keep")?;
    let htdocs = repo.root.join("htdocs");

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;
    fake_composer_link(&htdocs, "test/module", "1.0.x-dev")?;

    let vendor_path = htdocs.join("vendor/test/module");
    assert!(fs::symlink_metadata(&vendor_path)?.file_type().is_symlink());

    execute(
        &repo.root,
        ModuleAction::Unlink {
            module: Some("test/module".to_string()),
            purge: false,
            yes: false,
        },
        options(MockPrompts::default()),
    )?;

    assert!(
        fs::symlink_metadata(&vendor_path).is_err(),
        "the symlink must be gone before composer runs"
    );
    assert!(
        htdocs.join("modules/test/module/.git").exists(),
        "the development checkout must be preserved"
    );
    Ok(())
}

#[test]
fn unlink_errors_when_nothing_is_linked() -> Result<()> {
    let repo = setup("unlink_none")?;

    let err = execute(
        &repo.root,
        ModuleAction::Unlink {
            module: None,
            purge: false,
            yes: false,
        },
        options(MockPrompts::default()),
    )
    .unwrap_err();

    assert!(err.to_string().contains("No modules"), "got: {}", err);
    Ok(())
}

#[test]
fn purge_keeps_a_dirty_checkout_when_the_prompt_is_declined() -> Result<()> {
    let repo = setup("purge_declined")?;
    let htdocs = repo.root.join("htdocs");

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;
    fake_composer_link(&htdocs, "test/module", "1.0.x-dev")?;

    // Uncommitted work in the development checkout.
    fs::write(
        htdocs.join("modules/test/module/scratch.php"),
        "<?php // wip",
    )?;

    execute(
        &repo.root,
        ModuleAction::Unlink {
            module: Some("test/module".to_string()),
            purge: true,
            yes: false,
        },
        options(MockPrompts {
            confirm_purge: false,
            ..MockPrompts::default()
        }),
    )?;

    assert!(
        htdocs.join("modules/test/module/scratch.php").exists(),
        "declining the prompt must keep the checkout"
    );
    Ok(())
}

#[test]
fn purge_removes_a_clean_checkout_without_prompting() -> Result<()> {
    let repo = setup("purge_clean")?;
    let htdocs = repo.root.join("htdocs");

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;
    fake_composer_link(&htdocs, "test/module", "1.0.x-dev")?;

    execute(
        &repo.root,
        ModuleAction::Unlink {
            module: Some("test/module".to_string()),
            purge: true,
            yes: false,
        },
        // confirm_purge is false: a clean, fully-pushed checkout must not ask.
        options(MockPrompts::default()),
    )?;

    assert!(!htdocs.join("modules/test/module").exists());
    Ok(())
}

#[test]
fn list_reports_linked_and_unlinked_modules() -> Result<()> {
    let repo = setup("list")?;
    let htdocs = repo.root.join("htdocs");

    execute(
        &repo.root,
        ModuleAction::List,
        options(MockPrompts::default()),
    )?;

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;
    fake_composer_link(&htdocs, "test/module", "1.0.x-dev")?;

    let linked = linked_modules(&htdocs)?;
    assert_eq!(linked.len(), 1);
    execute(
        &repo.root,
        ModuleAction::List,
        options(MockPrompts::default()),
    )?;
    Ok(())
}

#[test]
fn execute_requires_an_application_composer_json() -> Result<()> {
    let repo = TestRepo::new("no_composer")?;

    let err = execute(
        &repo.root,
        ModuleAction::List,
        options(MockPrompts::default()),
    )
    .unwrap_err();

    assert!(err.to_string().contains("composer.json"), "got: {}", err);
    Ok(())
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

fn create_action(module: &str) -> ModuleAction {
    ModuleAction::Create {
        module: module.to_string(),
        r#type: None,
        // `composer init`'s questions can't be answered in a test; this is also what a
        // non-terminal stdin would force at runtime.
        yes: true,
        composer_args: Vec::new(),
    }
}

/// One commit, and which branch it is on. `skip_composer` means the module has no
/// `composer.json`, so the commit holds only the `src/` keepfile.
fn head_branch_and_count(path: &Path) -> Result<(String, usize)> {
    let repo = git2::Repository::open(path)?;
    let branch = repo.head()?.shorthand().unwrap_or_default().to_string();
    let mut walk = repo.revwalk()?;
    walk.push_head()?;
    Ok((branch, walk.count()))
}

#[test]
fn create_scaffolds_a_module_and_wires_it_for_development() -> Result<()> {
    let repo = setup("create_scaffold")?;
    let htdocs = repo.root.join("htdocs");
    fs::create_dir_all(repo.root.join(".idea"))?;

    execute(
        &repo.root,
        create_action("acme/widget"),
        options(MockPrompts::default()),
    )?;

    let dev_path = htdocs.join("modules/acme/widget");
    assert!(dev_path.join("src/.gitkeep").exists(), "src/ must be kept");

    let (branch, commits) = head_branch_and_count(&dev_path)?;
    assert_eq!(branch, "main", "the pin dev-main implies branch main");
    assert_eq!(commits, 1, "the scaffold must be committed");

    assert!(
        fs::read_to_string(htdocs.join(".gitignore"))?.contains("/modules/"),
        "the checkout must be ignored"
    );
    assert!(
        fs::read_to_string(repo.root.join(".idea/vcs.xml"))?.contains("htdocs/modules/acme/widget"),
        "PhpStorm must see the new checkout as a git root"
    );
    Ok(())
}

#[test]
fn create_rejects_an_invalid_package_name() -> Result<()> {
    let repo = setup("create_bad_name")?;

    for bad in [
        "Acme/Widget", // Composer package names are lowercase
        "acme",
        "acme/widget/extra",
        "acme/-widget",
        "acme/widget-",
        "acme/wid---get",
    ] {
        let err = execute(
            &repo.root,
            create_action(bad),
            options(MockPrompts::default()),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("not a valid Composer package name"),
            "{}: {}",
            bad,
            err
        );
    }

    // Validation runs before anything is created, so no rejection may leave a directory.
    assert!(!repo.root.join("htdocs/modules").exists());
    Ok(())
}

#[test]
fn create_points_at_link_when_the_module_already_exists() -> Result<()> {
    let repo = setup("create_exists")?;
    let htdocs = repo.root.join("htdocs");

    // Already a development checkout.
    fs::create_dir_all(htdocs.join("modules/acme/widget"))?;
    let err = execute(
        &repo.root,
        create_action("acme/widget"),
        options(MockPrompts::default()),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("module link acme/widget"), "{}", err);

    // Already installed under vendor/ — the setup() fixture ships test/module there.
    let err = execute(
        &repo.root,
        create_action("test/module"),
        options(MockPrompts::default()),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("module link test/module"), "{}", err);
    Ok(())
}

#[test]
fn create_refuses_a_module_that_is_already_linked() -> Result<()> {
    let repo = setup("create_already_linked")?;
    let htdocs = repo.root.join("htdocs");

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;
    fake_composer_link(&htdocs, "test/module", "1.0.x-dev")?;

    let err = execute(
        &repo.root,
        create_action("test/module"),
        options(MockPrompts::default()),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("already linked"), "{}", err);
    Ok(())
}

// ---------------------------------------------------------------------------
// unlink guard: a module that exists only locally
// ---------------------------------------------------------------------------

#[test]
fn unlink_refuses_a_checkout_with_no_remote() -> Result<()> {
    let repo = setup("unlink_local_only")?;
    let htdocs = repo.root.join("htdocs");

    // The state `create` leaves behind: a linked module whose checkout was never pushed.
    execute(
        &repo.root,
        create_action("acme/widget"),
        options(MockPrompts::default()),
    )?;
    fake_composer_link(&htdocs, "acme/widget", "dev-main")?;

    let before_json = fs::read_to_string(htdocs.join("composer.json"))?;
    let before_lock = fs::read_to_string(htdocs.join("composer.lock"))?;

    let err = execute(
        &repo.root,
        ModuleAction::Unlink {
            module: Some("acme/widget".to_string()),
            purge: false,
            yes: false,
        },
        options(MockPrompts::default()),
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("no git remote"), "{}", err);
    assert!(err.contains("composer remove acme/widget"), "{}", err);
    assert_eq!(
        fs::read_to_string(htdocs.join("composer.json"))?,
        before_json,
        "the guard must fire before anything is written"
    );
    assert_eq!(
        fs::read_to_string(htdocs.join("composer.lock"))?,
        before_lock
    );
    Ok(())
}

#[test]
fn unlink_proceeds_when_the_checkout_is_gone() -> Result<()> {
    let repo = setup("unlink_no_checkout")?;
    let htdocs = repo.root.join("htdocs");

    execute(
        &repo.root,
        create_action("acme/widget"),
        options(MockPrompts::default()),
    )?;
    fake_composer_link(&htdocs, "acme/widget", "dev-main")?;
    // Nothing to push and nothing to keep: removing a stale entry is exactly what unlink is for.
    fs::remove_dir_all(htdocs.join("modules/acme/widget"))?;

    execute(
        &repo.root,
        ModuleAction::Unlink {
            module: Some("acme/widget".to_string()),
            purge: false,
            yes: false,
        },
        options(MockPrompts::default()),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// .idea / PhpStorm registration
// ---------------------------------------------------------------------------

/// IDEA stores `IssueNavigationConfiguration` in `vcs.xml` too, and writes it *before*
/// `VcsDirectoryMappings`. The mapping has to land in the mappings component — anchoring on
/// the first `</component>` in the file put it in the navigation one, where PhpStorm ignores
/// it and drops it on the next rewrite.
#[test]
fn link_registers_the_git_root_in_the_mappings_component_not_the_first_one() -> Result<()> {
    let repo = setup("idea_register_second_component")?;
    fs::create_dir_all(repo.root.join(".idea"))?;
    repo.write_file(
        ".idea/vcs.xml",
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <project version=\"4\">\n\
         \x20 <component name=\"IssueNavigationConfiguration\">\n\
         \x20   <option name=\"links\" />\n\
         \x20 </component>\n\
         \x20 <component name=\"VcsDirectoryMappings\">\n\
         \x20   <mapping directory=\"$PROJECT_DIR$\" vcs=\"Git\" />\n\
         \x20 </component>\n\
         </project>",
    )?;

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;

    let vcs = fs::read_to_string(repo.root.join(".idea/vcs.xml"))?;
    let mapping = "<mapping directory=\"$PROJECT_DIR$/htdocs/modules/test/module\" vcs=\"Git\" />";
    assert!(vcs.contains(mapping), "vcs.xml: {}", vcs);

    // The mapping sits after VcsDirectoryMappings opens, not inside the component before it.
    let mappings_open = vcs
        .find("<component name=\"VcsDirectoryMappings\">")
        .unwrap();
    let inserted = vcs.find(mapping).unwrap();
    assert!(
        inserted > mappings_open,
        "mapping landed in the wrong component: {}",
        vcs
    );
    // And the navigation component is untouched.
    assert!(
        vcs.contains("<option name=\"links\" />"),
        "vcs.xml: {}",
        vcs
    );

    Ok(())
}

#[test]
fn link_registers_the_checkout_as_a_phpstorm_git_root() -> Result<()> {
    let repo = setup("idea_register")?;
    fs::create_dir_all(repo.root.join(".idea"))?;
    repo.write_file(
        ".idea/vcs.xml",
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<project version=\"4\">\n  <component name=\"VcsDirectoryMappings\">\n    <mapping directory=\"$PROJECT_DIR$\" vcs=\"Git\" />\n  </component>\n</project>",
    )?;

    execute(
        &repo.root,
        link_action("test/module"),
        options(MockPrompts::default()),
    )?;

    let vcs = fs::read_to_string(repo.root.join(".idea/vcs.xml"))?;
    assert!(
        vcs.contains(
            "<mapping directory=\"$PROJECT_DIR$/htdocs/modules/test/module\" vcs=\"Git\" />"
        ),
        "vcs.xml: {}",
        vcs
    );
    // The pre-existing project mapping survives.
    assert!(vcs.contains("<mapping directory=\"$PROJECT_DIR$\" vcs=\"Git\" />"));
    assert_eq!(vcs.matches("VcsDirectoryMappings").count(), 1);

    // Unlink removes it again.
    fake_composer_link(&repo.root.join("htdocs"), "test/module", "1.0.x-dev")?;
    execute(
        &repo.root,
        ModuleAction::Unlink {
            module: Some("test/module".to_string()),
            purge: false,
            yes: false,
        },
        options(MockPrompts::default()),
    )?;

    let vcs = fs::read_to_string(repo.root.join(".idea/vcs.xml"))?;
    assert!(!vcs.contains("modules/test/module"), "vcs.xml: {}", vcs);
    assert!(vcs.contains("<mapping directory=\"$PROJECT_DIR$\" vcs=\"Git\" />"));
    Ok(())
}

#[test]
fn phpstorm_registration_is_idempotent_and_optional() -> Result<()> {
    let repo = TestRepo::new("idea_helpers")?;

    // No .idea directory at all: both directions are silent no-ops.
    utils::register_phpstorm_git_root(&repo.root, "htdocs/modules/a/b")?;
    utils::unregister_phpstorm_git_root(&repo.root, "htdocs/modules/a/b")?;
    assert!(!repo.root.join(".idea").exists());

    // .idea exists but has no vcs.xml: it gets created with the mapping.
    fs::create_dir_all(repo.root.join(".idea"))?;
    utils::register_phpstorm_git_root(&repo.root, "htdocs/modules/a/b")?;
    let vcs = fs::read_to_string(repo.root.join(".idea/vcs.xml"))?;
    assert!(vcs.contains("VcsDirectoryMappings"));
    assert!(vcs.contains("$PROJECT_DIR$/htdocs/modules/a/b"));

    // Registering twice adds one mapping.
    utils::register_phpstorm_git_root(&repo.root, "htdocs/modules/a/b")?;
    let vcs = fs::read_to_string(repo.root.join(".idea/vcs.xml"))?;
    assert_eq!(vcs.matches("htdocs/modules/a/b").count(), 1);

    // Unregistering an absent mapping is a no-op.
    utils::unregister_phpstorm_git_root(&repo.root, "htdocs/modules/x/y")?;
    assert!(fs::read_to_string(repo.root.join(".idea/vcs.xml"))?.contains("htdocs/modules/a/b"));

    utils::unregister_phpstorm_git_root(&repo.root, "htdocs/modules/a/b")?;
    let vcs = fs::read_to_string(repo.root.join(".idea/vcs.xml"))?;
    assert!(!vcs.contains("htdocs/modules/a/b"));
    assert!(
        vcs.contains("</project>"),
        "file stays well-formed: {}",
        vcs
    );
    Ok(())
}
