//! Tracks which version of the project template a project is synced to, so
//! `update` can tell what genuinely changed instead of overwriting everything.
//!
//! The state file records, at every sync point (`init`, `update`, `migrate`),
//! **the hashes the template had at that moment**. That gives a merge base, so
//! each file yields three values — `base` (recorded), `theirs` (template now)
//! and `mine` (project now) — and the classification below is an ordinary
//! three-way merge against a template that has no repo of its own.
//!
//! Comparing docker-control's version number instead would be useless as a
//! trigger: the template changes in roughly one release out of five, so a
//! version bump on its own says nothing about whether anything needs applying.

use crate::ui;
use crate::utils;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Directory holding docker-control's own state for a project. Lives at the
/// project root, *outside* `htdocs/`: it describes the wrapper project
/// (`compose.yml`, `config/`, `secrets/`), which is a different git repo from
/// the application in `htdocs/`, and a project need not have `htdocs/` checked
/// out at all. Unrelated to `htdocs/.docker-control/`, which is app-level
/// config (see `config::DeployConfig`).
pub const STATE_DIR: &str = ".docker-control";
const STATE_FILE: &str = "state.json";

/// Template files whose project copy holds per-project values. They ship as
/// placeholders (`secrets/db_pw.txt` is `123456`), are seeded once at `init`,
/// and must never be overwritten afterwards — nor reported, or a project that
/// legitimately edited them would show a conflict forever.
fn is_preserve_local(rel: &str) -> bool {
    rel == "config/htpasswd" || (rel.starts_with("secrets/") && rel.ends_with(".txt"))
}

pub const ENV_DIST: &str = ".env-dist";
pub const GITIGNORE_DIST: &str = ".gitignore-dist";

/// `.gitignore-dist` is consumed on apply: `init` renames it to `.gitignore`
/// (`commands::init`) and `update` merges it in and deletes it
/// (`commands::update`), so the project never holds a copy to hash. It is
/// therefore excluded from the three-way comparison and checked by content
/// against the `.gitignore` it produced — see [`derived_changes`]. Hashing it
/// would report every project as permanently out of date, since a
/// deliberately-absent project copy is indistinguishable from a missing base.
///
/// `.env-dist` is deliberately *not* in this category. `init` copies it in and
/// leaves it (writing `.env` separately from its prompt answers), so the project
/// does have a copy and ordinary hash comparison keeps that reference current.
/// The keys it gained are reported against `.env` on top of that.
fn is_consumed(rel: &str) -> bool {
    rel == GITIGNORE_DIST
}

/// Runtime data directories. The template ships only `.gitkeep` placeholders
/// here and the sync already skips them.
fn is_runtime(rel: &str) -> bool {
    rel.starts_with("logs/") || rel.starts_with("volumes/")
}

/// rsync `--exclude` patterns covering what [`is_runtime`] and
/// [`is_preserve_local`] skip. Kept next to them so the two matchers — ours for
/// classification, rsync's for the actual copy — can't drift apart.
///
/// The patterns are written to match those functions exactly: a leading `/`
/// anchors to the transfer root (both predicates only match at the root, while
/// an unanchored `logs` would match a `logs/` directory at any depth), and `**`
/// crosses `/` (`is_preserve_local` matches `secrets/**.txt` at any depth, which
/// a single `*` would not).
pub const SYNC_EXCLUDES: &[&str] = &[
    "--exclude=/logs/",
    "--exclude=/volumes/",
    "--exclude=/secrets/**.txt",
    "--exclude=/config/htpasswd",
];

/// What happened to one template file since the project last synced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Template changed it, the project never touched it: safe to overwrite.
    SafeUpdate(String),
    /// New in the template and absent from the project.
    SafeAdd(String),
    /// Both the template and the project changed it since the last sync.
    Conflict(String),
    /// Gone from the template. Reported only — never deleted automatically.
    RemovedUpstream(String),
    /// The project already matches the new template; only the recorded base is
    /// stale. Not worth telling the user about, but worth re-stamping.
    AlreadyApplied(String),
    /// No recorded base, so local edits and template drift are
    /// indistinguishable. Only produced for projects predating the state file.
    Unknown(String),
    /// Keys the template's `.env-dist` has that the project's `.env` lacks.
    EnvKeys(Vec<String>),
    /// Entries the template's `.gitignore-dist` has that the project's
    /// `.gitignore` lacks.
    GitignoreEntries(Vec<String>),
}

impl Change {
    /// Whether this is worth showing the user. `AlreadyApplied` is bookkeeping.
    pub fn is_actionable(&self) -> bool {
        !matches!(self, Change::AlreadyApplied(_))
    }

    /// The template-relative path, for the variants that name one.
    pub fn path(&self) -> Option<&str> {
        match self {
            Change::SafeUpdate(p)
            | Change::SafeAdd(p)
            | Change::Conflict(p)
            | Change::RemovedUpstream(p)
            | Change::AlreadyApplied(p)
            | Change::Unknown(p) => Some(p),
            Change::EnvKeys(_) | Change::GitignoreEntries(_) => None,
        }
    }
}

/// Recorded template state for one project, persisted as
/// `.docker-control/state.json` at the project root. It is deliberately not
/// git-ignored, so the merge base travels with the project to other clones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateState {
    /// docker-control version that ran `init`. Absent for projects whose state
    /// was derived after the fact, where the original version is unknowable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialized_with: Option<String>,
    /// docker-control version of the most recent template sync.
    pub template_synced_at: String,
    /// Hash over the whole manifest — a one-comparison fast path.
    pub template_fingerprint: String,
    /// Hash the *template* had for each file at the last sync (not the
    /// project's). `None` means the base is unknown; see [`derive_base`].
    pub files: BTreeMap<String, Option<String>>,
}

impl TemplateState {
    pub fn path(project_dir: &Path) -> PathBuf {
        project_dir.join(STATE_DIR).join(STATE_FILE)
    }

    /// Loads the state, or `None` when it is missing or unreadable. A corrupt
    /// file is treated as absent rather than fatal: the worst case is that the
    /// base has to be re-derived, which must never block an ordinary command.
    pub fn load(project_dir: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(Self::path(project_dir)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self, project_dir: &Path) -> Result<()> {
        let path = Self::path(project_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {:?}", parent))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, format!("{}\n", json))
            .with_context(|| format!("Failed to write {:?}", path))
    }
}

/// Locates the template directory to copy from and compare against.
///
/// Every caller must agree on this: `init` stamping one template while `status`
/// and `update` compare against another would report a freshly created project
/// as already diverged. Extracts the embedded assets first, so a first run finds
/// the config-dir copy rather than an empty path.
pub fn resolve_dir() -> Result<PathBuf> {
    if let Ok(asset_manager) = crate::assets::AssetManager::new() {
        asset_manager.ensure_assets()?;
    }

    // An explicit override wins, so a user working on the template itself gets
    // it used consistently by every command.
    if let Ok(env_path) = std::env::var("DOCKER_CONTROL_TEMPLATE_DIR") {
        let path = PathBuf::from(env_path);
        if path.exists() {
            return Ok(path);
        }
    }

    if let Ok(asset_manager) = crate::assets::AssetManager::new() {
        let path = asset_manager.get_template_dir();
        if path.exists() {
            return Ok(path);
        }
    }

    // Relative to the binary, covering both an installed layout and a dev build.
    if let Ok(exe_path) = std::env::current_exe() {
        let real_exe_path = exe_path.canonicalize().unwrap_or(exe_path);
        if let Some(exe_dir) = real_exe_path.parent() {
            let mut candidates = vec![exe_dir.join("template")];
            if let Some(parent) = exe_dir.parent() {
                candidates.push(parent.join("template"));
                candidates.push(parent.join("share").join("docker-control").join("template"));
            }
            if let Some(path) = candidates.into_iter().find(|p| p.exists()) {
                return Ok(path);
            }
        }
    }

    // Running from a source checkout.
    let path = PathBuf::from("template");
    if path.exists() {
        return Ok(path);
    }

    Err(anyhow!("Could not find template directory"))
}

/// Records the template at `template_dir` as the project's new merge base.
/// Call after every operation that copies the template into a project.
/// `mark_initialized` stamps `initialized_with` and is only true for `init`;
/// on later syncs an existing value is preserved and a missing one stays
/// missing, since a legacy project's original version can't be recovered.
pub fn stamp(project_dir: &Path, template_dir: &Path, mark_initialized: bool) -> Result<()> {
    let files = manifest(template_dir)?;
    let version = env!("CARGO_PKG_VERSION").to_string();
    let previous = TemplateState::load(project_dir);

    let state = TemplateState {
        initialized_with: if mark_initialized {
            Some(version.clone())
        } else {
            previous.and_then(|p| p.initialized_with)
        },
        template_synced_at: version,
        template_fingerprint: fingerprint(&files),
        files: files.into_iter().map(|(k, v)| (k, Some(v))).collect(),
    };
    state.save(project_dir)
}

/// Hashes every file in `template_dir`, keyed by its `/`-separated path
/// relative to that directory. Runtime directories are skipped.
pub fn manifest(template_dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(template_dir).min_depth(1) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = relative_path(template_dir, entry.path())?;
        if is_runtime(&rel) {
            continue;
        }
        files.insert(rel, utils::hash_file(entry.path())?);
    }
    Ok(files)
}

/// A single hash standing in for the whole manifest, so the common "nothing
/// changed" case costs one string comparison and no project reads at all.
pub fn fingerprint(files: &BTreeMap<String, String>) -> String {
    let joined: String = files
        .iter()
        .map(|(path, hash)| format!("{}:{}\n", path, hash))
        .collect();
    utils::hash_bytes(joined.as_bytes())
}

/// Classifies every template file for `project_dir` against `template_dir`.
///
/// Never writes: `status` and the start-up notice call this, and neither should
/// leave a modified file behind in the user's repo. Projects with no recorded
/// state get an in-memory base from [`derive_base`]; persisting it is `update`'s
/// job.
pub fn diff(project_dir: &Path, template_dir: &Path) -> Result<Vec<Change>> {
    let theirs = manifest(template_dir)?;
    let state = TemplateState::load(project_dir);

    // The seed checks run unconditionally, *before* the fingerprint fast path.
    // They compare `.env`/`.gitignore` against what the seeds produced rather
    // than against a recorded base, so a stamp doesn't make them true: `update`
    // reports a missing `.env` key but never writes it, so the one change that
    // needs the user is also the one the fast path would forget the moment the
    // rest of the template was applied.
    let mut changes = derived_changes(project_dir, template_dir);

    // Fast path: the template hasn't moved since this project synced, so no
    // tracked file can need applying, whatever was edited locally.
    if let Some(state) = &state
        && state.template_fingerprint == fingerprint(&theirs)
    {
        return Ok(changes);
    }

    let base = match &state {
        Some(state) => state.files.clone(),
        None => derive_base(project_dir, &theirs)?,
    };

    let mine = project_files(project_dir, base.keys().chain(theirs.keys()))?;
    changes.splice(0..0, classify(&base, &theirs, &mine));
    Ok(changes)
}

/// Actionable changes grouped for display. Shared by `update`, `status` and the
/// start-up notice so all three describe the same state identically.
#[derive(Debug, Default, Clone)]
pub struct Summary {
    /// Applicable without asking: the template changed, the project didn't.
    pub safe: Vec<String>,
    /// Changed on both sides.
    pub conflicts: Vec<String>,
    /// Differs from the template with no recorded base to judge it against.
    pub unknown: Vec<String>,
    /// Dropped from the template; never removed automatically.
    pub removed: Vec<String>,
    /// Keys the template's `.env-dist` has that the project's `.env` lacks.
    pub env_keys: Vec<String>,
    /// Entries the template's `.gitignore-dist` has that `.gitignore` lacks.
    pub gitignore_entries: Vec<String>,
}

impl Summary {
    pub fn from_changes(changes: &[Change]) -> Self {
        let mut summary = Self::default();
        for change in changes {
            match change {
                Change::SafeUpdate(p) | Change::SafeAdd(p) => summary.safe.push(p.clone()),
                Change::Conflict(p) => summary.conflicts.push(p.clone()),
                Change::Unknown(p) => summary.unknown.push(p.clone()),
                Change::RemovedUpstream(p) => summary.removed.push(p.clone()),
                Change::EnvKeys(keys) => summary.env_keys = keys.clone(),
                Change::GitignoreEntries(entries) => summary.gitignore_entries = entries.clone(),
                Change::AlreadyApplied(_) => {}
            }
        }
        summary
    }

    pub fn is_empty(&self) -> bool {
        self.safe.is_empty()
            && self.conflicts.is_empty()
            && self.unknown.is_empty()
            && self.removed.is_empty()
            && self.env_keys.is_empty()
            && self.gitignore_entries.is_empty()
    }

    /// Prints the grouped summary. `unknown` is deliberately *not* shown by the
    /// passive notice (`notice = true`): a project predating the state file
    /// cannot clear those without running `update`, so nagging about them every
    /// time would be a permanent warning the user can't act on.
    pub fn print(&self, notice: bool) {
        let show = |label: &str, paths: &[String]| {
            if !paths.is_empty() {
                ui::warning(format!("    {}: {}", label, paths.join(", ")));
            }
        };
        show(
            &format!("{} file(s) can be updated safely", self.safe.len()),
            &self.safe,
        );
        show(
            &format!(
                "{} file(s) you modified also changed upstream",
                self.conflicts.len()
            ),
            &self.conflicts,
        );
        if !notice {
            show(
                &format!(
                    "{} file(s) differ with no recorded base (review manually)",
                    self.unknown.len()
                ),
                &self.unknown,
            );
        }
        show(
            &format!("{} file(s) no longer in the template", self.removed.len()),
            &self.removed,
        );
        if !self.env_keys.is_empty() {
            ui::warning(format!(
                "    .env is missing template keys: {}",
                self.env_keys.join(", ")
            ));
        }
        if !self.gitignore_entries.is_empty() {
            ui::warning(format!(
                "    .gitignore is missing template entries: {}",
                self.gitignore_entries.join(", ")
            ));
        }
    }
}

/// State of the project's copy of a template file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectFile {
    Missing,
    /// Present but unreadable by the host user — typically a file a container
    /// created (the condition `doctor` repairs). Deliberately distinct from
    /// [`ProjectFile::Missing`]: treating it as absent would classify it as a
    /// safe add and overwrite a file that may well hold local edits.
    Unreadable,
    Hash(String),
}

/// The three-way decision table. Pure over in-memory maps so every row is
/// unit-testable.
pub fn classify(
    base: &BTreeMap<String, Option<String>>,
    theirs: &BTreeMap<String, String>,
    mine: &BTreeMap<String, ProjectFile>,
) -> Vec<Change> {
    let paths: BTreeSet<&String> = base.keys().chain(theirs.keys()).collect();
    let mut changes = Vec::new();

    for path in paths {
        if is_runtime(path) || is_preserve_local(path) || is_consumed(path) {
            continue;
        }

        let recorded = base.get(path).cloned().flatten();
        let current = theirs.get(path);
        let mine_state = mine.get(path).cloned().unwrap_or(ProjectFile::Missing);

        let Some(theirs_hash) = current else {
            // Gone from the template. Only worth mentioning while the project
            // still has a copy — otherwise both sides agree it's gone and the
            // report would be a warning with nothing behind it.
            if mine_state != ProjectFile::Missing {
                changes.push(Change::RemovedUpstream(path.clone()));
            }
            continue;
        };

        // Can't be read, so can't be compared. Never assume it's absent.
        if mine_state == ProjectFile::Unreadable {
            changes.push(Change::Unknown(path.clone()));
            continue;
        }
        let mine_hash = match mine_state {
            ProjectFile::Hash(hash) => Some(hash),
            _ => None,
        };

        match (recorded, mine_hash) {
            // Template unchanged since the last sync: local edits are the
            // user's business and there is nothing to apply. This row is why
            // the notice can be trusted.
            (Some(b), _) if b == *theirs_hash => {}
            // The project already has the new content; only the base is stale.
            (_, Some(m)) if m == *theirs_hash => changes.push(Change::AlreadyApplied(path.clone())),
            // Absent locally: either new upstream, or locally deleted and since
            // changed. Either way re-adding it is non-destructive.
            (_, None) => changes.push(Change::SafeAdd(path.clone())),
            // Untouched locally since the last sync: safe to overwrite.
            (Some(b), Some(m)) if b == m => changes.push(Change::SafeUpdate(path.clone())),
            // Edited locally *and* changed upstream.
            (Some(_), Some(_)) => changes.push(Change::Conflict(path.clone())),
            // No base to compare against, and the project differs from the
            // template: can't tell an edit from drift, so don't guess.
            (None, Some(_)) => changes.push(Change::Unknown(path.clone())),
        }
    }

    changes
}

/// Best-effort base for a project that predates the state file. A project file
/// that still matches the current template is provably in sync, so the
/// template's hash is a real base; anything else is left `None` (→
/// [`Change::Unknown`]) rather than guessed at.
pub fn derive_base(
    project_dir: &Path,
    theirs: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Option<String>>> {
    let mine = project_files(project_dir, theirs.keys())?;
    Ok(theirs
        .iter()
        .map(|(path, hash)| {
            let matches = mine.get(path) == Some(&ProjectFile::Hash(hash.clone()));
            (path.clone(), matches.then(|| hash.clone()))
        })
        .collect())
}

/// Reads the state of the project's copy of each given template-relative path.
/// A read failure becomes [`ProjectFile::Unreadable`] rather than an error, so a
/// permission problem downgrades that one file to "can't judge" instead of
/// failing a whole `status` or `start`.
fn project_files<'a, I>(project_dir: &Path, paths: I) -> Result<BTreeMap<String, ProjectFile>>
where
    I: IntoIterator<Item = &'a String>,
{
    let mut mine = BTreeMap::new();
    for path in paths {
        if mine.contains_key(path) {
            continue;
        }
        let full = project_dir.join(path);
        let state = if !full.is_file() {
            ProjectFile::Missing
        } else {
            match utils::hash_file(&full) {
                Ok(hash) => ProjectFile::Hash(hash),
                Err(_) => ProjectFile::Unreadable,
            }
        };
        mine.insert(path.clone(), state);
    }
    Ok(mine)
}

/// Checks for the two seed files that have no project-side copy to hash. Both
/// comparisons are made against what the seed *produced* — `.env` and
/// `.gitignore` — so they need no recorded base and are therefore correct even
/// for a project that predates the state file.
///
/// Only additions are reported. A `.env-dist` change that merely alters a
/// default value says nothing about the project's own `.env`, and neither seed's
/// content is ever written over the file it seeded.
fn derived_changes(project_dir: &Path, template_dir: &Path) -> Vec<Change> {
    let mut changes = Vec::new();

    let missing_env = missing_entries(
        &read_or_empty(&template_dir.join(ENV_DIST)),
        &read_or_empty(&project_dir.join(".env")),
        env_keys,
    );
    if !missing_env.is_empty() {
        changes.push(Change::EnvKeys(missing_env));
    }

    let missing_ignores = missing_entries(
        &read_or_empty(&template_dir.join(GITIGNORE_DIST)),
        &read_or_empty(&project_dir.join(".gitignore")),
        gitignore_entries,
    );
    if !missing_ignores.is_empty() {
        changes.push(Change::GitignoreEntries(missing_ignores));
    }

    changes
}

/// Entries `template` defines that `project` doesn't, per `extract`.
fn missing_entries(template: &str, project: &str, extract: fn(&str) -> Vec<String>) -> Vec<String> {
    let existing = extract(project);
    extract(template)
        .into_iter()
        .filter(|entry| !existing.contains(entry))
        .collect()
}

fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Keys defined in a dotenv-style file, in file order, ignoring blanks and
/// comments.
fn env_keys(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, _)| key.trim().to_string())
        .collect()
}

/// Ignore patterns in a `.gitignore`, ignoring blanks and comments.
fn gitignore_entries(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// `path` relative to `root`, always `/`-separated so the manifest is
/// comparable across platforms.
fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("{:?} is not under {:?}", path, root))?;
    Ok(rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(entries: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        entries
            .iter()
            .map(|(p, h)| (p.to_string(), h.map(str::to_string)))
            .collect()
    }

    fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(p, h)| (p.to_string(), h.to_string()))
            .collect()
    }

    fn mine(entries: &[(&str, Option<&str>)]) -> BTreeMap<String, ProjectFile> {
        entries
            .iter()
            .map(|(p, h)| {
                let state = match h {
                    Some(hash) => ProjectFile::Hash(hash.to_string()),
                    None => ProjectFile::Missing,
                };
                (p.to_string(), state)
            })
            .collect()
    }

    fn unreadable(path: &str) -> BTreeMap<String, ProjectFile> {
        [(path.to_string(), ProjectFile::Unreadable)]
            .into_iter()
            .collect()
    }

    #[test]
    fn unchanged_template_is_silent_even_with_local_edits() {
        // The whole point of recording a base: the user edited compose.yml, but
        // the template hasn't moved, so there is nothing to offer them.
        let changes = classify(
            &base(&[("compose.yml", Some("a"))]),
            &map(&[("compose.yml", "a")]),
            &mine(&[("compose.yml", Some("local-edit"))]),
        );
        assert!(changes.is_empty(), "{:?}", changes);
    }

    #[test]
    fn untouched_locally_is_a_safe_update() {
        let changes = classify(
            &base(&[("compose.yml", Some("a"))]),
            &map(&[("compose.yml", "b")]),
            &mine(&[("compose.yml", Some("a"))]),
        );
        assert_eq!(changes, vec![Change::SafeUpdate("compose.yml".into())]);
    }

    #[test]
    fn edited_both_sides_is_a_conflict() {
        let changes = classify(
            &base(&[("config/php.ini", Some("a"))]),
            &map(&[("config/php.ini", "b")]),
            &mine(&[("config/php.ini", Some("local"))]),
        );
        assert_eq!(changes, vec![Change::Conflict("config/php.ini".into())]);
    }

    #[test]
    fn project_already_matching_the_new_template_is_not_actionable() {
        let changes = classify(
            &base(&[("compose.yml", Some("a"))]),
            &map(&[("compose.yml", "b")]),
            &mine(&[("compose.yml", Some("b"))]),
        );
        assert_eq!(changes, vec![Change::AlreadyApplied("compose.yml".into())]);
        assert!(!changes[0].is_actionable());
    }

    #[test]
    fn new_template_file_is_a_safe_add() {
        let changes = classify(
            &base(&[]),
            &map(&[("config/new.conf", "b")]),
            &mine(&[("config/new.conf", None)]),
        );
        assert_eq!(changes, vec![Change::SafeAdd("config/new.conf".into())]);
    }

    #[test]
    fn file_dropped_from_the_template_is_reported_not_deleted() {
        let changes = classify(
            &base(&[("config/old.conf", Some("a"))]),
            &map(&[]),
            &mine(&[("config/old.conf", Some("a"))]),
        );
        assert_eq!(
            changes,
            vec![Change::RemovedUpstream("config/old.conf".into())]
        );
    }

    #[test]
    fn file_dropped_from_both_sides_is_silent() {
        // Nothing to report once the project has deleted it too — otherwise the
        // warning would have no way to clear.
        let changes = classify(
            &base(&[("config/old.conf", Some("a"))]),
            &map(&[]),
            &mine(&[("config/old.conf", None)]),
        );
        assert!(changes.is_empty(), "{:?}", changes);
    }

    #[test]
    fn unreadable_project_file_is_never_treated_as_absent() {
        // A container-owned file the host user can't read must not be classified
        // as a safe add — that would overwrite it without a prompt. It is
        // unjudgeable, which is exactly what `Unknown` means.
        let changes = classify(
            &base(&[("config/php.ini", Some("a"))]),
            &map(&[("config/php.ini", "b")]),
            &unreadable("config/php.ini"),
        );
        assert_eq!(changes, vec![Change::Unknown("config/php.ini".into())]);
    }

    #[test]
    fn missing_base_with_a_differing_file_is_unknown() {
        let changes = classify(
            &base(&[("compose.yml", None)]),
            &map(&[("compose.yml", "b")]),
            &mine(&[("compose.yml", Some("something-else"))]),
        );
        assert_eq!(changes, vec![Change::Unknown("compose.yml".into())]);
    }

    #[test]
    fn per_project_files_are_never_reported() {
        // secrets/ and htpasswd hold per-project values; the template only ever
        // seeds them, so a divergence is expected rather than a conflict.
        let changes = classify(
            &base(&[
                ("secrets/db_pw.txt", Some("a")),
                ("config/htpasswd", Some("a")),
            ]),
            &map(&[("secrets/db_pw.txt", "b"), ("config/htpasswd", "b")]),
            &mine(&[
                ("secrets/db_pw.txt", Some("my-password")),
                ("config/htpasswd", Some("my-users")),
            ]),
        );
        assert!(changes.is_empty(), "{:?}", changes);
    }

    #[test]
    fn runtime_dirs_are_ignored() {
        let changes = classify(
            &base(&[("logs/apache/.gitkeep", Some("a"))]),
            &map(&[("logs/apache/.gitkeep", "b")]),
            &mine(&[("logs/apache/.gitkeep", None)]),
        );
        assert!(changes.is_empty(), "{:?}", changes);
    }

    #[test]
    fn consumed_seed_is_excluded_from_hash_classification() {
        // `.gitignore-dist` is renamed/merged away on apply, so a missing project
        // copy is normal and must never read as "deleted locally" — nor, when the
        // base is unknown, as a pending update. It is checked by content in
        // `derived_changes` instead.
        let changes = classify(
            &base(&[(".gitignore-dist", None)]),
            &map(&[(".gitignore-dist", "b")]),
            &mine(&[(".gitignore-dist", None)]),
        );
        assert!(changes.is_empty(), "{:?}", changes);
    }

    #[test]
    fn env_dist_is_tracked_like_any_other_file() {
        // `init` leaves `.env-dist` in the project as the reference list of keys
        // (it writes `.env` separately), so the project has a copy and ordinary
        // comparison keeps that reference current.
        let changes = classify(
            &base(&[(".env-dist", Some("a"))]),
            &map(&[(".env-dist", "b")]),
            &mine(&[(".env-dist", Some("a"))]),
        );
        assert_eq!(changes, vec![Change::SafeUpdate(".env-dist".into())]);
    }

    #[test]
    fn missing_entries_reports_only_additions() {
        // A changed default value is not an addition and must not be reported.
        assert!(
            missing_entries(
                "BASE_DOMAIN=new.lvh.me\n",
                "BASE_DOMAIN=old.lvh.me\n",
                env_keys
            )
            .is_empty()
        );
        assert_eq!(
            missing_entries(
                "BASE_DOMAIN=x\nSELF_SIGNED_HOST=1\n",
                "BASE_DOMAIN=x\n",
                env_keys
            ),
            vec!["SELF_SIGNED_HOST".to_string()]
        );
        // The merge sorts and dedups `.gitignore`, so order must not matter.
        assert!(
            missing_entries(
                "htdocs\nlogs\n",
                "logs\nhtdocs\nvendor\n",
                gitignore_entries
            )
            .is_empty()
        );
        assert_eq!(
            missing_entries("htdocs\nreleases\n", "htdocs\n", gitignore_entries),
            vec!["releases".to_string()]
        );
    }

    #[test]
    fn env_keys_ignores_comments_and_blanks() {
        let content = "# comment\n\nBASE_DOMAIN=x.lvh.me\n  PHP_VERSION=8.2\nnot-an-assignment\n";
        assert_eq!(
            env_keys(content),
            vec!["BASE_DOMAIN".to_string(), "PHP_VERSION".to_string()]
        );
    }

    #[test]
    fn fingerprint_is_order_independent_but_content_sensitive() {
        let a = map(&[("compose.yml", "1"), ("config/php.ini", "2")]);
        let b = map(&[("config/php.ini", "2"), ("compose.yml", "1")]);
        assert_eq!(fingerprint(&a), fingerprint(&b));

        let c = map(&[("compose.yml", "1"), ("config/php.ini", "3")]);
        assert_ne!(fingerprint(&a), fingerprint(&c));
    }

    #[test]
    fn fingerprint_distinguishes_renames() {
        // A path change with identical content must still register.
        let a = map(&[("config/php.ini", "1")]);
        let b = map(&[("config/php.new.ini", "1")]);
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }
}
