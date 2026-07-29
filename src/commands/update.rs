use crate::docker;
use crate::template::{self, Change};
use crate::ui;
use crate::utils;
use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOptions {
    /// Skip the confirmation prompt (required for non-interactive use).
    pub yes: bool,
    /// Report what would change and exit without touching anything.
    pub check: bool,
    /// Overwrite every template-owned file regardless of local edits — the
    /// behaviour `update` had before it tracked template state.
    pub force_template: bool,
}

/// Abstracts the "update the project?" confirmation so tests can inject a fixed
/// answer instead of blocking on a real prompt, matching the
/// `UpgradePromptProvider`/`MergePromptProvider` pattern used elsewhere in this
/// codebase for interactive `inquire` prompts.
pub trait UpdatePromptProvider {
    fn confirm_update(&self) -> bool;
}

pub struct InteractiveUpdatePromptProvider;

impl UpdatePromptProvider for InteractiveUpdatePromptProvider {
    fn confirm_update(&self) -> bool {
        inquire::Confirm::new("Continue updating THIS PROJECT from the template?")
            // Safe default: don't rewrite the project unless the user opts in.
            .with_default(false)
            .prompt()
            .unwrap_or(false)
    }
}

/// What to do with a file that changed both in the project and in the template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    KeepMine,
    TakeTheirs,
    /// Keep the project's file and drop the template's version beside it as
    /// `<name>.dist` for the user to merge by hand.
    WriteDist,
}

/// How to treat the files of a project that has no recorded template state, and
/// whose divergences therefore can't be attributed to either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownPolicy {
    ReviewEach,
    TakeTemplate,
    KeepMine,
}

/// Per-file decisions during an update. Separate from [`UpdatePromptProvider`]
/// so the pre-existing confirmation tests keep their one-method mock.
pub trait ConflictPrompt {
    fn resolve(&self, rel: &str, mine: &Path, theirs: &Path) -> ConflictResolution;
    fn unknown_policy(&self, count: usize) -> UnknownPolicy;
}

pub struct InteractiveConflictPrompt;

impl ConflictPrompt for InteractiveConflictPrompt {
    fn resolve(&self, rel: &str, mine: &Path, theirs: &Path) -> ConflictResolution {
        const KEEP: &str = "keep my version";
        const TAKE: &str = "take the template version";
        const DIST: &str = "keep mine, write the template copy as *.dist";
        const DIFF: &str = "show diff";

        loop {
            ui::warning(format!("Conflict: {}", rel));
            let choice = inquire::Select::new(
                "You modified this file and the template changed it too:",
                vec![KEEP, TAKE, DIST, DIFF],
            )
            .prompt();

            match choice {
                Ok(KEEP) => return ConflictResolution::KeepMine,
                Ok(TAKE) => return ConflictResolution::TakeTheirs,
                Ok(DIST) => return ConflictResolution::WriteDist,
                Ok(DIFF) => show_diff(mine, theirs),
                // A cancelled prompt (Esc/Ctrl-C) must not silently overwrite.
                _ => return ConflictResolution::KeepMine,
            }
        }
    }

    fn unknown_policy(&self, count: usize) -> UnknownPolicy {
        const REVIEW: &str = "review each file";
        const TAKE: &str = "take the template version for all (previous behaviour)";
        const KEEP: &str = "keep my versions for all";

        ui::warning(format!(
            "This project has no recorded template state, and {} file(s) differ from the current template.",
            count
        ));
        ui::info("Without a recorded base, local edits and template changes can't be told apart.");

        match inquire::Select::new("How should they be handled?", vec![REVIEW, TAKE, KEEP]).prompt()
        {
            Ok(TAKE) => UnknownPolicy::TakeTemplate,
            Ok(REVIEW) => UnknownPolicy::ReviewEach,
            _ => UnknownPolicy::KeepMine,
        }
    }
}

/// Shows a unified diff between the project's file and the template's. Uses the
/// system `diff`, which exits 1 when the files differ — expected here, so only a
/// failure to run it at all is worth reporting.
fn show_diff(mine: &Path, theirs: &Path) {
    match Command::new("diff")
        .arg("-u")
        .arg(mine)
        .arg(theirs)
        .status()
    {
        Ok(_) => {}
        Err(e) => {
            ui::warning(format!("Could not run `diff`: {}", e));
            ui::info(format!("Yours:    {}", mine.display()));
            ui::info(format!("Template: {}", theirs.display()));
        }
    }
}

/// Decide whether to proceed with the destructive project update. `yes`
/// bypasses the prompt entirely; otherwise an interactive terminal is warned
/// (naming `upgrade` as the likely intended command) and prompted, while a
/// non-interactive run is refused so scripts/CI can't silently rewrite a
/// project. Pure (no filesystem work) so the decision matrix is unit-testable.
fn confirm_or_abort(
    yes: bool,
    is_interactive: bool,
    prompt: &dyn UpdatePromptProvider,
) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !is_interactive {
        bail!(
            "refusing to rewrite the project non-interactively. \
             Re-run with --yes to confirm. \
             (To upgrade docker-control itself, run `upgrade`.)"
        );
    }
    ui::warning("This rewrites THIS PROJECT's files from the template (a backup is created).");
    ui::info("To upgrade docker-control itself instead, run `upgrade`.");
    Ok(prompt.confirm_update())
}

pub fn execute(project_dir: &Path, opts: UpdateOptions) -> Result<()> {
    let template_dir = template::resolve_dir()?;

    if opts.check {
        return report_pending(project_dir, &template_dir);
    }

    let is_interactive = std::io::stdin().is_terminal();
    if !confirm_or_abort(opts.yes, is_interactive, &InteractiveUpdatePromptProvider)? {
        ui::info("Update cancelled.");
        return Ok(());
    }

    apply_template_update(
        project_dir,
        &template_dir,
        opts,
        is_interactive,
        &InteractiveConflictPrompt,
    )
}

/// `--check`: report what an update would do, change nothing, and exit non-zero
/// when anything is pending so it can gate a CI step.
fn report_pending(project_dir: &Path, template_dir: &Path) -> Result<()> {
    let changes = template::diff(project_dir, template_dir)?;
    let summary = template::Summary::from_changes(&changes);

    if summary.is_empty() {
        ui::success("Project template is up to date.");
        return Ok(());
    }

    ui::warning("The project template has pending changes:");
    summary.print(false);

    for path in summary
        .safe
        .iter()
        .chain(summary.conflicts.iter())
        .chain(summary.unknown.iter())
    {
        println!();
        ui::info(format!("--- {} ---", path));
        show_diff(&project_dir.join(path), &template_dir.join(path));
    }

    Err(anyhow!("template changes pending"))
}

/// The set of files an update will act on, once conflicts have been resolved.
#[derive(Debug, Default)]
struct ApplyPlan {
    /// Template-relative paths to copy over the project.
    take: Vec<String>,
    /// Paths to copy as `<name>.dist` beside the project's own file.
    sidecars: Vec<String>,
    /// Paths left as-is, reported so the outcome isn't silent.
    kept: Vec<String>,
}

impl ApplyPlan {
    fn is_noop(&self) -> bool {
        self.take.is_empty() && self.sidecars.is_empty()
    }
}

/// Turns classified changes into concrete actions, prompting for anything
/// ambiguous. Runs before the project is stopped or backed up, so the user
/// isn't left with a halted project while deciding.
fn plan_apply(
    changes: &[Change],
    project_dir: &Path,
    template_dir: &Path,
    is_interactive: bool,
    prompt: &dyn ConflictPrompt,
) -> ApplyPlan {
    let mut plan = ApplyPlan::default();

    let unknown: Vec<&String> = changes
        .iter()
        .filter_map(|c| match c {
            Change::Unknown(p) => Some(p),
            _ => None,
        })
        .collect();

    // Ask once up front rather than firing one prompt per file for a project
    // that predates the state file, where every file may be unattributable.
    let unknown_policy = if unknown.is_empty() {
        UnknownPolicy::KeepMine
    } else if is_interactive {
        prompt.unknown_policy(unknown.len())
    } else {
        // Nothing can be attributed and nobody can be asked: never overwrite.
        UnknownPolicy::KeepMine
    };

    let ask = |path: &String| {
        if is_interactive {
            prompt.resolve(path, &project_dir.join(path), &template_dir.join(path))
        } else {
            // `--yes` must never silently discard local work, so the
            // non-interactive default keeps the project's file and leaves the
            // template's version alongside it.
            ConflictResolution::WriteDist
        }
    };

    for change in changes {
        let (path, resolution) = match change {
            Change::SafeUpdate(path) | Change::SafeAdd(path) => {
                (path.clone(), ConflictResolution::TakeTheirs)
            }
            Change::Conflict(path) => (path.clone(), ask(path)),
            Change::Unknown(path) => match unknown_policy {
                UnknownPolicy::TakeTemplate => (path.clone(), ConflictResolution::TakeTheirs),
                UnknownPolicy::KeepMine => (path.clone(), ConflictResolution::KeepMine),
                UnknownPolicy::ReviewEach => (path.clone(), ask(path)),
            },
            // Bringing in `.gitignore-dist` is what lets `merge_gitignore` fold
            // the new entries into the project's own `.gitignore`; the seed
            // itself is consumed and deleted there, never left behind.
            Change::GitignoreEntries(_) => (
                template::GITIGNORE_DIST.to_string(),
                ConflictResolution::TakeTheirs,
            ),
            // `.env` is never written for the user: the keys are reported and
            // they decide what values to give them.
            Change::EnvKeys(_) => continue,
            // Reported by the caller; never acted on automatically.
            Change::RemovedUpstream(_) | Change::AlreadyApplied(_) => continue,
        };

        match resolution {
            ConflictResolution::TakeTheirs => plan.take.push(path),
            ConflictResolution::WriteDist => {
                plan.sidecars.push(path.clone());
                plan.kept.push(path);
            }
            ConflictResolution::KeepMine => plan.kept.push(path),
        }
    }

    plan
}

fn apply_template_update(
    project_dir: &Path,
    template_dir: &Path,
    opts: UpdateOptions,
    is_interactive: bool,
    prompt: &dyn ConflictPrompt,
) -> Result<()> {
    let changes = if opts.force_template {
        Vec::new()
    } else {
        template::diff(project_dir, template_dir)?
    };
    let summary = template::Summary::from_changes(&changes);

    let plan = if opts.force_template {
        ui::warning("--force-template: overwriting every template-owned file.");
        ApplyPlan::default()
    } else {
        if summary.is_empty() {
            ui::success("Project template is already up to date.");
            // Re-stamp anyway when the recorded base is stale (the project was
            // brought in line by hand). Left alone it would surface as a
            // conflict the next time the template moves.
            if changes.iter().any(|c| !c.is_actionable()) {
                template::stamp(project_dir, template_dir, false)?;
            }
            return Ok(());
        }

        ui::warning("The project template has changed:");
        summary.print(false);

        let plan = plan_apply(&changes, project_dir, template_dir, is_interactive, prompt);
        if plan.is_noop() {
            ui::info("Nothing to copy — keeping every local file.");
            for path in &plan.kept {
                ui::info(format!("Kept your version of {}", path));
            }
            // Still record the current template as the base. "Keep mine" is a
            // decision, not a deferral: without the stamp the same prompt would
            // reappear on every run, and a file merely dropped from the template
            // would warn forever with nothing to apply. Recording it means the
            // local edit now sits on top of the current template, so the next
            // genuine upstream change to that file is a fresh conflict.
            template::stamp(project_dir, template_dir, false)?;
            report_unwritten(&summary);
            return Ok(());
        }
        plan
    };

    ui::info("Updating project with latest template...");

    let was_running = docker::is_running(project_dir);
    if was_running {
        ui::info("Stopping project...");
        docker::execute_compose(project_dir, &["down"])?;
    }

    create_backup(project_dir)?;

    if opts.force_template {
        sync_all(template_dir, project_dir)?;
    } else {
        sync_files(template_dir, project_dir, &plan.take)?;
        for path in &plan.sidecars {
            write_sidecar(template_dir, project_dir, path)?;
        }
    }

    // Refresh the Capistrano Dockerfile if this project already uses one (it's opt-in
    // and not part of the template, so it's kept in sync here rather than via
    // copy_recursive). Rebuild the image only when the file actually changed.
    let capistrano_build_dir = project_dir.join("build/capistrano");
    if crate::commands::migrate::write_capistrano_dockerfile(&capistrano_build_dir)? {
        ui::info("Rebuilding Capistrano image (Dockerfile changed)...");
        docker::execute_compose(project_dir, &["build", "capistrano"])?;
    }

    merge_gitignore(project_dir)?;

    // `.env-dist` is deliberately left in the project: `init` ships it as the
    // reference list of available keys and writes `.env` separately, so it is
    // kept in sync like any other template file rather than being consumed.

    template::stamp(project_dir, template_dir, false)?;

    if was_running {
        ui::info("Restarting project...");
        docker::execute_compose(project_dir, &["up", "-d"])?;
    }

    for path in &plan.kept {
        ui::info(format!("Kept your version of {}", path));
    }
    for path in &plan.sidecars {
        ui::info(format!("Wrote {}.dist for manual merge", path));
    }
    report_unwritten(&summary);
    ui::success("Project updated successfully.");

    Ok(())
}

/// Spells out the things an update reports but never performs, at the point
/// where the stamp is about to stop mentioning some of them.
fn report_unwritten(summary: &template::Summary) {
    if !summary.removed.is_empty() {
        ui::info(format!(
            "Left in place, but no longer part of the template (delete if unused): {}",
            summary.removed.join(", ")
        ));
    }
    if !summary.env_keys.is_empty() {
        // Reported until the keys are actually present: the `.env` check does not
        // depend on the recorded base, so stamping does not silence it.
        ui::warning(format!(
            "Add these keys to .env yourself (values are project-specific): {}",
            summary.env_keys.join(", ")
        ));
    }
}

fn create_backup(project_dir: &Path) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    let backup_name = format!("backup_{}", now);
    let backup_dir = project_dir.join(&backup_name);

    ui::info(format!("Creating backup {}...", backup_name));
    fs::create_dir_all(&backup_dir)?;
    utils::exclude_from_phpstorm(project_dir, &backup_name)?;

    // Backup current files (excluding what bash excludes)
    let backup_excludes = ["backup_*", ".git", "htdocs", "logs", "volumes"];
    copy_recursive(project_dir, &backup_dir, &backup_excludes)
}

/// Copies exactly `paths` (template-relative) from the template into the
/// project, via `sudo rsync`. Containers create files under the project as
/// root/www-data that the host user often can't overwrite, so a plain copy fails
/// with EACCES; `rsync -a` run as root can always overwrite them, and because it
/// preserves the template's (host-user) ownership, refreshed files end up owned
/// by the invoking user rather than root — clearing the permission problem going
/// forward.
///
/// `-R` with the `/./` marker reproduces each path under the project root
/// (creating intermediate directories), and `-I` defeats rsync's size+mtime
/// quick check — the paths were selected by content hash, so rsync must not
/// second-guess them. Without it, a same-size edit with a matching mtime is
/// silently skipped.
fn sync_files(template_dir: &Path, project_dir: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    ui::info(format!(
        "Applying {} template file(s) (may prompt for sudo password)...",
        paths.len()
    ));

    let sources: Vec<String> = paths
        .iter()
        .map(|path| format!("{}/./{}", template_dir.display(), path))
        .collect();
    let destination = format!("{}/", project_dir.display());

    let mut args = vec!["rsync", "-aIR"];
    args.extend_from_slice(template::SYNC_EXCLUDES);
    args.extend(sources.iter().map(String::as_str));
    args.push(&destination);

    utils::sudo::run(&args)
}

/// `--force-template`: the pre-state-tracking behaviour — copy the whole
/// template over the project. Runtime data and per-project files are still
/// excluded; resetting a real database password to the template's placeholder
/// was never intended behaviour.
///
/// `-I` is essential here, not just an optimisation defeat: without it rsync's
/// size+mtime quick check skips a file whose size happens to match and whose
/// mtime lands in the same second as the template's — so a `--force` that
/// silently declines to overwrite. Since this path exists precisely to overwrite
/// unconditionally, the quick check must not get a vote.
fn sync_all(template_dir: &Path, project_dir: &Path) -> Result<()> {
    ui::info("Applying template changes (may prompt for sudo password)...");
    let template_src = format!("{}/", template_dir.display());
    let project_dst = format!("{}/", project_dir.display());

    let mut args = vec!["rsync", "-aI"];
    args.extend_from_slice(template::SYNC_EXCLUDES);
    args.push(&template_src);
    args.push(&project_dst);

    utils::sudo::run(&args)
}

/// Drops the template's version of a conflicting file beside the project's own
/// as `<name>.dist`. Falls back to `sudo cp` when the target directory is owned
/// by a container user and the host user can't write it — followed by `chown`,
/// since the whole point of the sidecar is that the user merges it by hand and a
/// root-owned file they can't edit or delete would defeat that.
fn write_sidecar(template_dir: &Path, project_dir: &Path, path: &str) -> Result<()> {
    let source = template_dir.join(path);
    let target = project_dir.join(format!("{}.dist", path));

    if fs::copy(&source, &target).is_ok() {
        return Ok(());
    }

    let target_arg = target.display().to_string();
    utils::sudo::run(&["cp", &source.display().to_string(), &target_arg])
        .with_context(|| format!("Failed to write {:?}", target))?;

    let owner = unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) };
    utils::sudo::run(&["chown", &owner, &target_arg])
        .with_context(|| format!("Failed to take ownership of {:?}", target))
}

/// Folds `.gitignore-dist` into the project's `.gitignore` and removes it, so
/// the template can add ignore rules without clobbering project-specific ones.
fn merge_gitignore(project_dir: &Path) -> Result<()> {
    let gitignore_dist = project_dir.join(".gitignore-dist");
    let gitignore = project_dir.join(".gitignore");

    if !gitignore_dist.exists() {
        return Ok(());
    }

    let mut content = fs::read_to_string(&gitignore).unwrap_or_default();
    let dist_content = fs::read_to_string(&gitignore_dist)?;
    content.push('\n');
    content.push_str(&dist_content);

    let mut lines: Vec<String> = content
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();
    lines.sort();
    lines.dedup();

    fs::write(&gitignore, lines.join("\n"))?;
    fs::remove_file(gitignore_dist)?;
    Ok(())
}

/// Snapshots the immediate children of `src` into `dst`, skipping names matched
/// by `excludes` (a single trailing `*` acts as a prefix wildcard). Used to back
/// up the project before a template update.
fn copy_recursive(src: &Path, dst: &Path, excludes: &[&str]) -> Result<()> {
    for entry in WalkDir::new(src).min_depth(1).max_depth(1) {
        let entry = entry?;
        let path = entry.path();
        let file_name = path.file_name().unwrap().to_string_lossy();

        let mut should_exclude = false;
        for exclude in excludes {
            if exclude.contains('*') {
                let pattern = exclude.replace('*', "");
                if file_name.starts_with(&pattern) {
                    should_exclude = true;
                    break;
                }
            } else if file_name == *exclude {
                should_exclude = true;
                break;
            }
        }

        if should_exclude {
            continue;
        }

        let target = dst.join(&*file_name);
        if path.is_dir() {
            let mut options = fs_extra::dir::CopyOptions::new();
            options.copy_inside = true;
            options.overwrite = true;
            fs_extra::dir::copy(path, dst, &options)
                .map_err(|e| anyhow::anyhow!("Failed to copy dir {:?}: {}", path, e))?;
        } else {
            fs::copy(path, &target)
                .with_context(|| format!("Failed to copy file to {:?}", target))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockPrompt(bool);
    impl UpdatePromptProvider for MockPrompt {
        fn confirm_update(&self) -> bool {
            self.0
        }
    }

    struct MockConflictPrompt {
        resolution: ConflictResolution,
        policy: UnknownPolicy,
    }

    impl ConflictPrompt for MockConflictPrompt {
        fn resolve(&self, _rel: &str, _mine: &Path, _theirs: &Path) -> ConflictResolution {
            self.resolution
        }
        fn unknown_policy(&self, _count: usize) -> UnknownPolicy {
            self.policy
        }
    }

    fn mock(resolution: ConflictResolution, policy: UnknownPolicy) -> MockConflictPrompt {
        MockConflictPrompt { resolution, policy }
    }

    fn plan(changes: &[Change], interactive: bool, prompt: &dyn ConflictPrompt) -> ApplyPlan {
        plan_apply(
            changes,
            Path::new("/project"),
            Path::new("/template"),
            interactive,
            prompt,
        )
    }

    #[test]
    fn yes_flag_proceeds_without_consulting_prompt() {
        // A prompt that would say "no" must be ignored when --yes is set,
        // regardless of interactivity.
        assert!(confirm_or_abort(true, true, &MockPrompt(false)).unwrap());
        assert!(confirm_or_abort(true, false, &MockPrompt(false)).unwrap());
    }

    #[test]
    fn non_interactive_without_yes_is_refused() {
        let err = confirm_or_abort(false, false, &MockPrompt(true)).unwrap_err();
        assert!(err.to_string().contains("--yes"));
    }

    #[test]
    fn interactive_follows_prompt_answer() {
        assert!(confirm_or_abort(false, true, &MockPrompt(true)).unwrap());
        assert!(!confirm_or_abort(false, true, &MockPrompt(false)).unwrap());
    }

    #[test]
    fn safe_changes_are_applied_without_prompting() {
        let changes = vec![
            Change::SafeUpdate("compose.yml".into()),
            Change::SafeAdd("config/new.conf".into()),
        ];
        // A prompt that would keep local files must not be consulted at all.
        let plan = plan(
            &changes,
            true,
            &mock(ConflictResolution::KeepMine, UnknownPolicy::KeepMine),
        );
        assert_eq!(plan.take, vec!["compose.yml", "config/new.conf"]);
        assert!(plan.kept.is_empty());
    }

    #[test]
    fn conflict_honours_the_chosen_resolution() {
        let changes = vec![Change::Conflict("config/php.ini".into())];

        let taken = plan(
            &changes,
            true,
            &mock(ConflictResolution::TakeTheirs, UnknownPolicy::KeepMine),
        );
        assert_eq!(taken.take, vec!["config/php.ini"]);
        assert!(taken.sidecars.is_empty());

        let kept = plan(
            &changes,
            true,
            &mock(ConflictResolution::KeepMine, UnknownPolicy::KeepMine),
        );
        assert!(kept.take.is_empty());
        assert_eq!(kept.kept, vec!["config/php.ini"]);

        let sidecar = plan(
            &changes,
            true,
            &mock(ConflictResolution::WriteDist, UnknownPolicy::KeepMine),
        );
        assert!(sidecar.take.is_empty());
        assert_eq!(sidecar.sidecars, vec!["config/php.ini"]);
    }

    #[test]
    fn non_interactive_conflict_keeps_local_and_writes_sidecar() {
        // --yes previously overwrote local edits silently; an unattended run
        // must never lose work, so the template copy goes beside it instead.
        let changes = vec![Change::Conflict("config/php.ini".into())];
        let plan = plan(
            &changes,
            false,
            &mock(ConflictResolution::TakeTheirs, UnknownPolicy::TakeTemplate),
        );
        assert!(plan.take.is_empty());
        assert_eq!(plan.sidecars, vec!["config/php.ini"]);
        assert_eq!(plan.kept, vec!["config/php.ini"]);
    }

    #[test]
    fn unknown_policy_applies_to_every_unattributable_file() {
        let changes = vec![
            Change::Unknown("compose.yml".into()),
            Change::Unknown("config/php.ini".into()),
        ];

        let take = plan(
            &changes,
            true,
            &mock(ConflictResolution::KeepMine, UnknownPolicy::TakeTemplate),
        );
        assert_eq!(take.take, vec!["compose.yml", "config/php.ini"]);

        let keep = plan(
            &changes,
            true,
            &mock(ConflictResolution::KeepMine, UnknownPolicy::KeepMine),
        );
        assert!(keep.take.is_empty());
        assert_eq!(keep.kept, vec!["compose.yml", "config/php.ini"]);

        // "review each" defers to the per-file resolution.
        let review = plan(
            &changes,
            true,
            &mock(ConflictResolution::TakeTheirs, UnknownPolicy::ReviewEach),
        );
        assert_eq!(review.take, vec!["compose.yml", "config/php.ini"]);
    }

    #[test]
    fn non_interactive_never_overwrites_unattributable_files() {
        let changes = vec![Change::Unknown("compose.yml".into())];
        let plan = plan(
            &changes,
            false,
            &mock(ConflictResolution::TakeTheirs, UnknownPolicy::TakeTemplate),
        );
        assert!(plan.take.is_empty());
        assert_eq!(plan.kept, vec!["compose.yml"]);
    }

    #[test]
    fn reported_only_changes_produce_no_actions() {
        let changes = vec![
            Change::RemovedUpstream("config/old.conf".into()),
            Change::EnvKeys(vec!["SELF_SIGNED_HOST".into()]),
            Change::AlreadyApplied("compose.yml".into()),
        ];
        let plan = plan(
            &changes,
            true,
            &mock(ConflictResolution::TakeTheirs, UnknownPolicy::TakeTemplate),
        );
        assert!(plan.is_noop());
        assert!(plan.kept.is_empty());
    }
}
