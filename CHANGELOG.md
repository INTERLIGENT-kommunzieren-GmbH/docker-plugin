# Changelog

All notable changes since 2.3.0 are documented here.

## 2.6.0 — 2026-08-31
- Add `module link` / `module unlink` / `module list` for developing a vendor module in place. Vendor modules are source installs, so `htdocs/vendor/<vendor>/<name>/` is a real git clone — but edits there are discarded by the next `composer install`. `module link` moves the clone to `htdocs/modules/<vendor>/<name>/` and adds a Composer `path` repository (`symlink: true`) that symlinks it back into `vendor/`, so edits survive. The version is pinned via `options.versions` to whatever `composer.lock` already records, so your `require` constraint is never edited and there is no state file — the repository entry itself is the state. `release` and `merge` keep working on a linked module, since they reach it through the symlink.
- `module link` also ignores the checkout in `htdocs/.gitignore` and registers it as an extra git root in `.idea/vcs.xml`, so PhpStorm shows the module's branches, commits and diffs; `unlink` reverses both. The `.gitignore` entry is written under a `# docker-control:` marker comment, which identifies it as the command's own — so `unlink` can remove it without ever deleting a `/modules/` line the project already had, and a link/unlink round trip leaves `composer.json`, `composer.lock`, `vendor/` and `.gitignore` byte-for-byte as they were. The `.idea` handling is a no-op for projects without one.
- Both `module link` and `module unlink` verify the result on disk rather than trusting Composer's exit status, because Composer can report success without having done the thing: if another repository in `composer.json` outranks the path entry, `update` exits 0 having re-cloned from upstream, and if the path entry survives removal it exits 0 having re-created the symlink. Either case rolls back `composer.json`/`composer.lock` and reports what to fix.
- `module unlink` never destroys work: it leaves the development checkout in `htdocs/modules/` unless `--purge` is given, and `--purge` first checks for uncommitted changes and for commits that exist on no remote. Because a Composer source install leaves the checkout on a detached HEAD, that check looks for commits unreachable from *any* remote ref rather than comparing against `origin/<branch>`, and `link` checks out the branch the pin implies so you aren't left committing to a detached HEAD.
- Add `module create <vendor>/<name>` for a module that does not exist yet. `link` assumes the package is already installed; `create` scaffolds one — `htdocs/modules/<vendor>/<name>/src/`, `git init` on `main`, `composer init` run **interactively** in the module directory so Composer asks its usual questions, a PSR-4 mapping for `src/` when `composer init` didn't write one (namespace derived from the package name, so `acme/my-widget` autoloads `Acme\MyWidget\`), and an initial commit — then wires it into the application with the same `path` repository `link` uses, pinned `dev-main`.
- `module create` also adds the application's `require`, which `link` deliberately never does: nothing requires a brand-new module, so `vendor/<vendor>/<name>` would never appear without it. That splits what you commit — the path repository must stay out of a commit, while the `require` belongs in one *once the module is pushed* somewhere the application's other repositories can reach. If `create` fails partway through, the application side is rolled back but the new module is kept: those files are the developer's work, not something Composer can reproduce.
- `module unlink` now refuses a module whose checkout has no git remote, before touching anything. Such a module exists only in `htdocs/modules/`, so removing the path repository — currently the package's only source — would leave the application requiring a package nothing can supply, and the failure would land after `composer.lock` had already been rewritten.
- `console` can now run a one-shot command instead of opening a shell: everything after a `--` (`console -- composer install`, `console db -- mysql -e '…'`) runs in the container as the same user and in the same working directory the interactive shell would use, and `docker-control` exits with that command's own status code, so it composes in scripts and `&&` chains. The `--` is required, which is what keeps the command unambiguous against the optional service name. The command is passed as arguments rather than through a shell, so pipes and redirects need an explicit `bash -lc '…'`; a TTY is allocated only when this process has one on both ends, so redirecting the output to a file doesn't produce CRLF line endings. For the `php` service the one-shot path passes `COMPOSER_HOME=/var/www/.composer` for the reason documented below — otherwise `console -- composer …` would see different private repositories than `console` followed by `composer …`. `console help -- <cmd>` is rejected with an explanation, since `help` lists the services rather than naming one to run a command in.
- Flags meant for docker-control are now recognised only where they can actually belong to it. `--version`/`-V` counts as the leading token only, and every other raw-argv scan (`--debug`, `--dir`, `--stop-ssh-agent`, the dependency-check and plugin-metadata probes) stops at the first standalone `--`. Previously they matched anywhere in argv, which swallowed arguments belonging to something else: `console -- php --version` printed docker-control's version instead of PHP's, and `module link <m> --version <v>` — the form the manual documents — printed the version and never linked. One behaviour change falls out of this: `docker-control <subcommand> --version` is now a parse error rather than a version print, since `--version` is a top-level flag.
- Composer calls made through `docker compose exec` now pass `COMPOSER_HOME=/var/www/.composer` explicitly. `COMPOSER_HOME` is set in `/var/www/.bashrc`, which only interactive shells read, so it applied to `console` but not to a non-interactive `exec`; what it fell back to varied by image tag, and only `/var/www/.composer` has the project's private-repository config mounted into it.

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