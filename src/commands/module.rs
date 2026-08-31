//! Move a vendor module out of `vendor/` so it can be developed in place.
//!
//! Vendor modules are installed from source, so `htdocs/vendor/<vendor>/<name>/`
//! is a real git clone — but anything edited there is discarded by the next
//! `composer install`. This command relocates the clone to
//! `htdocs/modules/<vendor>/<name>/` and wires a Composer `path` repository that
//! symlinks it back into `vendor/`, so edits survive and the module's own git
//! history stays usable (including via `docker-control release` / `merge`, which
//! reach it through the symlink).
//!
//! Three details are load-bearing and were established empirically:
//!
//! * The `path` entry must be **prepended** to `repositories`. Appended, it loses
//!   priority to a private `composer` repository declared in the same file and
//!   Composer exits *successfully* having re-cloned from upstream, orphaning the
//!   development checkout. [`link`] therefore asserts the symlink afterwards
//!   instead of trusting the exit status.
//! * `composer config` — not Rust — writes `composer.json`. It prepends, and it
//!   preserves the file's existing indentation and key order;
//!   `serde_json::to_string_pretty` would reflow the whole file. JSON is only ever
//!   *read* here.
//! * [`unlink`] removes the `vendor/` symlink *before* invoking Composer. Left in
//!   place, Composer follows it into a dirty worktree and aborts with
//!   "Source directory ... has uncommitted changes" — having already rewritten
//!   `composer.lock`.

use crate::docker;
use crate::git::GitService;
use crate::ui;
use crate::utils;
use anyhow::{Context, Result, anyhow};
use inquire::{Confirm, Select};
use serde_json::Value;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;

/// Development checkouts live here, relative to `htdocs/`. This is inside
/// `htdocs/` because `./htdocs:/var/www/html` is the only application mount, so
/// anywhere else is invisible to the in-container Composer.
const MODULES_DIR: &str = "modules";
/// `htdocs/` as seen from inside the `php` container.
const CONTAINER_HTDOCS: &str = "/var/www/html";
/// Must be passed explicitly — see [`docker::exec_as_user`].
const COMPOSER_HOME: &str = "/var/www/.composer";
const COMPOSER_SERVICE: &str = "php";
const COMPOSER_USER: &str = "www-data";
const GITIGNORE_ENTRY: &str = "/modules/";
/// Written immediately above [`GITIGNORE_ENTRY`] so `unlink` can tell an entry it
/// added from one the project already had, and remove only its own.
const GITIGNORE_MARKER: &str = "# docker-control: development module checkouts";
/// What `create` pins in the path repository. A new module has released nothing, so the
/// entry advertises the development branch instead of inventing a version; `branch_from_pin`
/// maps this to [`CREATE_BRANCH`], which is the branch `create` initialises.
const CREATE_PIN: &str = "dev-main";
const CREATE_BRANCH: &str = "main";
/// Keeps the otherwise-empty `src/` in the initial commit — git tracks files, not directories.
const SRC_KEEPFILE: &str = "src/.gitkeep";

#[derive(clap::Subcommand, Debug, Clone)]
pub enum ModuleAction {
    /// Move a vendor module to htdocs/modules/ and symlink it back for development
    Link {
        /// Module to link as <vendor>/<name> (prompted for when omitted)
        module: Option<String>,

        /// Version to pin in the path repository (default: the version in composer.lock)
        #[arg(long)]
        version: Option<String>,

        /// Extra arguments forwarded to `composer update` (e.g. -W, --no-scripts)
        #[arg(trailing_var_arg = true)]
        composer_args: Vec<String>,
    },
    /// Restore a linked module to a normal Composer install under vendor/
    Unlink {
        /// Module to unlink as <vendor>/<name> (prompted for when omitted)
        module: Option<String>,

        /// Also delete the development checkout from htdocs/modules/
        #[arg(long)]
        purge: bool,

        /// Skip the confirmation prompt when purging
        #[arg(short, long)]
        yes: bool,
    },
    /// Scaffold a new module in htdocs/modules/ and link it into vendor/ for development
    Create {
        /// New module as <vendor>/<name>
        module: String,

        /// Package type forwarded to `composer init` (default: library)
        #[arg(long)]
        r#type: Option<String>,

        /// Skip composer init's questions
        #[arg(short, long)]
        yes: bool,

        /// Extra arguments forwarded to `composer require`
        #[arg(trailing_var_arg = true)]
        composer_args: Vec<String>,
    },
    /// List vendor modules and show which are linked for development
    List,
}

pub trait ModulePromptProvider {
    fn select_module_to_link(&self, modules: Vec<String>) -> Result<String>;
    fn select_module_to_unlink(&self, modules: Vec<String>) -> Result<String>;
    fn confirm_purge(&self, module: &str) -> Result<bool>;
}

pub struct InteractiveModulePromptProvider;

impl ModulePromptProvider for InteractiveModulePromptProvider {
    fn select_module_to_link(&self, modules: Vec<String>) -> Result<String> {
        Ok(Select::new("Select vendor module to link for development", modules).prompt()?)
    }

    fn select_module_to_unlink(&self, modules: Vec<String>) -> Result<String> {
        Ok(Select::new("Select module to unlink", modules).prompt()?)
    }

    fn confirm_purge(&self, module: &str) -> Result<bool> {
        Ok(Confirm::new(&format!(
            "Delete the development checkout of {} anyway?",
            module
        ))
        .with_default(false)
        .prompt()?)
    }
}

pub struct ModuleOptions {
    pub prompt_provider: Box<dyn ModulePromptProvider>,
    /// Skip every in-container Composer call. Used by tests, which assert on the
    /// filesystem side effects rather than on Composer's behaviour.
    pub skip_composer: bool,
}

impl Default for ModuleOptions {
    fn default() -> Self {
        Self {
            prompt_provider: Box::new(InteractiveModulePromptProvider),
            skip_composer: false,
        }
    }
}

/// A `path` repository entry in `htdocs/composer.json` pointing into `modules/`.
#[derive(Debug, Clone)]
pub struct LinkedModule {
    /// The key `composer config --unset repositories.<key>` needs.
    ///
    /// For the object form of `repositories` that is the object key. For the array
    /// form it is the entry's `name` property — Composer adds one when it writes an
    /// entry, and addresses array entries by it. It is emphatically *not* the array
    /// index: `--unset repositories.0` silently does nothing and still exits 0.
    ///
    /// `None` for a hand-written array entry with no `name`, which cannot be
    /// removed by `composer config` at all.
    pub key: Option<String>,
    /// Composer package name, e.g. `acme/widget`.
    pub package: String,
    /// Path relative to `htdocs/`, e.g. `modules/acme/widget`.
    pub url: String,
    /// The pinned version from `options.versions`, when present.
    pub version: Option<String>,
}

impl LinkedModule {
    /// The `<vendor>/<name>` directory path under `modules/` and `vendor/`.
    pub fn relative_path(&self) -> &str {
        self.url.strip_prefix("modules/").unwrap_or(&self.url)
    }
}

pub fn execute(project_dir: &Path, action: ModuleAction, options: ModuleOptions) -> Result<()> {
    let htdocs = project_dir.join("htdocs");
    if !htdocs.join("composer.json").exists() {
        return Err(anyhow!(
            "No composer.json found in {:?} — this command needs an application in htdocs/",
            htdocs
        ));
    }

    match action {
        ModuleAction::List => list(project_dir, &htdocs),
        ModuleAction::Link {
            module,
            version,
            composer_args,
        } => link(
            project_dir,
            &htdocs,
            module,
            version,
            &composer_args,
            &options,
        ),
        ModuleAction::Create {
            module,
            r#type,
            yes,
            composer_args,
        } => create(
            project_dir,
            &htdocs,
            module,
            r#type,
            yes,
            &composer_args,
            &options,
        ),
        ModuleAction::Unlink { module, purge, yes } => {
            unlink(project_dir, &htdocs, module, purge, yes, &options)
        }
    }
}

// ---------------------------------------------------------------------------
// link
// ---------------------------------------------------------------------------

/// What `link` has changed so far, so a failure can be undone.
#[derive(Default)]
struct LinkState {
    moved: bool,
    gitignore_added: bool,
    /// The commit the checkout's HEAD was detached at before
    /// [`checkout_pinned_branch`] put it on a branch, so a rollback can put it back.
    /// `None` when HEAD was never moved.
    detached_head: Option<String>,
}

fn link(
    project_dir: &Path,
    htdocs: &Path,
    module: Option<String>,
    version: Option<String>,
    composer_args: &[String],
    options: &ModuleOptions,
) -> Result<()> {
    require_running_stack(project_dir, options)?;

    let linked = linked_modules(htdocs)?;
    let relative = resolve_link_target(project_dir, &linked, module, options)?;

    if let Some(existing) = linked.iter().find(|l| l.relative_path() == relative) {
        ui::info(format!(
            "{} is already linked for development (pinned {}).",
            existing.package,
            existing.version.as_deref().unwrap_or("unpinned")
        ));
        return Ok(());
    }

    let vendor_path = htdocs.join("vendor").join(&relative);
    let dev_path = htdocs.join(MODULES_DIR).join(&relative);

    // The checkout may already exist from an earlier `unlink` without --purge.
    let reuse_existing = dev_path.exists();
    if reuse_existing {
        if vendor_path.exists() && !is_symlink(&vendor_path) {
            return Err(anyhow!(
                "Both {:?} and {:?} exist as real directories. Remove whichever copy is stale before linking.",
                vendor_path,
                dev_path
            ));
        }
    } else if !vendor_path.join(".git").exists() {
        return Err(anyhow!(
            "{:?} is not a git repository — only source-installed modules can be linked",
            vendor_path
        ));
    }

    let source = if reuse_existing {
        &dev_path
    } else {
        &vendor_path
    };
    let package = package_name(source, &relative);

    let pin = match version {
        Some(v) => v,
        None => installed_version(htdocs, &package)?.ok_or_else(|| {
            anyhow!(
                "Could not determine the installed version of {} from composer.lock — pass --version <version> to pin it explicitly",
                package
            )
        })?,
    };

    ui::info(format!("Linking {} (pinned {})", package, pin));

    let snapshot = ComposerSnapshot::capture(htdocs)?;
    let mut state = LinkState::default();

    let result = link_inner(
        project_dir,
        htdocs,
        &relative,
        &package,
        &pin,
        &vendor_path,
        &dev_path,
        reuse_existing,
        composer_args,
        options,
        &mut state,
    );

    if let Err(e) = result {
        ui::warning("Rolling back changes...");
        rollback_link(htdocs, &snapshot, &vendor_path, &dev_path, &state);
        return Err(e);
    }

    let idea_path = format!("htdocs/{}/{}", MODULES_DIR, relative);
    utils::register_phpstorm_git_root(project_dir, &idea_path)?;

    ui::success(format!("{} is linked for development.", package));
    ui::info(format!("  edit here: htdocs/{}/{}", MODULES_DIR, relative));
    ui::warning(
        "composer.json and composer.lock are tracked — do not commit this link. `deploy` and \
         `release` build from the committed tree, where modules/ does not exist.",
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn link_inner(
    project_dir: &Path,
    htdocs: &Path,
    relative: &str,
    package: &str,
    pin: &str,
    vendor_path: &Path,
    dev_path: &Path,
    reuse_existing: bool,
    composer_args: &[String],
    options: &ModuleOptions,
    state: &mut LinkState,
) -> Result<()> {
    if !reuse_existing {
        if let Some(parent) = dev_path.parent() {
            fs::create_dir_all(parent)
                .context(format!("Failed to create modules directory {:?}", parent))?;
        }
        fs::rename(vendor_path, dev_path).context(format!(
            "Failed to move {:?} to {:?}",
            vendor_path, dev_path
        ))?;
        state.moved = true;
    }

    state.detached_head = checkout_pinned_branch(dev_path, pin);

    state.gitignore_added = ensure_modules_gitignored(htdocs)?;
    if state.gitignore_added {
        ui::info("  added /modules/ to htdocs/.gitignore");
    }

    // Written by Composer, not by us: `composer config` prepends the entry (which
    // is what gives it repository priority) and leaves the rest of the file's
    // formatting untouched.
    composer(
        project_dir,
        &[
            "config",
            &repo_key(relative),
            &repo_entry_json(relative, package, pin)?,
        ],
        options,
    )?;

    let mut update_args = vec!["update".to_string(), package.to_string()];
    update_args.extend(composer_args.iter().cloned());
    let update_args: Vec<&str> = update_args.iter().map(String::as_str).collect();
    composer(project_dir, &update_args, options)?;

    if options.skip_composer {
        return Ok(());
    }

    // Composer can exit 0 without linking when a higher-priority repository also
    // offers the package, so verify rather than trust the exit status.
    if !links_to(vendor_path, dev_path) {
        return Err(anyhow!(
            "Composer reported success but {:?} is not a symlink to {:?}.\n\
             Another repository in htdocs/composer.json is taking priority over the path \
             repository. Check that the `{}` entry is first in `repositories`.",
            vendor_path,
            dev_path,
            repo_key(relative)
        ));
    }

    Ok(())
}

fn rollback_link(
    htdocs: &Path,
    snapshot: &ComposerSnapshot,
    vendor_path: &Path,
    dev_path: &Path,
    state: &LinkState,
) {
    if let Err(e) = snapshot.restore(htdocs) {
        ui::critical(format!(
            "Failed to restore composer.json/composer.lock: {}",
            e
        ));
    }

    // Before the move, so `dev_path` still points at the checkout.
    if let Some(commit) = &state.detached_head {
        restore_detached_head(dev_path, commit);
    }

    if state.moved {
        // Composer may have left a symlink where the real directory belongs.
        if is_symlink(vendor_path) {
            let _ = fs::remove_file(vendor_path);
        }
        if let Err(e) = fs::rename(dev_path, vendor_path) {
            ui::critical(format!(
                "Failed to move {:?} back to {:?}: {}",
                dev_path, vendor_path, e
            ));
        } else {
            prune_empty_module_dirs(htdocs, dev_path);
        }
    }

    if state.gitignore_added
        && let Err(e) = remove_modules_gitignore(htdocs)
    {
        ui::warning(format!("Failed to revert htdocs/.gitignore: {}", e));
    }
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

/// What `create` has changed so far. Deliberately does **not** track the checkout for
/// deletion — see [`rollback_create`].
#[derive(Default)]
struct CreateState {
    gitignore_added: bool,
}

fn create(
    project_dir: &Path,
    htdocs: &Path,
    module: String,
    package_type: Option<String>,
    yes: bool,
    composer_args: &[String],
    options: &ModuleOptions,
) -> Result<()> {
    // Validated as typed, not lowercased first: silently turning `Acme/Widget` into
    // `acme/widget` would create a module under a name the developer did not ask for.
    let relative = module.trim_matches('/').to_string();
    validate_package_name(&relative)?;

    let dev_path = htdocs.join(MODULES_DIR).join(&relative);
    let vendor_path = htdocs.join("vendor").join(&relative);

    // Every "it already exists" case is really a request for `link`, so say so rather than
    // failing halfway through with a Composer error.
    if let Some(existing) = linked_modules(htdocs)?
        .iter()
        .find(|l| l.relative_path() == relative)
    {
        return Err(anyhow!(
            "{} is already linked for development.",
            existing.package
        ));
    }
    if dev_path.exists() {
        return Err(anyhow!(
            "{:?} already exists — link the existing checkout with `docker-control module link {}`",
            dev_path,
            relative
        ));
    }
    if vendor_path.exists() {
        return Err(anyhow!(
            "{:?} already exists, so {} is an installed package rather than a new one — use \
             `docker-control module link {}`",
            vendor_path,
            relative,
            relative
        ));
    }

    require_running_stack(project_dir, options)?;

    ui::info(format!("Creating {} (pinned {})", relative, CREATE_PIN));

    let snapshot = ComposerSnapshot::capture(htdocs)?;
    let mut state = CreateState::default();

    let result = create_inner(
        project_dir,
        htdocs,
        &relative,
        package_type.as_deref(),
        yes,
        composer_args,
        &dev_path,
        &vendor_path,
        options,
        &mut state,
    );

    let package = match result {
        Ok(package) => package,
        Err(e) => {
            ui::warning("Rolling back the application-side changes...");
            rollback_create(htdocs, &snapshot, &dev_path, &state);
            return Err(e);
        }
    };

    let idea_path = format!("htdocs/{}/{}", MODULES_DIR, relative);
    utils::register_phpstorm_git_root(project_dir, &idea_path)?;

    ui::success(format!(
        "{} is created and linked for development.",
        package
    ));
    ui::info(format!("  edit here: htdocs/{}/{}", MODULES_DIR, relative));
    ui::warning(
        "Do not commit the path repository in composer.json — `deploy` and `release` build \
         from the committed tree, where modules/ does not exist.",
    );
    ui::warning(format!(
        "The `require` on {} *does* belong in a commit, but only once the module is pushed \
         somewhere the application's other repositories can reach. Until then the application \
         only installs on this machine.",
        package
    ));

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_inner(
    project_dir: &Path,
    htdocs: &Path,
    relative: &str,
    package_type: Option<&str>,
    yes: bool,
    composer_args: &[String],
    dev_path: &Path,
    vendor_path: &Path,
    options: &ModuleOptions,
    state: &mut CreateState,
) -> Result<String> {
    fs::create_dir_all(dev_path.join("src"))
        .context(format!("Failed to create {:?}", dev_path.join("src")))?;
    fs::write(dev_path.join(SRC_KEEPFILE), "").context(format!(
        "Failed to create {:?}",
        dev_path.join(SRC_KEEPFILE)
    ))?;
    ui::info(format!("  created htdocs/{}/{}", MODULES_DIR, relative));

    // Before Composer runs, so the checkout is a repository from its first commit onwards and
    // the pin's branch actually exists.
    let git = GitService::init(dev_path, CREATE_BRANCH)?;

    let container_dir = container_module_dir(relative);
    let mut init_args = vec![
        "init".to_string(),
        format!("--name={}", relative),
        format!("--type={}", package_type.unwrap_or("library")),
    ];
    // Composer's questions need a terminal to answer them on.
    if yes || !std::io::stdin().is_terminal() {
        init_args.push("--no-interaction".to_string());
    }
    composer_init(project_dir, &container_dir, &as_strs(&init_args), options)?;

    // `composer init` offers the name as a default rather than fixing it, so read back what
    // actually landed in the file: the repository entry and the `require` must both name the
    // package the module calls itself.
    let package = package_name(dev_path, relative);

    ensure_psr4_autoload(dev_path, &package)?;
    commit_scaffold(&git, dev_path, &package);

    state.gitignore_added = ensure_modules_gitignored(htdocs)?;
    if state.gitignore_added {
        ui::info("  added /modules/ to htdocs/.gitignore");
    }

    // Same two Composer calls `link` makes, plus the `require` that `link` never needs: the
    // package it links is required already, whereas nothing yet requires a brand-new module,
    // so without this `vendor/<vendor>/<name>` would never appear.
    composer(
        project_dir,
        &[
            "config",
            &repo_key(relative),
            &repo_entry_json(relative, &package, CREATE_PIN)?,
        ],
        options,
    )?;

    let mut require_args = vec!["require".to_string(), format!("{}:{}", package, CREATE_PIN)];
    require_args.extend(composer_args.iter().cloned());
    composer(project_dir, &as_strs(&require_args), options)?;

    if options.skip_composer {
        return Ok(package);
    }

    // Same reason as `link_inner`: Composer can exit 0 without having created the symlink.
    if !links_to(vendor_path, dev_path) {
        return Err(anyhow!(
            "Composer reported success but {:?} is not a symlink to {:?}.\n\
             Another repository in htdocs/composer.json is taking priority over the path \
             repository. Check that the `{}` entry is first in `repositories`.",
            vendor_path,
            dev_path,
            repo_key(relative)
        ));
    }

    Ok(package)
}

/// Undoes only what `create` changed in the **application**, and keeps the module checkout.
///
/// The opposite of [`rollback_link`], on purpose: the files under `htdocs/modules/` are new
/// work — possibly answers typed into `composer init`'s prompts — not a checkout Composer can
/// reproduce. Deleting them to tidy up a failed `composer require` would destroy the only copy.
fn rollback_create(
    htdocs: &Path,
    snapshot: &ComposerSnapshot,
    dev_path: &Path,
    state: &CreateState,
) {
    if let Err(e) = snapshot.restore(htdocs) {
        ui::critical(format!(
            "Failed to restore composer.json/composer.lock: {}",
            e
        ));
    }

    if state.gitignore_added
        && let Err(e) = remove_modules_gitignore(htdocs)
    {
        ui::warning(format!("Failed to revert htdocs/.gitignore: {}", e));
    }

    if dev_path.exists() {
        ui::info(format!(
            "  the new module is kept at {:?} — rerun `module create` after fixing the above, \
             or delete it by hand",
            dev_path
        ));
    }
}

/// Adds a PSR-4 autoload mapping for `src/` when the module has none.
///
/// Interactive `composer init` offers to write one, so this is usually a no-op; it fills the
/// gap when the developer declines or `--no-interaction` skips the question. Unlike the
/// application's `composer.json`, this file was generated seconds ago by us and is not
/// hand-maintained, so writing it with `serde_json` reflows nothing anyone cares about — and
/// it avoids depending on whether `composer config` can address `autoload.*` at all.
fn ensure_psr4_autoload(dev_path: &Path, package: &str) -> Result<()> {
    let path = dev_path.join("composer.json");
    let Ok(raw) = fs::read_to_string(&path) else {
        return Ok(()); // no composer.json (e.g. `composer init` was skipped) — nothing to amend
    };
    let mut value: Value =
        serde_json::from_str(&raw).context(format!("Failed to parse {:?}", path))?;

    if value
        .get("autoload")
        .and_then(|a| a.get("psr-4"))
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty())
    {
        return Ok(());
    }

    let namespace = psr4_namespace(package);
    value
        .as_object_mut()
        .ok_or_else(|| anyhow!("{:?} is not a JSON object", path))?
        .entry("autoload")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("\"autoload\" in {:?} is not a JSON object", path))?
        .insert(
            "psr-4".to_string(),
            serde_json::json!({ namespace.clone(): "src/" }),
        );

    let mut out = serde_json::to_string_pretty(&value)?;
    out.push('\n');
    fs::write(&path, out).context(format!("Failed to write {:?}", path))?;
    ui::info(format!("  autoloading {} from src/", namespace));
    Ok(())
}

/// `acme/my-widget` -> `Acme\MyWidget\`. `-`, `_` and `.` are word breaks; the trailing
/// separator is what PSR-4 requires of a namespace prefix.
fn psr4_namespace(package: &str) -> String {
    let studly = |segment: &str| -> String {
        segment
            .split(['-', '_', '.'])
            .filter(|w| !w.is_empty())
            .map(|w| {
                let mut chars = w.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect()
    };

    let mut namespace: String = package
        .split('/')
        .map(studly)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\\");
    namespace.push('\\');
    namespace
}

/// Rejects a name `composer init` would reject anyway, before anything is created.
///
/// Mirrors Composer's own `<vendor>/<name>` rule: each segment starts and ends with
/// `[a-z0-9]`, and `.`/`_`/`-` may separate runs (`--` is allowed, which is why the check
/// counts separator runs rather than forbidding repeats outright).
fn validate_package_name(package: &str) -> Result<()> {
    let invalid = |reason: &str| {
        Err(anyhow!(
            "{:?} is not a valid Composer package name ({}). Expected <vendor>/<name>, \
             lowercase, e.g. acme/widget",
            package,
            reason
        ))
    };

    let mut segments = package.split('/');
    let (Some(vendor), Some(name), None) = (segments.next(), segments.next(), segments.next())
    else {
        return invalid("it must have exactly one '/'");
    };

    for segment in [vendor, name] {
        if segment.is_empty() {
            return invalid("neither side of the '/' may be empty");
        }
        if segment.chars().any(|c| c.is_ascii_uppercase()) {
            return invalid("Composer package names are lowercase");
        }
        if !segment
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return invalid("only a-z, 0-9, '.', '_' and '-' are allowed");
        }
        let is_sep = |c: char| matches!(c, '.' | '_' | '-');
        if segment.starts_with(is_sep) || segment.ends_with(is_sep) {
            return invalid("a segment may not start or end with '.', '_' or '-'");
        }
        if segment
            .as_bytes()
            .windows(3)
            .any(|w| w.iter().all(|&c| is_sep(c as char)))
        {
            return invalid("at most two separators may appear in a row");
        }
    }

    Ok(())
}

/// `htdocs/modules/<vendor>/<name>` as the container sees it.
fn container_module_dir(relative: &str) -> String {
    format!("{}/{}/{}", CONTAINER_HTDOCS, MODULES_DIR, relative)
}

/// Commits the scaffold so the module starts on a real [`CREATE_BRANCH`] rather than an
/// unborn HEAD, which `release`/`merge` and the `unlink` guard would both read as odd.
/// Advisory: a failure here leaves a perfectly usable checkout, just uncommitted.
fn commit_scaffold(git: &GitService, dev_path: &Path, package: &str) {
    for relative_file in ["composer.json", SRC_KEEPFILE] {
        if dev_path.join(relative_file).exists()
            && let Err(e) = git.add_file(Path::new(relative_file))
        {
            ui::debug(format!("could not stage {}: {}", relative_file, e));
        }
    }

    match git.commit_with_identity_fallback(
        &format!("Initial commit for {}", package),
        "docker-control",
        "docker-control@localhost",
    ) {
        Ok(false) => ui::info(format!("  initial commit on {}", CREATE_BRANCH)),
        Ok(true) => ui::warning(format!(
            "  initial commit on {} was authored as docker-control — set user.name and \
             user.email in your git config, then `git commit --amend --reset-author`",
            CREATE_BRANCH
        )),
        Err(e) => ui::warning(format!(
            "  could not create the initial commit ({}) — the checkout is a git repository \
             on {}, commit it by hand",
            e, CREATE_BRANCH
        )),
    }
}

fn as_strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

// ---------------------------------------------------------------------------
// unlink
// ---------------------------------------------------------------------------

fn unlink(
    project_dir: &Path,
    htdocs: &Path,
    module: Option<String>,
    purge: bool,
    yes: bool,
    options: &ModuleOptions,
) -> Result<()> {
    require_running_stack(project_dir, options)?;

    let linked = linked_modules(htdocs)?;
    if linked.is_empty() {
        return Err(anyhow!("No modules are currently linked for development."));
    }

    let target = resolve_unlink_target(&linked, module, options)?;
    let relative = target.relative_path().to_string();
    let vendor_path = htdocs.join("vendor").join(&relative);
    let dev_path = htdocs.join(MODULES_DIR).join(&relative);

    // Bail before touching anything: `composer config --unset` cannot address an
    // array entry that has no `name`, and it reports success while doing nothing.
    if target.key.is_none() {
        return Err(anyhow!(
            "The path repository for {} in htdocs/composer.json has no \"name\" property, so \
             `composer config --unset` cannot remove it. Delete the entry pointing at {} by \
             hand, then run `composer update {}`.",
            target.package,
            target.url,
            target.package
        ));
    }

    // Also before touching anything, and for the same reason as the bail above: `unlink` ends
    // in `composer update <pkg>`, which has to find the package somewhere other than the path
    // repository it just removed. A module `create` made and nobody has pushed exists only in
    // htdocs/modules/, so that update fails — after the lock has been rewritten.
    if dev_path.exists() && !checkout_has_remote(&dev_path) {
        return Err(anyhow!(
            "{} has no git remote, so it exists only in htdocs/{}/{}. Removing the path              repository would leave the application requiring a package nothing can supply.\n\
             Push the module to a repository the application can reach and run `unlink` again,              or — if it was a mistake — drop it from the application first with              `docker-control console -- composer remove {}`.",
            target.package,
            MODULES_DIR,
            relative,
            target.package
        ));
    }

    ui::info(format!("Unlinking {}", target.package));

    let snapshot = ComposerSnapshot::capture(htdocs)?;

    // Before Composer runs: it would otherwise follow the symlink into the
    // development checkout and refuse to remove a worktree with local changes.
    if is_symlink(&vendor_path) {
        fs::remove_file(&vendor_path)
            .context(format!("Failed to remove symlink {:?}", vendor_path))?;
    }

    let result = unlink_inner(project_dir, &target, &vendor_path, &dev_path, options);

    if let Err(e) = result {
        ui::warning("Rolling back changes...");
        if let Err(e) = snapshot.restore(htdocs) {
            ui::critical(format!(
                "Failed to restore composer.json/composer.lock: {}",
                e
            ));
        }
        if !vendor_path.exists()
            && let Err(e) = symlink_dev_checkout(&relative, &vendor_path)
        {
            ui::critical(format!(
                "Failed to restore symlink {:?}: {}",
                vendor_path, e
            ));
        }
        return Err(e);
    }

    let idea_path = format!("htdocs/{}/{}", MODULES_DIR, relative);
    utils::unregister_phpstorm_git_root(project_dir, &idea_path)?;

    if purge {
        purge_checkout(htdocs, &dev_path, &target.package, yes, options)?;
    }

    ui::success(format!(
        "{} is back to a normal vendor install.",
        target.package
    ));

    if dev_path.exists() {
        ui::info(format!(
            "  development checkout kept at htdocs/{}/{} (remove it with --purge)",
            MODULES_DIR, relative
        ));
    }

    // Once nothing is linked and no checkout is left, take our own .gitignore entry
    // back out so a link/unlink round trip leaves htdocs/ as it was found.
    tidy_modules_gitignore(htdocs);

    ui::info("  restart the php container if the application serves stale paths");

    Ok(())
}

fn unlink_inner(
    project_dir: &Path,
    target: &LinkedModule,
    vendor_path: &Path,
    dev_path: &Path,
    options: &ModuleOptions,
) -> Result<()> {
    let key = target
        .key
        .as_deref()
        .ok_or_else(|| anyhow!("no addressable repositories key for {}", target.package))?;

    composer(
        project_dir,
        &["config", "--unset", &format!("repositories.{}", key)],
        options,
    )?;

    // --prefer-source restores a real git clone, which is how these modules are
    // installed and what `GitService::list_vendor_modules` requires to find them.
    // Note it applies to the whole run, not just this package.
    composer(
        project_dir,
        &["update", &target.package, "--prefer-source"],
        options,
    )?;

    if options.skip_composer {
        return Ok(());
    }

    // Symmetric to the check in `link_inner`, and for the same reason: Composer
    // can report success without having done the thing. If the path repository
    // survived the `--unset`, `update` happily re-symlinks and exits 0.
    if links_to(vendor_path, dev_path) {
        return Err(anyhow!(
            "Composer reported success but {:?} is still a symlink to the development checkout.\n\
             The path repository was not removed from htdocs/composer.json — remove the entry \
             pointing at {} by hand, then run `composer update {}`.",
            vendor_path,
            target.url,
            target.package
        ));
    }

    Ok(())
}

fn purge_checkout(
    htdocs: &Path,
    dev_path: &Path,
    package: &str,
    yes: bool,
    options: &ModuleOptions,
) -> Result<()> {
    if !dev_path.exists() {
        return Ok(());
    }

    if let Ok(git) = GitService::open(dev_path) {
        let dirty = git.is_dirty().unwrap_or(false);
        let unpushed = git.unpushed_commits().unwrap_or(0);

        if dirty || unpushed > 0 {
            if dirty {
                ui::warning(format!("{} has uncommitted changes.", package));
            }
            if unpushed > 0 {
                ui::warning(format!(
                    "{} has {} commit(s) that exist on no remote.",
                    package, unpushed
                ));
            }
            if !yes && !options.prompt_provider.confirm_purge(package)? {
                ui::info(format!("  keeping {:?}", dev_path));
                return Ok(());
            }
        }
    }

    fs::remove_dir_all(dev_path).context(format!("Failed to remove {:?}", dev_path))?;
    prune_empty_module_dirs(htdocs, dev_path);
    ui::info(format!("  removed {:?}", dev_path));
    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(project_dir: &Path, htdocs: &Path) -> Result<()> {
    let linked = linked_modules(htdocs)?;
    // `list_vendor_modules` follows symlinks, so this includes linked modules.
    let mut names = GitService::list_vendor_modules(project_dir)?;
    for l in &linked {
        let relative = l.relative_path().to_string();
        if !names.contains(&relative) {
            names.push(relative);
        }
    }
    names.sort();
    names.dedup();

    if names.is_empty() {
        ui::info("No vendor modules found in htdocs/vendor.");
        return Ok(());
    }

    ui::info("Vendor modules:");
    for name in &names {
        let entry = linked.iter().find(|l| l.relative_path() == *name);
        let (glyph, state) = match entry {
            Some(_) => ("✓", "linked"),
            None => ("○", "vendor"),
        };

        let version = match entry {
            Some(l) => l.version.clone(),
            None => {
                let package = package_name(&htdocs.join("vendor").join(name), name);
                installed_version(htdocs, &package).unwrap_or(None)
            }
        };

        let path = if entry.is_some() {
            htdocs.join(MODULES_DIR).join(name)
        } else {
            htdocs.join("vendor").join(name)
        };

        println!(
            "  {} {:34} {:8} {:14} {}",
            glyph,
            name,
            state,
            version.unwrap_or_else(|| "-".to_string()),
            current_branch(&path)
        );
    }

    Ok(())
}

/// The checked-out branch, or `detached` — a Composer source install sits on a
/// detached HEAD at the locked reference, where `get_current_branch` reports the
/// literal `HEAD`.
fn current_branch(path: &Path) -> String {
    match GitService::open(path).and_then(|g| g.get_current_branch()) {
        Ok(branch) if branch == "HEAD" => "detached".to_string(),
        Ok(branch) => branch,
        Err(_) => "-".to_string(),
    }
}

// ---------------------------------------------------------------------------
// composer.json / composer.lock (read-only; Composer owns the writes)
// ---------------------------------------------------------------------------

/// Raw bytes of `composer.json` and `composer.lock`, for rollback.
///
/// Both are needed: Composer writes the lock file *before* running the install
/// step, so a failure partway through leaves a rewritten lock behind.
struct ComposerSnapshot {
    json: Vec<u8>,
    lock: Option<Vec<u8>>,
}

impl ComposerSnapshot {
    fn capture(htdocs: &Path) -> Result<Self> {
        let json_path = htdocs.join("composer.json");
        Ok(Self {
            json: fs::read(&json_path).context(format!("Failed to read {:?}", json_path))?,
            lock: fs::read(htdocs.join("composer.lock")).ok(),
        })
    }

    fn restore(&self, htdocs: &Path) -> Result<()> {
        fs::write(htdocs.join("composer.json"), &self.json)?;
        if let Some(lock) = &self.lock {
            fs::write(htdocs.join("composer.lock"), lock)?;
        }
        Ok(())
    }
}

/// The version Composer currently has installed, per `composer.lock`.
///
/// This is the right thing to pin: Composer already resolved it against the root
/// constraint, so it normally satisfies it. "Normally" because Composer
/// re-resolves from scratch on `update` and does not trust the lock — if the
/// constraint or `minimum-stability` changed since the lock was written it can
/// still reject the pin, which surfaces as a clear Composer error.
pub fn installed_version(htdocs: &Path, package: &str) -> Result<Option<String>> {
    let lock_path = htdocs.join("composer.lock");
    if !lock_path.exists() {
        return Ok(None);
    }

    let lock: Value = serde_json::from_str(&fs::read_to_string(&lock_path)?)
        .context(format!("Failed to parse {:?}", lock_path))?;

    for key in ["packages", "packages-dev"] {
        if let Some(list) = lock.get(key).and_then(Value::as_array) {
            for entry in list {
                if entry.get("name").and_then(Value::as_str) == Some(package) {
                    return Ok(entry
                        .get("version")
                        .and_then(Value::as_str)
                        .map(str::to_string));
                }
            }
        }
    }

    Ok(None)
}

/// The `path` repositories in `htdocs/composer.json` that point into `modules/`.
///
/// Matches on the entry's shape rather than on its key, so entries written by
/// hand are recognised too. See [`LinkedModule::key`] for how the key that
/// `composer config --unset` needs is derived — the array index is *not* it.
pub fn linked_modules(htdocs: &Path) -> Result<Vec<LinkedModule>> {
    let composer_path = htdocs.join("composer.json");
    let composer: Value = serde_json::from_str(&fs::read_to_string(&composer_path)?)
        .context(format!("Failed to parse {:?}", composer_path))?;

    let entries: Vec<(Option<String>, &Value)> = match composer.get("repositories") {
        Some(Value::Object(map)) => map.iter().map(|(k, v)| (Some(k.clone()), v)).collect(),
        // Array entries are addressed by their `name` property, which Composer
        // writes when it adds one. An entry without a name is unaddressable.
        Some(Value::Array(list)) => list
            .iter()
            .map(|v| (v.get("name").and_then(Value::as_str).map(str::to_string), v))
            .collect(),
        _ => Vec::new(),
    };

    let prefix = format!("{}/", MODULES_DIR);
    let mut linked = Vec::new();

    for (key, entry) in entries {
        if entry.get("type").and_then(Value::as_str) != Some("path") {
            continue;
        }
        let Some(url) = entry.get("url").and_then(Value::as_str) else {
            continue;
        };
        if !url.starts_with(&prefix) {
            continue;
        }

        let versions = entry
            .pointer("/options/versions")
            .and_then(Value::as_object);
        let package = versions
            .and_then(|v| v.keys().next().cloned())
            .unwrap_or_else(|| url.trim_start_matches(&prefix).to_string());
        let version = versions
            .and_then(|v| v.values().next())
            .and_then(Value::as_str)
            .map(str::to_string);

        linked.push(LinkedModule {
            key,
            package,
            url: url.to_string(),
            version,
        });
    }

    linked.sort_by(|a, b| a.package.cmp(&b.package));
    Ok(linked)
}

/// The module's own declared package name, falling back to its directory path.
pub fn package_name(module_dir: &Path, relative: &str) -> String {
    fs::read_to_string(module_dir.join("composer.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .and_then(|v| {
            v.get("name")
                .and_then(Value::as_str)
                .map(|s| s.to_lowercase())
        })
        .unwrap_or_else(|| relative.to_string())
}

fn repo_key(relative: &str) -> String {
    format!("repositories.dc2-{}", relative.replace('/', "-"))
}

fn repo_entry_json(relative: &str, package: &str, pin: &str) -> Result<String> {
    let mut versions = serde_json::Map::new();
    versions.insert(package.to_string(), Value::String(pin.to_string()));

    // `canonical` is deliberately left at its default: canonical filtering is what
    // keeps a newer upstream release from winning over the pinned local checkout.
    let entry = serde_json::json!({
        "type": "path",
        "url": format!("{}/{}", MODULES_DIR, relative),
        "options": {
            "symlink": true,
            "versions": Value::Object(versions),
        }
    });

    Ok(serde_json::to_string(&entry)?)
}

// ---------------------------------------------------------------------------
// filesystem helpers
// ---------------------------------------------------------------------------

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Whether the development checkout has any git remote configured.
///
/// `true` when the answer can't be determined (not a repository, git error): the check exists
/// to catch the one case it is certain about — a locally-created module — and must not block
/// `unlink` on anything else.
fn checkout_has_remote(dev_path: &Path) -> bool {
    match GitService::open(dev_path) {
        Ok(git) => git.has_remotes().unwrap_or(true),
        Err(_) => true,
    }
}

/// `true` when `link` is a symlink resolving to `target`.
fn links_to(link: &Path, target: &Path) -> bool {
    if !is_symlink(link) {
        return false;
    }
    match (link.canonicalize(), target.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Recreates the `vendor/` symlink Composer would have made. Only used to undo a
/// failed `unlink`; the relative form matches what Composer writes.
fn symlink_dev_checkout(relative: &str, vendor_path: &Path) -> Result<()> {
    let target = format!("../../{}/{}", MODULES_DIR, relative);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, vendor_path)?;
    #[cfg(not(unix))]
    return Err(anyhow!("symlinks are only supported on unix"));
    #[cfg(unix)]
    Ok(())
}

/// Leaves the development checkout out of the application repository.
///
/// The entry is written under [`GITIGNORE_MARKER`], which is what makes it
/// removable later: it proves the line is ours rather than one the project
/// already had, so `unlink` can take it back out without risking the project's
/// own `.gitignore` content.
///
/// Returns whether the file was changed.
fn ensure_modules_gitignored(htdocs: &Path) -> Result<bool> {
    let path = htdocs.join(".gitignore");
    let content = fs::read_to_string(&path).unwrap_or_default();

    let already = content.lines().any(|line| {
        matches!(
            line.trim(),
            "/modules" | "/modules/" | "modules" | "modules/"
        )
    });
    if already {
        return Ok(false);
    }

    // Preserve whether the file ended with a newline, so git reports only the
    // added lines rather than also rewriting the previous last line.
    let ends_with_newline = content.is_empty() || content.ends_with('\n');

    let mut updated = content;
    if !ends_with_newline {
        updated.push('\n');
    }
    updated.push_str(GITIGNORE_MARKER);
    updated.push('\n');
    updated.push_str(GITIGNORE_ENTRY);
    if ends_with_newline {
        updated.push('\n');
    }

    fs::write(&path, updated).context(format!("Failed to write {:?}", path))?;
    Ok(true)
}

/// Drops our `.gitignore` entry once nothing is linked and no checkout remains,
/// so a link/unlink round trip leaves `htdocs/` as it was found.
fn tidy_modules_gitignore(htdocs: &Path) {
    let still_linked = linked_modules(htdocs)
        .map(|l| !l.is_empty())
        .unwrap_or(true);
    let modules_dir = htdocs.join(MODULES_DIR);
    let has_checkouts = fs::read_dir(&modules_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);

    if still_linked || has_checkouts {
        return;
    }

    let _ = fs::remove_dir(&modules_dir);
    if let Err(e) = remove_modules_gitignore(htdocs) {
        ui::debug(format!("Could not tidy htdocs/.gitignore: {}", e));
    }
}

/// Removes `modules/<vendor>/` and `modules/` once they are empty, so a rollback
/// or a `--purge` leaves no stray directories. `remove_dir` only ever removes an
/// empty directory, so this cannot delete a sibling module's checkout.
fn prune_empty_module_dirs(htdocs: &Path, dev_path: &Path) {
    let mut dir = dev_path.parent();
    while let Some(d) = dir {
        if d == htdocs || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// Exact inverse of [`ensure_modules_gitignored`], preserving the file's original
/// trailing-newline state — including the case where that function *created* the file:
/// if our two lines were all it held, the file is removed rather than truncated to zero
/// bytes, so a link/unlink round trip really does leave `htdocs/` as it was found.
///
/// Removes the entry **only** where [`GITIGNORE_MARKER`] immediately precedes it,
/// which is the proof that we wrote it. An unmarked `/modules/` line the project
/// added itself is left alone, as is a marker whose entry has been edited.
fn remove_modules_gitignore(htdocs: &Path) -> Result<()> {
    let path = htdocs.join(".gitignore");
    let Ok(content) = fs::read_to_string(&path) else {
        return Ok(());
    };

    let ends_with_newline = content.ends_with('\n');
    let lines: Vec<&str> = content.lines().collect();

    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut removed = false;
    let mut i = 0;
    while i < lines.len() {
        let is_our_pair = lines[i].trim() == GITIGNORE_MARKER
            && lines.get(i + 1).map(|l| l.trim()) == Some(GITIGNORE_ENTRY);
        if is_our_pair {
            i += 2;
            removed = true;
            continue;
        }
        kept.push(lines[i]);
        i += 1;
    }

    if !removed {
        return Ok(());
    }

    let mut updated = kept.join("\n");
    if updated.trim().is_empty() {
        fs::remove_file(&path).context(format!("Failed to remove {:?}", path))?;
        return Ok(());
    }
    if ends_with_newline {
        updated.push('\n');
    }

    fs::write(&path, updated)?;
    Ok(())
}

/// Puts the checkout on the branch the pin implies, so the developer is not left
/// committing onto the detached HEAD a source install leaves behind. Advisory:
/// a missing branch is reported, not treated as a failure.
///
/// Returns the commit HEAD pointed at beforehand, but only when the checkout actually
/// moved — [`restore_detached_head`] uses it to undo this on rollback, so that a failed
/// `composer update` doesn't leave a vendor install sitting on a branch instead of the
/// detached HEAD Composer put it on.
fn checkout_pinned_branch(dev_path: &Path, pin: &str) -> Option<String> {
    let branch = branch_from_pin(pin)?;
    let git = GitService::open(dev_path).ok()?;

    if git.get_current_branch().ok().as_deref() == Some(branch.as_str()) {
        return None;
    }

    let previous = git.head_commit_id().ok();

    match git.checkout_branch(&branch) {
        Ok(()) => {
            ui::info(format!("  checked out branch {}", branch));
            previous
        }
        Err(_) => {
            ui::warning(format!(
                "  could not check out branch {} — the checkout is on a detached HEAD, \
                 switch branches before committing",
                branch
            ));
            None
        }
    }
}

/// Undoes [`checkout_pinned_branch`]. Best-effort: the checkout is about to be moved back
/// into `vendor/` where `composer install` would fix HEAD anyway, so a failure here is
/// reported at debug level rather than escalated over the error that caused the rollback.
fn restore_detached_head(dev_path: &Path, commit: &str) {
    let result = GitService::open(dev_path).and_then(|git| git.checkout_detached(commit));
    match result {
        Ok(()) => ui::debug(format!(
            "restored detached HEAD at {} in {:?}",
            commit, dev_path
        )),
        Err(e) => ui::debug(format!("could not restore detached HEAD: {}", e)),
    }
}

/// `2.4.x-dev` -> `2.4.x`, `dev-main` -> `main`. A concrete version pins no branch.
fn branch_from_pin(pin: &str) -> Option<String> {
    if let Some(rest) = pin.strip_prefix("dev-") {
        return Some(rest.to_string());
    }
    pin.strip_suffix("-dev").map(str::to_string)
}

// ---------------------------------------------------------------------------
// shared plumbing
// ---------------------------------------------------------------------------

fn require_running_stack(project_dir: &Path, options: &ModuleOptions) -> Result<()> {
    if options.skip_composer || docker::is_running(project_dir) {
        return Ok(());
    }
    Err(anyhow!(
        "The project containers are not running — Composer runs inside the php container. \
         Start them with `docker-control start` first."
    ))
}

/// Runs Composer in the application root, non-interactively.
fn composer(project_dir: &Path, args: &[&str], options: &ModuleOptions) -> Result<()> {
    if options.skip_composer {
        ui::debug(format!("skipping composer {}", args.join(" ")));
        return Ok(());
    }

    let mut full = vec!["composer"];
    full.extend_from_slice(args);

    docker::exec_as_user(
        project_dir,
        COMPOSER_SERVICE,
        COMPOSER_USER,
        Some(CONTAINER_HTDOCS),
        &[("COMPOSER_HOME", COMPOSER_HOME)],
        &full,
    )
}

/// Runs `composer init` with a TTY so it can ask its questions.
///
/// Composer reports its own failures on the terminal the developer is already looking at, so
/// this turns a non-zero exit into the shortest error that still triggers the caller's
/// rollback, rather than repeating Composer's output.
fn composer_init(
    project_dir: &Path,
    workdir: &str,
    args: &[&str],
    options: &ModuleOptions,
) -> Result<()> {
    if options.skip_composer {
        ui::debug(format!("skipping composer {}", args.join(" ")));
        return Ok(());
    }

    let mut full = vec!["composer"];
    full.extend_from_slice(args);

    let code = docker::exec_interactive(
        project_dir,
        COMPOSER_SERVICE,
        COMPOSER_USER,
        Some(workdir),
        &[("COMPOSER_HOME", COMPOSER_HOME)],
        &full,
    )?;

    if code != 0 {
        return Err(anyhow!("composer init exited with {}", code));
    }
    Ok(())
}

fn resolve_link_target(
    project_dir: &Path,
    linked: &[LinkedModule],
    module: Option<String>,
    options: &ModuleOptions,
) -> Result<String> {
    let available = GitService::list_vendor_modules(project_dir)?;

    if let Some(m) = module {
        let m = m.trim_matches('/').to_string();
        if available.contains(&m) || linked.iter().any(|l| l.relative_path() == m) {
            return Ok(m);
        }
        // Also accept a module already moved out but not yet wired up.
        if project_dir
            .join("htdocs")
            .join(MODULES_DIR)
            .join(&m)
            .exists()
        {
            return Ok(m);
        }
        return Err(anyhow!(
            "Module {} not found. Available: {}",
            m,
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            }
        ));
    }

    let candidates: Vec<String> = available
        .into_iter()
        .filter(|m| !linked.iter().any(|l| l.relative_path() == *m))
        .collect();

    if candidates.is_empty() {
        return Err(anyhow!(
            "No unlinked vendor modules found in htdocs/vendor. Run `docker-control module list`."
        ));
    }

    options.prompt_provider.select_module_to_link(candidates)
}

fn resolve_unlink_target(
    linked: &[LinkedModule],
    module: Option<String>,
    options: &ModuleOptions,
) -> Result<LinkedModule> {
    if let Some(m) = module {
        let m = m.trim_matches('/');
        return linked
            .iter()
            .find(|l| l.relative_path() == m || l.package == m)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "{} is not linked. Linked modules: {}",
                    m,
                    linked
                        .iter()
                        .map(|l| l.package.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
    }

    if linked.len() == 1 {
        return Ok(linked[0].clone());
    }

    let names: Vec<String> = linked.iter().map(|l| l.package.clone()).collect();
    let selection = options.prompt_provider.select_module_to_unlink(names)?;
    linked
        .iter()
        .find(|l| l.package == selection)
        .cloned()
        .ok_or_else(|| anyhow!("Unknown module {}", selection))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `htdocs` with the given `.gitignore` and no linked modules.
    fn htdocs_with_gitignore(gitignore: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let htdocs = temp.path().join("htdocs");
        fs::create_dir_all(&htdocs).unwrap();
        fs::write(htdocs.join("composer.json"), r#"{"name":"t/r"}"#).unwrap();
        if let Some(content) = gitignore {
            fs::write(htdocs.join(".gitignore"), content).unwrap();
        }
        (temp, htdocs)
    }

    fn gitignore(htdocs: &Path) -> String {
        fs::read_to_string(htdocs.join(".gitignore")).unwrap_or_default()
    }

    #[test]
    fn gitignore_entry_round_trips_byte_for_byte() {
        // Both trailing-newline states must survive add-then-remove exactly.
        for original in ["vendor/\ndata/cache", "vendor/\ndata/cache\n"] {
            let (_t, htdocs) = htdocs_with_gitignore(Some(original));

            assert!(ensure_modules_gitignored(&htdocs).unwrap());
            let linked = gitignore(&htdocs);
            assert!(linked.contains(GITIGNORE_MARKER), "marker written");
            assert!(linked.contains(GITIGNORE_ENTRY), "entry written");
            assert_eq!(
                linked.ends_with('\n'),
                original.ends_with('\n'),
                "trailing-newline state must be preserved: {:?}",
                linked
            );

            remove_modules_gitignore(&htdocs).unwrap();
            assert_eq!(gitignore(&htdocs), original, "round trip must be exact");
        }
    }

    #[test]
    fn psr4_namespace_studly_cases_each_segment() {
        assert_eq!(psr4_namespace("acme/widget"), "Acme\\Widget\\");
        assert_eq!(psr4_namespace("acme/my-widget"), "Acme\\MyWidget\\");
        assert_eq!(psr4_namespace("acme/my_widget"), "Acme\\MyWidget\\");
        assert_eq!(psr4_namespace("acme/my.widget"), "Acme\\MyWidget\\");
        assert_eq!(psr4_namespace("acme-corp/widget"), "AcmeCorp\\Widget\\");
        // Digits are not word breaks.
        assert_eq!(psr4_namespace("acme/oauth2"), "Acme\\Oauth2\\");
    }

    #[test]
    fn package_names_composer_would_reject_are_rejected_first() {
        for good in [
            "acme/widget",
            "acme/my-widget",
            "acme/wid--get",
            "acme-corp/widget.js",
            "a/b",
            "acme2/widget3",
        ] {
            assert!(
                validate_package_name(good).is_ok(),
                "{} must be valid",
                good
            );
        }
        for bad in [
            "acme",
            "acme/",
            "/widget",
            "acme/widget/extra",
            "Acme/Widget",
            "acme/-widget",
            "acme/widget-",
            ".acme/widget",
            "acme/wid---get",
            "acme/wid get",
        ] {
            assert!(
                validate_package_name(bad).is_err(),
                "{} must be rejected",
                bad
            );
        }
    }

    #[test]
    fn ensure_psr4_autoload_leaves_an_existing_mapping_alone() {
        let temp = tempfile::tempdir().unwrap();
        let original = "{\n  \"name\": \"acme/widget\",\n  \"autoload\": {\"psr-4\": {\"Custom\\\\\": \"lib/\"}}\n}\n";
        fs::write(temp.path().join("composer.json"), original).unwrap();

        ensure_psr4_autoload(temp.path(), "acme/widget").unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("composer.json")).unwrap(),
            original,
            "composer init already wrote a mapping; it must be untouched"
        );
    }

    #[test]
    fn ensure_psr4_autoload_adds_a_mapping_when_there_is_none() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("composer.json"),
            r#"{"name": "acme/my-widget"}"#,
        )
        .unwrap();

        ensure_psr4_autoload(temp.path(), "acme/my-widget").unwrap();

        let value: Value =
            serde_json::from_str(&fs::read_to_string(temp.path().join("composer.json")).unwrap())
                .unwrap();
        assert_eq!(value["autoload"]["psr-4"]["Acme\\MyWidget\\"], "src/");
        assert_eq!(value["name"], "acme/my-widget", "the rest must survive");
    }

    #[test]
    fn rollback_create_restores_the_application_but_keeps_the_new_module() {
        let (_t, htdocs) = htdocs_with_gitignore(None);
        let original = fs::read_to_string(htdocs.join("composer.json")).unwrap();

        // The state create reaches just before a failing `composer require`.
        let dev_path = htdocs.join(MODULES_DIR).join("acme/widget");
        fs::create_dir_all(dev_path.join("src")).unwrap();
        fs::write(dev_path.join("composer.json"), "{}").unwrap();
        let snapshot = ComposerSnapshot::capture(&htdocs).unwrap();
        let state = CreateState {
            gitignore_added: ensure_modules_gitignored(&htdocs).unwrap(),
        };
        fs::write(htdocs.join("composer.json"), r#"{"wrecked": true}"#).unwrap();

        rollback_create(&htdocs, &snapshot, &dev_path, &state);

        assert_eq!(
            fs::read_to_string(htdocs.join("composer.json")).unwrap(),
            original,
            "the application must be back as it was"
        );
        assert!(
            !htdocs.join(".gitignore").exists(),
            "the .gitignore we created must be gone"
        );
        assert!(
            dev_path.join("composer.json").exists(),
            "the new module is the developer's work and must survive"
        );
    }

    #[test]
    fn gitignore_entry_is_added_once() {
        let (_t, htdocs) = htdocs_with_gitignore(Some("vendor/\n"));

        assert!(ensure_modules_gitignored(&htdocs).unwrap());
        assert!(
            !ensure_modules_gitignored(&htdocs).unwrap(),
            "second call must report no change"
        );
        assert_eq!(gitignore(&htdocs).matches(GITIGNORE_ENTRY).count(), 1);
        assert_eq!(gitignore(&htdocs).matches(GITIGNORE_MARKER).count(), 1);
    }

    #[test]
    fn an_unmarked_entry_is_never_removed() {
        // The project ignores modules/ itself. We must neither duplicate it nor,
        // later, delete it — that line is not ours.
        for original in [
            "vendor/\n/modules/\n",
            "vendor/\nmodules/\n",
            "vendor/\n/modules\n",
        ] {
            let (_t, htdocs) = htdocs_with_gitignore(Some(original));

            assert!(
                !ensure_modules_gitignored(&htdocs).unwrap(),
                "already ignored, nothing to add"
            );
            remove_modules_gitignore(&htdocs).unwrap();
            assert_eq!(gitignore(&htdocs), original, "the project's line must stay");
        }
    }

    #[test]
    fn a_marker_whose_entry_was_edited_is_left_alone() {
        let edited = format!("vendor/\n{}\n/modules/subdir/\n", GITIGNORE_MARKER);
        let (_t, htdocs) = htdocs_with_gitignore(Some(&edited));

        remove_modules_gitignore(&htdocs).unwrap();
        assert_eq!(
            gitignore(&htdocs),
            edited,
            "only the exact marker+entry pair may be removed"
        );
    }

    #[test]
    fn gitignore_is_created_when_absent() {
        let (_t, htdocs) = htdocs_with_gitignore(None);

        assert!(ensure_modules_gitignored(&htdocs).unwrap());
        let written = gitignore(&htdocs);
        assert!(written.starts_with(GITIGNORE_MARKER), "got {:?}", written);
        assert!(written.contains(GITIGNORE_ENTRY));
    }

    #[test]
    fn a_gitignore_we_created_is_removed_again_not_left_empty() {
        // The round trip has to leave htdocs as it was found, which for a project without
        // a .gitignore means no file — not a stray zero-byte one.
        let (_t, htdocs) = htdocs_with_gitignore(None);

        assert!(ensure_modules_gitignored(&htdocs).unwrap());
        remove_modules_gitignore(&htdocs).unwrap();

        assert!(
            !htdocs.join(".gitignore").exists(),
            "stray .gitignore left behind: {:?}",
            gitignore(&htdocs)
        );
    }

    #[test]
    fn tidy_only_fires_once_nothing_is_linked_and_no_checkout_remains() {
        let (_t, htdocs) = htdocs_with_gitignore(Some("vendor/\n"));
        ensure_modules_gitignored(&htdocs).unwrap();

        // A leftover checkout still needs ignoring.
        let checkout = htdocs.join(MODULES_DIR).join("acme/widget");
        fs::create_dir_all(&checkout).unwrap();
        tidy_modules_gitignore(&htdocs);
        assert!(
            gitignore(&htdocs).contains(GITIGNORE_ENTRY),
            "an un-purged checkout must stay ignored"
        );

        // Still linked, even with no checkout on disk.
        fs::remove_dir_all(htdocs.join(MODULES_DIR)).unwrap();
        fs::write(
            htdocs.join("composer.json"),
            r#"{"repositories":[{"name":"dc2-acme-widget","type":"path","url":"modules/acme/widget",
                "options":{"symlink":true,"versions":{"acme/widget":"1.0.0"}}}]}"#,
        )
        .unwrap();
        tidy_modules_gitignore(&htdocs);
        assert!(
            gitignore(&htdocs).contains(GITIGNORE_ENTRY),
            "a linked module must stay ignored"
        );

        // Neither: now it goes.
        fs::write(htdocs.join("composer.json"), r#"{"name":"t/r"}"#).unwrap();
        tidy_modules_gitignore(&htdocs);
        assert_eq!(gitignore(&htdocs), "vendor/\n");
        assert!(
            !htdocs.join(MODULES_DIR).exists(),
            "the empty modules/ dir goes too"
        );
    }

    #[test]
    fn branch_from_pin_handles_both_dev_forms() {
        assert_eq!(branch_from_pin("2.4.x-dev").as_deref(), Some("2.4.x"));
        assert_eq!(branch_from_pin("dev-main").as_deref(), Some("main"));
        assert_eq!(branch_from_pin("2.4.3"), None);
    }

    #[test]
    fn repo_key_is_derived_from_the_module_path() {
        assert_eq!(repo_key("acme/widget"), "repositories.dc2-acme-widget");
    }

    #[test]
    fn repo_entry_pins_the_package_version() {
        let json = repo_entry_json("acme/widget", "acme/widget", "2.4.x-dev").unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["type"], "path");
        assert_eq!(value["url"], "modules/acme/widget");
        assert_eq!(value["options"]["symlink"], true);
        assert_eq!(value["options"]["versions"]["acme/widget"], "2.4.x-dev");
        // Left unset on purpose — canonical filtering keeps upstream from winning.
        assert!(value["options"].get("canonical").is_none());
    }

    #[test]
    fn linked_module_strips_the_modules_prefix() {
        let m = LinkedModule {
            key: Some("dc2-acme-widget".to_string()),
            package: "acme/widget".to_string(),
            url: "modules/acme/widget".to_string(),
            version: None,
        };
        assert_eq!(m.relative_path(), "acme/widget");
    }
}
