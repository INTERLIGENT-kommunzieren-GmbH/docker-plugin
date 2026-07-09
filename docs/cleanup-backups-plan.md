# Add `cleanup-backups` command

## Context

`src/commands/update.rs` and `src/commands/migrate.rs` each create a local
`backup_<unix_timestamp>/` folder in the project directory before overwriting
files, as a safety net. Nothing ever deletes these folders, so they
accumulate indefinitely and consume disk space. There is currently no
command, flag, or config option anywhere in the codebase to prune them
(confirmed via full-codebase search — "backup" cleanup/retention logic only
exists for remote deploy releases, which is a separate, already-handled
concern via a hardcoded "keep last 5" `xargs rm -rf` in `deploy.rs`, not the
target of this task).

This plan adds a new `cleanup-backups` subcommand that lists and deletes old
local `backup_*` folders, with configurable retention (keep count or max
age), a dry-run mode, and an interactive confirmation (skippable with
`--yes`), matching the safety patterns already used by `deploy` (`--yes`)
and `add_deploy_config`/`create_script` (`inquire` prompts).

## Design decisions (confirmed with user)

- Target: local `backup_*` folders from `update`/`migrate` (not remote
  release retention, which already exists and is out of scope).
- Retention policy: support **both** `--keep <N>` (keep N most recent,
  default 5 if neither flag given) and `--older-than <DAYS>` (delete
  anything older than N days). Mutually exclusive via clap `conflicts_with`.
- Safety: interactive confirmation by default (using `inquire::Confirm`,
  same crate already used in `main.rs::maybe_offer_image_pull` and
  `create_script.rs`), `--yes` to skip the prompt, `--dry-run` to only list
  candidates without deleting.

## Implementation

### 1. New utility: reverse of `exclude_from_phpstorm`

`src/utils/mod.rs` has `exclude_from_phpstorm(project_dir, folder_name)`
(`src/utils/mod.rs:87-124`), called by `update.rs:33` and `migrate.rs:42`
right after a backup folder is created. It inserts an
`<excludeFolder url="file://$MODULE_DIR$/backup_.../" />` line into every
`*.iml` file under `.idea/`. There is no existing counterpart to remove that
line, so once we delete a backup folder, stale exclude entries would be left
behind pointing at nonexistent directories.

Add `remove_phpstorm_exclude(project_dir: &Path, folder_name: &str) -> Result<()>`
next to `exclude_from_phpstorm` in `src/utils/mod.rs`, mirroring its file
selection logic (iterate `.idea/*.iml`) but stripping the matching
`<excludeFolder .../>` line (and its trailing newline) if present instead of
inserting it. Call this after successfully removing each backup directory.

### 2. New command file: `src/commands/cleanup_backups.rs`

```rust
pub fn execute(
    project_dir: &Path,
    keep: Option<usize>,
    older_than_days: Option<u64>,
    yes: bool,
    dry_run: bool,
) -> Result<()>
```

Logic:
1. Scan top-level entries of `project_dir` (plain `fs::read_dir`, not
   recursive) for directories named `backup_<digits>`, parsing the suffix as
   `u64` (matches the `SystemTime::now().duration_since(UNIX_EPOCH).as_secs()`
   naming used in `update.rs:25-28` / `migrate.rs:34-37`).
2. Sort descending by timestamp (newest first).
3. If none found, `ui::info("No backup folders found.")` and return.
4. Compute candidates for deletion:
   - `older_than_days` given → cutoff = now (secs) − days×86400; candidates
     = entries older than cutoff.
   - otherwise → `effective_keep = keep.unwrap_or(5)`; candidates = entries
     beyond the first `effective_keep` newest.
5. If no candidates, `ui::info` a "nothing to clean up" message and return.
6. Print each candidate with a human-readable age (e.g. "backup_1700000000
   (45 days old)").
7. If `dry_run`, print a summary of what *would* be deleted and return
   without touching the filesystem.
8. If not `yes`, prompt with `inquire::Confirm::new(...).with_default(false)`;
   on decline, print "Aborted." and return `Ok(())`.
9. For each candidate: `fs::remove_dir_all`, then
   `utils::remove_phpstorm_exclude`. On per-item failure, `ui::warning` and
   continue with the rest (don't abort the whole batch — consistent with the
   best-effort style of `migrate.rs`'s restore steps).
10. `ui::success` a final count summary ("Removed N of M backup folder(s).").

Do **not** call `utils::is_managed` / `check_managed` — `update` and
`migrate` don't require it either (see `main.rs` dispatch: `Commands::Update`
and `Commands::Migrate` call straight into their `execute()` with no
`check_managed` wrapper), since backups can predate/survive the managed
sentinel file.

### 3. Wire up the command

- `src/commands/mod.rs`: add `pub mod cleanup_backups;`
- `src/main.rs` `Commands` enum: add a new variant, alphabetically placed
  near `Build`/`Console` (matching the enum's existing alphabetical-ish
  ordering):
  ```rust
  /// Clean up old local backup_* folders created by update/migrate
  CleanupBackups {
      /// Number of most-recent backups to keep (default 5)
      #[arg(short, long, conflicts_with = "older_than")]
      keep: Option<usize>,

      /// Delete backups older than this many days
      #[arg(long, conflicts_with = "keep")]
      older_than: Option<u64>,

      /// List backups that would be deleted without deleting them
      #[arg(long)]
      dry_run: bool,

      /// Skip interactive confirmation
      #[arg(short, long)]
      yes: bool,
  },
  ```
- `src/main.rs` dispatch `match command`: add
  ```rust
  Commands::CleanupBackups { keep, older_than, dry_run, yes } => {
      commands::cleanup_backups::execute(&project_dir, keep, older_than, yes, dry_run)?;
  }
  ```

### 4. Tests

- Unit tests in `cleanup_backups.rs` for the pure selection logic (given a
  list of `(name, timestamp)` pairs, verify `--keep N` and `--older-than D`
  each pick the right candidates), using a `tempfile::tempdir()` with
  actual `backup_<ts>` directories created — no need to mock anything since
  this command has no SSH/docker dependency.
- Optionally an integration test `tests/cleanup_backups_tests.rs` following
  the `TestRepo` pattern in `tests/common/mod.rs`: scaffold a project dir,
  create a few `backup_<ts>` folders with distinct timestamps, run the
  compiled binary with `--dry-run` and with `--keep`/`--older-than`, assert
  on stdout output and on which directories remain on disk afterward.

## Verification

- `cargo build`
- `cargo clippy`
- `cargo nextest run cleanup_backups`
- Manual smoke test: in a scratch directory, create a few
  `backup_<timestamp>` folders (and optionally a `.idea/*.iml` with a
  matching exclude entry), then run:
  - `cargo run -- cleanup-backups --dry-run` → confirm it lists candidates
    without deleting anything
  - `cargo run -- cleanup-backups --keep 1 --yes` → confirm only the newest
    folder remains and the corresponding `.iml` exclude lines for deleted
    folders are gone
  - `cargo run -- cleanup-backups --older-than 9999 --yes` → confirm no-op
    ("nothing to clean up") when nothing is old enough