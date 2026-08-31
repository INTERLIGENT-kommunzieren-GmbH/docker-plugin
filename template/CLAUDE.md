# CLAUDE.md

Guidance for Claude Code (claude.ai/code) when working in this project.

> **Start here:** this file covers *infrastructure only* and is overwritten by
> `dc2 update`. Project-specific instructions and persistent memory live in
> **`htdocs/CLAUDE.md`** — read it before touching application code.

## What this is

A **Dockerized PHP / LAMP project managed by `docker-control`**. The whole container
stack (Apache+mod_php, MariaDB, phpMyAdmin, Gotenberg, Valkey/Redis, Mailpit, logrotate)
is described in `compose.yml`, but you should **operate it through the `docker-control`
CLI, not raw `docker` / `docker compose`** — the CLI wires up SSH agent forwarding,
ACL/permission fixes, ingress, secrets and env handling for you.

The CLI can be invoked three equivalent ways:

- `docker-control <command>` — the standalone binary
- `docker control <command>` — the Docker CLI plugin form
- `dc2 <command>` — a common shorthand alias

Below, `dc2` is used as shorthand for any of the three.

## Where the application lives

This directory is the **infrastructure / operations** layer. The actual PHP application
is in **`htdocs/`, which is a separate git repository** (gitignored here) mounted into
the `php` container at `/var/www/html`.

### `htdocs/CLAUDE.md` — application instructions and memory

**`htdocs/CLAUDE.md` is the authoritative source for project-specific instructions and
persistent memory.** Read it at the start of any task that touches the application.

- **Read it first.** Before working on application code, read `htdocs/CLAUDE.md` for the
  framework, domain model, build/test commands, coding conventions and any
  project-specific rules. Claude Code does not always load nested `CLAUDE.md` files
  automatically, so open it explicitly. Also check `htdocs/.claude/` (skills, agents,
  settings, commands) if present.
- **Write there, not here.** Anything worth remembering about *this* project —
  conventions, gotchas, decisions, user preferences, notes for future sessions — belongs
  in `htdocs/CLAUDE.md` (or `htdocs/.claude/`), which lives in the `htdocs/` repo and is
  versioned with the application.
- **This file is template-owned and disposable.** `dc2 update` and `dc2 migrate` both
  rsync the template over the project root and **overwrite this `CLAUDE.md`**, so notes
  added here are lost. (`htdocs/`, `.claude/` and `.idea/` are preserved across both.)
- **If `htdocs/CLAUDE.md` is missing,** gather application knowledge from the `htdocs/`
  source itself, and create `htdocs/CLAUDE.md` when you have durable knowledge to record.

This file (the project root `CLAUDE.md`) only covers infrastructure and the
`docker-control` workflow.

## Project layout

| Path | Purpose |
|---|---|
| `compose.yml` | The service stack. Template-owned; don't edit unless explicitly asked. |
| `htdocs/` | **The application — a separate git repo (gitignored here).** Mounted at `/var/www/html`. |
| `htdocs/CLAUDE.md` | **Project-specific instructions and memory** — read this for anything app-related. |
| `htdocs/.docker-control/` | Per-project config: `.deploy.json`, `control-scripts/`, `deployment-scripts/<env>.rhai`. |
| `config/` | Container config: `apache-sites/`, `php.ini`, `mariadb.cnf`, `composer.config.json`, `crontab`, `htpasswd`, `ssmtp.conf`. |
| `secrets/` | DB credential files (`db_name.txt`, `db_user.txt`, `db_pw.txt`, `db_root_pw.txt`). |
| `volumes/` | Persistent data: `db/`, `valkey/`, `composer-cache/`, `db-share/`. |
| `logs/` | Container logs: `apache/`, `mariadb/`. |
| `releases/`, `deployments/` | Release/deploy worktrees and deployment hook scripts. |
| `control-scripts/` | Project-local custom subcommands (see below). |
| `.env` | Project settings (from `.env-dist`): `PROJECTNAME`, `BASE_DOMAIN`, `PHP_VERSION`, `DB_HOST_PORT`, `ENVIRONMENT`, `XDEBUG_IP`, `IDE_KEY`. |
| `.managed-by-docker-control` | Sentinel marking this directory as managed by the CLI. |

## Everyday commands

**Lifecycle**

```bash
dc2 start            # start the project containers (detached)
dc2 stop             # stop the project containers
dc2 restart          # stop + start
dc2 status           # show container status
dc2 build [args]     # build containers (accepts docker-compose build args, e.g. --no-cache)
dc2 pull             # pull the latest images for the project
```

**Shell / running commands inside the stack**

```bash
dc2 console          # bash in the php container as www-data (cwd /var/www/html)
dc2 console db       # bash in the db container
dc2 console -- <cmd> # run <cmd> in the php container instead of opening a shell
```

Run `composer`, `php`, `bin/console`, `artisan`, etc. **inside** the php container via
`dc2 console`, since the app is mounted at `/var/www/html`. MariaDB is reachable from the
host at `127.0.0.1:${DB_HOST_PORT}` (see `.env`).

Anything after `--` runs one-shot in the container — same user, same workdir — and exits with
that command's status code, which is the form to use non-interactively:

```bash
dc2 console -- composer install
dc2 console -- php bin/console cache:clear
dc2 console db -- mysql -e 'show databases'
dc2 console -- bash -lc 'ls -1 var/log/*.log | wc -l'   # pipes/globs need an explicit shell
```

The `--` is mandatory (without it the first word is read as a service name), and the command is
passed as arguments rather than through a shell, so redirects and pipes belong either on the host
side of the call or inside an explicit `bash -lc`.

**Developing a vendor module**

```bash
dc2 module list                  # which vendor modules exist, and which are linked
dc2 module create acme/widget    # scaffold a NEW module and link it (composer init runs interactively)
dc2 module link acme/widget      # move an existing one to htdocs/modules/ and symlink it into vendor/
dc2 module unlink acme/widget    # restore the normal vendor install
```

Vendor modules are source installs, so `htdocs/vendor/<vendor>/<name>/` is a real git clone —
but edits there are discarded by the next `composer install`. `dc2 module link` moves the clone
to `htdocs/modules/<vendor>/<name>/` and adds a Composer `path` repository that symlinks it back
into `vendor/`, so edits survive. The module's git history stays usable, including via
`dc2 release` / `dc2 merge`.

⚠️ `module link` edits `composer.json` and `composer.lock`, which are **tracked** files. Do not
commit the link: `dc2 deploy` and `dc2 release` build from the committed tree, where `modules/`
does not exist. Run `dc2 module unlink` before committing.

`dc2 module create` is for a module that does not exist yet: it scaffolds `src/`, runs `git init` on
`main`, runs `composer init` interactively, adds a PSR-4 mapping, and wires the same path repository
`link` uses — pinned `dev-main`. Unlike `link` it also adds the app's `require`, because nothing
requires a brand-new module. So the path repository still must not be committed, but the `require`
does belong in a commit *once the module is pushed* somewhere the app's other repositories can
reach. Until then `dc2 module unlink` will refuse it, since removing the path repository would
leave the app requiring a package nothing can supply.

**Ingress** (shared reverse proxy — only when needed)

```bash
dc2 start-ingress    # dc2 stop-ingress / status-ingress / pull-ingress
dc2 trust-ca         # trust the ingress CA cert on this host (HTTPS without warnings)
```

`dc2 trust-ca` installs the ingress proxy's self-signed CA certificate into the host trust
store (system store on Linux/Windows, Keychain on macOS, plus the NSS store for
Firefox/Chromium) so the project's `https://` dev URLs are trusted with no browser
warnings. Run `dc2 start-ingress` first — the CA is generated by the proxy companion — then
run `dc2 trust-ca` once per host (it may prompt for `sudo`/admin rights).

**Permissions**

```bash
dc2 setacl           # re-apply host/container ACLs on htdocs when file perms drift
```

**Release & deploy** (interactive; the machinery is involved — prefer running these
directly and let them prompt, rather than scripting around them)

```bash
dc2 add-deploy-config   # add deployment config for an environment
dc2 release             # create a new release branch (auto-versioning + composer.lock)
dc2 merge               # cherry-pick a release branch back to main (skips release: commits)
dc2 deploy <env>        # deploy a selected release/tag to <env> (e.g. production, staging)
```

**Housekeeping**

```bash
dc2 update            # re-sync this project with the current template (backs up first)
dc2 cleanup-backups   # remove old backup_* folders left by update/migrate
dc2 show-running      # list all running docker-control projects
dc2 help              # full help + git/deploy/container status for this project
```

**Custom commands** — shell scripts in `control-scripts/` or
`htdocs/.docker-control/control-scripts/` are dispatched as `dc2 <script-name>`. Create
one with `dc2 create-control-script <name>`.

## Guardrails

- **Prefer `dc2`/`docker-control` commands over raw `docker` / `docker compose`.**
- **Don't edit** `compose.yml`, `secrets/`, or `config/` (template-owned) unless explicitly
  asked. `dc2 update` overwrites root-level template files — **including this
  `CLAUDE.md`** — so project-specific instructions and memory belong in
  `htdocs/CLAUDE.md`, not here (see above).
- **Application code changes go in `htdocs/`** (its own repo and commits); infrastructure
  changes go in this project-root repo. Keep the two straight.
- The `php` image tag is `fduarte42/docker-php:${PHP_VERSION}` (`PHP_VERSION` from `.env`).
- Dev URLs use `BASE_DOMAIN` (e.g. `*.lvh.me`, which resolves to `127.0.0.1`).
  phpMyAdmin is served at `/_phpmyadmin/` and Mailpit at `/_mail/`.
