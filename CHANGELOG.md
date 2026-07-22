# Changelog

All notable changes since 2.3.0 are documented here.

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