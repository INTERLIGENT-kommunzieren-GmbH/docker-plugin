# Changelog

All notable changes since 2.3.0 are documented here.

## 2.5.0 — 2026-07-29
- `update` now applies only what actually changed. `init`/`update`/`migrate` record the template's file hashes in `.docker-control/state.json` at the project root, giving a merge base: a file the template changed but you didn't is applied silently, a file you both changed prompts (keep mine / take the template's / show diff / write `*.dist`), a file dropped from the template is reported rather than deleted, and a file the template didn't touch is left alone no matter what you edited. Projects predating the state file are asked once how to treat their differing files instead of being overwritten.
- `start`, `restart` and `status` now tell you when the project template has changed — and only when it genuinely has. The check compares content hashes, not version numbers, so upgrades that don't touch the template stay silent; its fast path is a single fingerprint comparison, so it is never throttled.
- Add `update --check` (report pending changes and diffs, change nothing, exit non-zero when anything is pending) and `update --force-template` (the previous wholesale overwrite).
- `update` no longer resets per-project files to the template's placeholders: `secrets/*.txt` and `config/htpasswd` are seeded at `init` and never overwritten again.
- `.env` is never rewritten. When the template's `.env-dist` gains a key your `.env` lacks, the key is reported and you choose the value — repeated until the key is actually present, since nothing else will add it for you. The project's `.env-dist` (which `init` ships as the reference list of available keys) is kept in sync like any other template file.
- Conflict resolutions are remembered: answering "keep my version" records the current template as the base, so the same file isn't queried again until the template changes it anew. The same applies to the one-off prompt for projects predating the state file.
- `update --yes` no longer discards local edits silently. A conflicting file is kept and the template's version is written beside it as `*.dist`, so an unattended run can't lose work; `--force-template` restores the old behaviour.
- Fix the template sync silently skipping files. rsync's size+mtime quick check would decline to copy a file whose size matched the template's and whose mtime landed in the same second — so a template change could be reported as applied without being written. Both the selective and wholesale (`--force-template`) paths now pass `--ignore-times`, since the files to copy are selected by content hash and rsync must not overrule that.

## 2.4.14 — 2026-07-28
- The project template's `CLAUDE.md` now points explicitly at `htdocs/CLAUDE.md` as the place for project-specific instructions and persistent memory, and states that the root file is template-owned and overwritten by `update`/`migrate`.
- Fix `migrate` restoring the project-root `CLAUDE.md` from its backup. That file ships with the template, so restoring it shadowed the refreshed copy and left migrated projects pinned to a stale version. `htdocs/` (including `htdocs/CLAUDE.md`), `.claude/` and `.idea/` are still preserved, and the previous root copy remains in the backup folder.

## 2.4.13 — 2026-07-27
- Capistrano (when a project uses it) now builds on a Debian base (`ruby:3.3.8-slim`) instead of Alpine, for broader compatibility. The bundled `build/capistrano/Dockerfile` is refreshed during `migrate` and `update` for projects that already have one — and the image is rebuilt only when the file actually changed. New projects are unaffected: Capistrano remains opt-in and is never created where it doesn't already exist.
- Fix `doctor` reporting hundreds of false-positive permission errors. It no longer flags read-only files that `www-data` already owns (e.g. git's immutable object files in `.composer/cache` and vendor `.git` dirs) — a named-user ACL can never make an owner's file writable, and these are harmless. Broken symlinks (missing vendor targets) are likewise no longer misreported as unreadable. `doctor` now flags only genuine problems: paths `www-data` can't read, directories it can't write, or files it can't write that it doesn't own.
- `update` now syncs the template with `sudo rsync -a` instead of an in-process copy, so it can overwrite project files a container created as root/www-data (previously it aborted mid-update with "Permission denied"). Because `rsync -a` preserves the template's ownership, refreshed files end up owned by the invoking user, clearing the permission problem going forward. `logs`/`volumes` are still skipped and files absent from the template are preserved.

## 2.4.12 — 2026-07-22
- Add `install-claude` command to install Claude Code via Anthropic's official installer, then install codebase-memory-mcp via its official installer and enable its auto-indexing of new projects.
- Add `doctor` command to check that `htdocs`/`/var/www` is readable/writable — the host ACL on `htdocs` and, for the container's `www-data` user, every path under `/var/www`. With `--fix` it creates the Composer/XDG home dirs (`.composer`, `.config`, `.cache`, `.local`), re-applies the container ACL, and replays the host ACL; it preflights that `setfacl` exists in the php image and reports a clear message if the `acl` package is missing.

## 2.4.11 — 2026-07-20
- Stop flagging optional dependencies at startup: they're now checked only on demand by the command that needs them (`deploy` → `7z`, `trust-ca` → `certutil`, `start`/`restart` → the ACL tools), so `docker control` no longer prints "Optional dependency … is missing" warnings on every invocation.

## 2.4.10 — 2026-07-20
- Add `user-manual` command to open the bundled PDF manual in the default PDF app (opens the Windows viewer on WSL); ship `USER-MANUAL.pdf` via the Homebrew distribution.
- Enforce per-command dependencies with a direct install offer: `deploy` requires `7z`, `start`/`restart`/`setacl` require the ACL tools on Linux, and `trust-ca` requires `certutil` when a browser is present. Homebrew is treated as non-optional.
- Show per-command help via `<command> --help`, and let custom scripts implement their own help via a `_help_` hook.
- Add `--all` flag to `cleanup-backups` to remove every backup.

## 2.4.9 — 2026-07-16
- Verify SSH host keys with trust-on-first-use for `git2` clones/fetches.
- Fix `install-deps`: use the `p7zip` formula and install `acl` for `setfacl`/`getfacl`.

## 2.4.8 — 2026-07-16
- **template/compose.yml**: add `--innodb_snapshot_isolation=OFF` to the mariadb command (compatibility fix).

## 2.4.7 — 2026-07-15
- Confirm before `update` rewrites the project.
- Add `install-deps` command for Homebrew dependencies.

## 2.4.6 — 2026-07-15
- **ingress**: update restart policy.

## 2.4.5 — 2026-07-15
- Trust ingress CA in Firefox and offer `certutil` via Homebrew.
- **template**: add `CLAUDE.md` to the project template.

## 2.4.4 — 2026-07-15
- Enable git2 SSH and HTTPS features for cloning.

## 2.4.3 — 2026-07-13
- **template**: pin `mariadb` and `gotenberg` image versions in compose.

## 2.4.2 — 2026-07-13
- Support macOS ACLs via `chmod` instead of `setfacl`/`getfacl`.

## 2.4.1 — 2026-07-09
- Gate the `update` command behind the managed-project check.

## 2.4.0 — 2026-07-09
- Add weekly-throttled checks for outdated images and docker-control updates.
- Offer to install missing dependencies via Homebrew, add `getfacl` check.
- Add `trust-ca` command to install the ingress CA into the OS trust store.
- Prompt on custom-script/built-in name clashes, add `_override_` convention.
- Add `cleanup-backups` command to prune old `backup_*` folders.
- **template**: fix apache mail config.
- Fix clippy `unnecessary_sort_by` lint in `cleanup_backups`.

## 2.3.12 — 2026-07-07
- Fix rustfmt formatting in `acl.rs`.

## 2.3.11 — 2026-07-07
- Fix host ACL already-set detection: force numeric `getfacl -n` output so the uid check matches.

## 2.3.10 — 2026-07-07
- Drop the `brew update` step from `upgrade` (upgrade the formula directly).

## 2.3.9 — 2026-07-07
- Add `upgrade` command to update docker-control itself via Homebrew.

## 2.3.8 — 2026-07-07
- Fix rustfmt formatting in `acl.rs`.

## 2.3.7 — 2026-07-07
- Show a progress message while checking container images for updates (explains why start/restart may pause).

## 2.3.6 — 2026-07-07
- Show progress messages when applying host/container ACL permissions on htdocs (explains the sudo prompt on start/restart/setacl).

## 2.3.5 — 2026-07-07
- Fix rustfmt formatting in `sanitize_command_name`.

## 2.3.4 — 2026-07-07
- Fix path traversal and injection bugs found in code review.

## 2.3.3 — 2026-07-07
- Check for outdated images before starting containers.

## 2.3.2 — 2026-07-07
- Prompt for description and script location in `create-control-script`.
- Add `setacl` command and apply ACL fixes on restart.

## 2.3.1 — 2026-07-06
- Add ACL management.
- **template**: route mailpit through apache.