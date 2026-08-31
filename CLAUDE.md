# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                        # debug build
cargo build --release              # release build
cargo run -- <args>                # run (e.g. cargo run -- init)
cargo nextest run                  # run all tests (must use nextest, not cargo test)
cargo nextest run <test_substring> # run a single test by substring match
cargo clippy                       # lint
cargo fmt                          # format
cargo fix --allow-dirty            # auto-fix lint warnings
```

`#[allow(clippy::collapsible_if)]` is intentionally set at crate level.

## Architecture

This is a Rust 2024-edition CLI tool (`docker-control`) that manages Docker-based PHP projects. It acts as both a standalone binary and a Docker CLI plugin (`docker control`).

### Startup flow (`src/main.rs`)

SSH agent lifecycle (start/stop/restart) is handled before the async runtime starts — those flags are intercepted from raw `args` before clap parsing. For all other commands, a Tokio runtime is created and `async_main` runs. On startup it:
1. Checks external tool dependencies (`utils::dependencies`)
2. Detects platform (`utils::platform`)
3. Auto-starts the SSH agent daemon if not already running on port 2222
4. Initialises embedded assets (`assets::AssetManager`)

Several of those pre-clap steps scan raw `args` for docker-control's own flags. They must scan `commands::custom::args_before_separator(&args)`, never the full argv: tokens after a standalone `--` belong to the command `console -- <cmd>` runs in the container (or to a custom script), so a file-wide scan silently steals them — `console -- php --version` printed docker-control's version, `console -- foo --stop-ssh-agent` stopped the agent. `--version`/`-V` is narrower still: it counts only as the *leading* token (via `split_leading_subcommand`, as `is_help` does), because anywhere later it is a subcommand's own argument — `module link <m> --version <v>`. The full argv stays available for the custom-script dispatch, which has to forward everything the user typed.

### Module responsibilities

| Module | Purpose |
|---|---|
| `src/commands/` | One file per subcommand; each exposes an `execute()` function |
| `src/docker/mod.rs` | Wraps `docker compose` via `std::process::Command`; `bollard` is used for container introspection only |
| `src/git/mod.rs` | `GitService` wraps `git2`; handles branches, tags, worktrees, cherry-pick, push |
| `src/ssh/mod.rs` | `exec_ssh` / `copy_ssh` helpers used by deploy |
| `src/config/mod.rs` | Loads/saves `.deploy.json`; config file search order: `htdocs/.docker-control/.deploy.json` → `.deploy.json` |
| `src/assets/mod.rs` | `template/` and `ingress/` directories are compiled into the binary via `include_dir!` and extracted to the OS config dir on first run (or when version changes) |
| `src/template/mod.rs` | Tracks which template a project is synced to (`.docker-control/state.json`); three-way classification of template changes |
| `src/ui/mod.rs` | Terminal output helpers: `info`, `warning`, `critical`, `success`, `debug` |
| `src/utils/` | Platform detection, SSH agent forwarding, dependency checks, `is_managed()` |

### Key domain concepts

**Managed projects** — Most commands require a `.managed-by-docker-control` (or `.managed-by-docker-control-plugin`) sentinel file in the project directory. `utils::is_managed()` checks this.

**Project layout** — The tool expects web app source at `htdocs/` (a separate git repo), vendor modules at `htdocs/vendor/<name>/`, and config at `htdocs/.docker-control/`. Note the two same-named directories: `htdocs/.docker-control/` is app-level config (deploy config, control/deployment scripts) in the app repo, while `<project>/.docker-control/` holds docker-control's own state for the wrapper project.

**Template state** (`template/mod.rs`) — `init`/`update`/`migrate` record the *template's* file hashes in `<project>/.docker-control/state.json`. That is a merge base, so `base` (recorded) vs `theirs` (template now) vs `mine` (project now) classifies each file exactly: unchanged upstream → silent regardless of local edits; changed upstream only → safe to apply; changed on both sides → conflict. Version numbers are deliberately *not* the trigger — the template changes in roughly one release in five, so a version bump says nothing about whether anything needs applying. A `template_fingerprint` over the whole manifest is the fast path. `.env-dist`/`.gitignore-dist` are excluded from hashing (their project copy is renamed/consumed, so a missing copy is indistinguishable from a missing base) and checked by content against `.env`/`.gitignore` instead; `secrets/*.txt` and `config/htpasswd` are seeded once and never compared.

**Development modules** (`commands/module.rs`) — `module create` scaffolds a module that does not exist yet (`src/` + PSR-4, `git init` on `main`, `composer init` run *interactively* via `docker::exec_interactive`, initial commit) and then wires it exactly as `link` does, pinned `dev-main`. The one asymmetry: `create` also runs `composer require`, which `link` deliberately never does — nothing requires a brand-new module, so `vendor/<vendor>/<name>` would never appear without it. Consequences that are easy to get wrong: the path repository still must not be committed while the `require` eventually must, so `create`'s rollback restores the *application* but keeps the checkout (the opposite of `rollback_link`, because those files are the developer's work and Composer cannot reproduce them); and `unlink` refuses a checkout with no git remote, since its closing `composer update` would have nothing to resolve the package from once the path entry is gone. `create` validates the package name before creating anything, and writes the module's *own* `composer.json` with `serde_json` — allowed only because that file was generated seconds earlier by us, unlike the application's.

`module link` moves a source-installed module from `htdocs/vendor/<vendor>/<name>` to `htdocs/modules/<vendor>/<name>` and wires a Composer `path` repository (`symlink: true`, version pinned via `options.versions` to whatever `composer.lock` already records) so `vendor/` becomes a symlink to the checkout. The repo entry *is* the state — there is no state file — and `require` is never edited. Three non-obvious constraints: the entry must be **prepended** (appended, it loses priority to a private `composer` repo in the same file and Composer exits 0 having re-cloned upstream, so `link` asserts the symlink afterwards); `composer config` does the `composer.json` writes, because `serde_json::to_string_pretty` would reflow the whole file; and `unlink` deletes the `vendor/` symlink *before* calling Composer, which otherwise follows it into a dirty worktree and aborts after already rewriting the lock. Modules live under `htdocs/` because `./htdocs:/var/www/html` is the only application mount.

⚠️ **`COMPOSER_HOME` under `docker compose exec`** — `/var/www/.bashrc` sets `COMPOSER_HOME`, and `.bashrc` is only read by *interactive* shells, so it applies to `console` but not to a non-interactive `exec`. What the variable falls back to is image-tag dependent (unset on `8.2-debug`, so it resolves to `$HOME/.composer`; `/composer` on `8.5-slim-debug`), and only `/var/www/.composer` has `config/composer.config.json` with the private repositories mounted into it. Any `exec`-based Composer call must therefore pass `-e COMPOSER_HOME=/var/www/.composer` — see `docker::exec_as_user`, and `docker::console_exec_flags` for the same reason applied to `console -- <cmd>`.

**`console` vs `console_exec`** (`docker/mod.rs`) — `console` opens an interactive `bash`; `console -- <cmd>` goes through `console_exec` instead. The split is deliberate: the one-shot path has to re-state as flags what the interactive shell gets from the image and `/var/www/.bashrc` (`-u www-data`, `-w /var/www/html`, `COMPOSER_HOME`), and it returns the container command's exit code for `main` to `process::exit` with, rather than an `anyhow::Result` that would print an `Error:` line over a failure the inner command already reported. The `--` separator is enforced by clap's `last = true`, not by hand-parsing argv, so `console ls -la` is a parse error rather than a guess about whether `ls` is a service.

**Release workflow** (`commands/release.rs`) — Uses git worktrees under `releases/` to keep release preparation isolated. Increments semver from existing branches/tags, updates `composer.json`, runs `composer install` inside a Docker container (`fduarte42/docker-php:<PHP_VERSION>`), commits `composer.lock`, then pushes the branch. Patch releases create a tag; major/minor create a new `X.Y.x` branch.

**Merge workflow** (`commands/merge.rs`) — Creates a `<release_branch>-merge` branch in a worktree under `releases/`, then cherry-picks commits from the release branch to primary, skipping commits with a `release:` prefix. Handles interactive conflict resolution and pushes the merge branch for a PR.

**Deploy workflow** (`commands/deploy.rs`) — Checks out the selected tag via `git2` into a temp dir, runs `composer install` in Docker, compresses with `7z`, uploads via `scp`, extracts on the server, handles shared directories/files via symlinks, runs Doctrine migrations, flips the `current` symlink, and manages maintenance mode. Supports Rhai hook scripts (`pre_deploy`, `post_deploy`, `done_deploy`) loaded from `htdocs/.docker-control/deployment-scripts/<env>.rhai`.

**SSH agent daemon** — Runs as a separate daemonized process on port 2222 (`SSH_AGENT_PORT`), forwarding the host SSH agent into Docker containers. Automatically started when missing. Controlled via `--start-ssh-agent` / `--stop-ssh-agent` / `--restart-ssh-agent` flags.

**Ingress** — A separate Docker compose stack for the reverse proxy. Located relative to the binary (`../share/docker-control/ingress/`), via `DOCKER_CONTROL_INGRESS_DIR`, or the embedded assets.

**Custom commands** — Shell scripts in `control-scripts/` or `htdocs/.docker-control/control-scripts/` are dispatched as external subcommands.

### Testing patterns

Interactive prompt dependencies (`inquire`) are abstracted behind trait objects (`PromptProvider`, `MergePromptProvider`) so tests can inject a mock implementation. SSH calls in `deploy.rs` are mocked via a `thread_local! MOCK_SSH_COMMANDS` that is swapped in during `#[cfg(test)]`. Integration tests live in `tests/` and use `tempfile` for filesystem isolation.

### Environment variables respected at runtime

| Variable | Effect |
|---|---|
| `DOCKER_CONTROL_SKIP_DEPENDENCY_CHECK` | Skip external tool checks |
| `DOCKER_CONTROL_SKIP_SSH_AGENT` | Skip SSH agent management |
| `DOCKER_CONTROL_INGRESS_DIR` | Override ingress directory path |
| `SSH_AUTH_PORT` | Set automatically to `<bind_ip>:2222`; read by deploy/release Docker commands |
| `PHP_VERSION` | Read from project `.env`; used for the `fduarte42/docker-php` image tag |