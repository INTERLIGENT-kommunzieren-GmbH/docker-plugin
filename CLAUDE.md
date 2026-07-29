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