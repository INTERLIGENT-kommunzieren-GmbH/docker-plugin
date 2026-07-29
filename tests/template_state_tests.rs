use anyhow::Result;
use docker_control::template::{self, Change, TemplateState};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// A fixture standing in for the real `template/` directory, kept small but
/// structurally faithful: a template-owned file, a nested config file, a
/// per-project secret, a `*-dist` seed and a runtime placeholder.
fn write_template(dir: &Path) -> Result<()> {
    write(
        dir,
        "compose.yml",
        "services:\n  php:\n    image: php:8.2\n",
    )?;
    write(dir, "config/php.ini", "memory_limit = 256M\n")?;
    write(dir, "config/apache-sites/default.conf", "ServerName x\n")?;
    write(dir, "config/htpasswd", "user:default\n")?;
    write(dir, "secrets/db_pw.txt", "123456")?;
    write(
        dir,
        ".env-dist",
        "BASE_DOMAIN=example.lvh.me\nPHP_VERSION=8.2\n",
    )?;
    write(dir, ".gitignore-dist", "htdocs\nlogs\n")?;
    write(dir, "logs/apache/.gitkeep", "")?;
    write(dir, "volumes/db/data/.gitkeep", "")?;
    Ok(())
}

/// Mirrors exactly what `commands::init` produces: the whole template copied in,
/// `.gitignore-dist` renamed to `.gitignore`, `.env` written fresh from the
/// prompt answers — and `.env-dist` left in place, which is why it stays a
/// normally-tracked file rather than a consumed seed.
fn init_project(project: &Path, template: &Path) -> Result<()> {
    copy_dir(template, project)?;
    fs::rename(project.join(".gitignore-dist"), project.join(".gitignore"))?;
    write(
        project,
        ".env",
        "BASE_DOMAIN=myproj.lvh.me\nPHP_VERSION=8.2\n",
    )?;
    template::stamp(project, template, true)?;
    Ok(())
}

fn write(dir: &Path, rel: &str, contents: &str) -> Result<()> {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn fixture() -> Result<(TempDir, TempDir)> {
    let template = TempDir::new()?;
    let project = TempDir::new()?;
    write_template(template.path())?;
    init_project(project.path(), template.path())?;
    Ok((template, project))
}

#[test]
fn init_records_state_and_reports_no_changes() -> Result<()> {
    let (template, project) = fixture()?;

    let state = TemplateState::load(project.path()).expect("state.json should exist");
    assert_eq!(
        state.initialized_with.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert!(state.files.contains_key("compose.yml"));
    // Runtime placeholders are not part of the tracked template.
    assert!(!state.files.keys().any(|k| k.starts_with("logs/")));

    let changes = template::diff(project.path(), template.path())?;
    assert!(changes.is_empty(), "{:?}", changes);
    Ok(())
}

#[test]
fn state_lives_at_the_project_root_outside_htdocs() -> Result<()> {
    let (_template, project) = fixture()?;
    assert!(project.path().join(".docker-control/state.json").is_file());
    assert!(!project.path().join("htdocs").exists());
    Ok(())
}

#[test]
fn template_only_change_is_a_safe_update() -> Result<()> {
    let (template, project) = fixture()?;
    write(template.path(), "config/php.ini", "memory_limit = 512M\n")?;

    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(changes, vec![Change::SafeUpdate("config/php.ini".into())]);
    Ok(())
}

#[test]
fn project_only_change_reports_nothing() -> Result<()> {
    // The false-positive case this whole mechanism exists to prevent: the user
    // edited a file, the template did not move, so there is nothing to offer.
    let (template, project) = fixture()?;
    write(project.path(), "config/php.ini", "memory_limit = 1G\n")?;

    let changes = template::diff(project.path(), template.path())?;
    assert!(changes.is_empty(), "{:?}", changes);
    Ok(())
}

#[test]
fn changes_on_both_sides_are_a_conflict() -> Result<()> {
    let (template, project) = fixture()?;
    write(template.path(), "config/php.ini", "memory_limit = 512M\n")?;
    write(project.path(), "config/php.ini", "memory_limit = 1G\n")?;

    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(changes, vec![Change::Conflict("config/php.ini".into())]);
    Ok(())
}

#[test]
fn project_matching_the_new_template_is_not_actionable() -> Result<()> {
    let (template, project) = fixture()?;
    write(template.path(), "config/php.ini", "memory_limit = 512M\n")?;
    write(project.path(), "config/php.ini", "memory_limit = 512M\n")?;

    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(
        changes,
        vec![Change::AlreadyApplied("config/php.ini".into())]
    );
    assert!(template::Summary::from_changes(&changes).is_empty());
    Ok(())
}

#[test]
fn new_template_file_is_a_safe_add() -> Result<()> {
    let (template, project) = fixture()?;
    write(template.path(), "config/new.conf", "new\n")?;

    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(changes, vec![Change::SafeAdd("config/new.conf".into())]);
    Ok(())
}

#[test]
fn hand_edited_secrets_are_never_reported() -> Result<()> {
    // secrets/ and config/htpasswd hold per-project values, so a divergence is
    // expected rather than something to offer to overwrite.
    let (template, project) = fixture()?;
    write(project.path(), "secrets/db_pw.txt", "a-real-password")?;
    write(project.path(), "config/htpasswd", "user:realhash")?;
    write(template.path(), "secrets/db_pw.txt", "999999")?;
    write(template.path(), "config/htpasswd", "user:otherhash")?;

    let changes = template::diff(project.path(), template.path())?;
    assert!(changes.is_empty(), "{:?}", changes);
    Ok(())
}

#[test]
fn new_env_dist_keys_are_reported_against_the_projects_env() -> Result<()> {
    let (template, project) = fixture()?;
    write(
        template.path(),
        ".env-dist",
        "BASE_DOMAIN=example.lvh.me\nPHP_VERSION=8.2\nSELF_SIGNED_HOST=1\n",
    )?;

    // The project's reference copy of `.env-dist` is refreshed, *and* the key it
    // gained is reported against `.env` — which is never written for the user.
    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(
        changes,
        vec![
            Change::SafeUpdate(".env-dist".into()),
            Change::EnvKeys(vec!["SELF_SIGNED_HOST".into()]),
        ]
    );
    Ok(())
}

#[test]
fn missing_env_keys_survive_a_stamp() -> Result<()> {
    // `update` reports a missing `.env` key but never writes it, so the report
    // has to outlive the stamp that records the rest of the template as applied.
    // Otherwise the one change that needs the user is the one that gets forgotten.
    let (template, project) = fixture()?;
    write(
        template.path(),
        ".env-dist",
        "BASE_DOMAIN=example.lvh.me\nPHP_VERSION=8.2\nSELF_SIGNED_HOST=1\n",
    )?;
    template::stamp(project.path(), template.path(), false)?;

    // The fingerprint now matches, so the tracked-file fast path is taken.
    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(
        changes,
        vec![Change::EnvKeys(vec!["SELF_SIGNED_HOST".into()])],
        "the .env key report must not be silenced by stamping"
    );

    // Adding the key clears it.
    write(
        project.path(),
        ".env",
        "BASE_DOMAIN=example.lvh.me\nPHP_VERSION=8.2\nSELF_SIGNED_HOST=0\n",
    )?;
    assert!(template::diff(project.path(), template.path())?.is_empty());
    Ok(())
}

#[test]
fn removed_upstream_clears_once_the_project_drops_the_file() -> Result<()> {
    let (template, project) = fixture()?;
    fs::remove_file(template.path().join("config/php.ini"))?;

    assert_eq!(
        template::diff(project.path(), template.path())?,
        vec![Change::RemovedUpstream("config/php.ini".into())]
    );

    // Deleting the local copy resolves it; re-stamping also drops it from the
    // base, so either route clears the warning rather than repeating forever.
    fs::remove_file(project.path().join("config/php.ini"))?;
    assert!(template::diff(project.path(), template.path())?.is_empty());
    Ok(())
}

#[test]
fn env_dist_value_change_produces_no_env_report() -> Result<()> {
    // Only *missing keys* are actionable for `.env`; a changed default in
    // `.env-dist` says nothing about the project's own values. The reference copy
    // is still refreshed like any other template file.
    let (template, project) = fixture()?;
    write(
        template.path(),
        ".env-dist",
        "BASE_DOMAIN=other.lvh.me\nPHP_VERSION=8.5\n",
    )?;

    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(changes, vec![Change::SafeUpdate(".env-dist".into())]);
    assert!(
        !changes.iter().any(|c| matches!(c, Change::EnvKeys(_))),
        "a value-only change must not ask the user to touch .env"
    );
    Ok(())
}

#[test]
fn new_gitignore_dist_entries_are_reported_against_the_projects_gitignore() -> Result<()> {
    // `init`/`update` consume `.gitignore-dist`, so the project never holds one;
    // the comparison is against the `.gitignore` it produced.
    let (template, project) = fixture()?;
    write(
        template.path(),
        ".gitignore-dist",
        "htdocs\nlogs\nreleases\n",
    )?;

    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(
        changes,
        vec![Change::GitignoreEntries(vec!["releases".into()])]
    );
    Ok(())
}

#[test]
fn reordered_gitignore_is_not_reported() -> Result<()> {
    // `update` sorts and dedups the merged `.gitignore`, so a project whose file
    // is in a different order than the seed is still fully in sync.
    let (template, project) = fixture()?;
    write(project.path(), ".gitignore", "logs\nhtdocs\nvendor\n")?;
    write(template.path(), "config/php.ini", "memory_limit = 512M\n")?;

    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(changes, vec![Change::SafeUpdate("config/php.ini".into())]);
    Ok(())
}

#[test]
fn file_dropped_from_the_template_is_reported() -> Result<()> {
    let (template, project) = fixture()?;
    fs::remove_file(template.path().join("config/php.ini"))?;

    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(
        changes,
        vec![Change::RemovedUpstream("config/php.ini".into())]
    );
    Ok(())
}

#[test]
fn legacy_project_backfills_matching_files_and_flags_the_rest() -> Result<()> {
    let (template, project) = fixture()?;

    // A project predating the state file: no recorded base at all.
    fs::remove_dir_all(project.path().join(".docker-control"))?;
    assert!(TemplateState::load(project.path()).is_none());

    // One file diverges; everything else still matches the template.
    write(project.path(), "config/php.ini", "memory_limit = 1G\n")?;

    let theirs = template::manifest(template.path())?;
    let base = template::derive_base(project.path(), &theirs)?;
    assert_eq!(
        base.get("compose.yml"),
        Some(&Some(theirs["compose.yml"].clone())),
        "an in-sync file should get a real base"
    );
    assert_eq!(
        base.get("config/php.ini"),
        Some(&None),
        "a diverging file must not be given a guessed base"
    );

    // Only the unattributable file is reported, and never as a conflict.
    let changes = template::diff(project.path(), template.path())?;
    assert_eq!(changes, vec![Change::Unknown("config/php.ini".into())]);
    Ok(())
}

#[test]
fn diff_never_writes_to_the_project() -> Result<()> {
    // `status` and the start-up notice call diff(); neither may leave a modified
    // file behind in the user's repo.
    let (template, project) = fixture()?;
    fs::remove_dir_all(project.path().join(".docker-control"))?;
    write(template.path(), "config/php.ini", "memory_limit = 512M\n")?;

    template::diff(project.path(), template.path())?;
    assert!(!project.path().join(".docker-control").exists());
    Ok(())
}

#[test]
fn corrupt_state_is_treated_as_missing_rather_than_fatal() -> Result<()> {
    let (template, project) = fixture()?;
    write(project.path(), ".docker-control/state.json", "{ not json")?;

    assert!(TemplateState::load(project.path()).is_none());
    // Falls back to deriving a base, so the command still works.
    let changes = template::diff(project.path(), template.path())?;
    assert!(changes.is_empty(), "{:?}", changes);
    Ok(())
}

#[test]
fn restamping_preserves_the_original_init_version() -> Result<()> {
    let (template, project) = fixture()?;
    write(template.path(), "config/php.ini", "memory_limit = 512M\n")?;

    template::stamp(project.path(), template.path(), false)?;

    let state = TemplateState::load(project.path()).expect("state should exist");
    assert_eq!(
        state.initialized_with.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    // The new template is now the base, so nothing is pending.
    let changes = template::diff(project.path(), template.path())?;
    assert!(changes.is_empty(), "{:?}", changes);
    Ok(())
}

#[test]
fn the_shipped_template_hashes_cleanly() -> Result<()> {
    // Guards the real template against the exclusion rules silently breaking.
    let files = template::manifest(Path::new("template"))?;
    assert!(files.contains_key("compose.yml"));
    assert!(files.contains_key("config/apache-sites/default.conf"));
    assert!(
        !files.keys().any(|k| k.starts_with("logs/")),
        "runtime dirs must not be tracked"
    );
    assert!(
        !files.keys().any(|k| k.starts_with("volumes/")),
        "runtime dirs must not be tracked"
    );
    assert!(!template::fingerprint(&files).is_empty());
    Ok(())
}
