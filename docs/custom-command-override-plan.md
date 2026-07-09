# Custom-script / built-in command clash resolution + `_override_` convention

## Context

`docker-control` dispatches unrecognized subcommands to custom shell scripts in
`control-scripts/` or `htdocs/.docker-control/control-scripts/` via clap's
`external_subcommand` catch-all (`Commands::External` in `src/main.rs`). Because that
catch-all only fires when clap doesn't recognize the name as a built-in, a custom script
whose name collides with a built-in command (e.g. a script named `build.sh` vs. the
built-in `build` command) is **silently unreachable today** — the built-in always wins,
with no warning and no way to run the script.

This plan adds:
1. Interactive clash detection: if a typed command name matches both a built-in and a
   custom script, ask the user which one to run.
2. An `_override_` convention (mirroring the existing `_desc_` convention used for
   custom-command descriptions) so a script author can make their script win
   automatically, without prompting.

Once approved, this plan file will also be copied to `docs/custom-command-override-plan.md`
to match this repo's convention of keeping feature plans under `docs/`.

## Design

### 1. `src/commands/custom.rs` — new shared, testable logic

- `pub fn find_script_path(project_dir: &Path, name: &str) -> Option<PathBuf>` — the
  4-candidate-path search (htdocs/root, with/without `.sh`) currently inlined in
  `execute_external_script` (`src/main.rs:603-653`), extracted so it's reusable and
  unit-testable. `execute_external_script` is refactored to call this instead of
  duplicating the loop (behavior-preserving cleanup).
- `pub fn run_script(project_dir: &Path, script_path: &Path, args: &[String]) -> anyhow::Result<()>`
  — the `Command::new("bash")...status()` spawn logic extracted from
  `execute_external_script`, reused by both normal dispatch and clash resolution.
- `pub fn get_override(path: &Path) -> bool` — mirrors `get_description` (custom.rs:48-55):
  runs `bash <script> _override_`; returns `true` only if the process exits successfully
  **and** trimmed stdout equals `"true"`. Any other outcome (nonzero exit, no such block,
  garbage output) is `false` — same safe-default shape as `_desc_`'s fallback.
- `pub enum ClashChoice { Builtin, Custom }`
- `pub trait ClashPromptProvider { fn resolve(&self, command_name: &str) -> anyhow::Result<ClashChoice>; }`
  — mirrors the existing `PromptProvider` trait in `src/commands/release.rs:10-19`, for
  mock injection in tests.
- `pub struct InteractiveClashPromptProvider;` implementing it via `inquire::Select` with
  `"Built-in command"` / `"Custom script"` options. On any prompt failure (Esc, Ctrl-C,
  non-interactive stdin/CI) fall back to `Builtin` — preserves today's behavior rather
  than erroring.
- `pub fn resolve_clash(project_dir: &Path, command_name: &str, prompt_provider: &dyn ClashPromptProvider) -> anyhow::Result<Option<PathBuf>>`
  — `None` if no script named `command_name` exists (no clash); if one exists, checks
  `get_override` first (silently returns `Some(path)`, no prompt), otherwise delegates to
  `prompt_provider.resolve(...)` and returns `Some(path)` or `None` accordingly.
- `pub fn split_trailing_args(args: &[String], subcommand_name: &str) -> Vec<String>` —
  pure helper (no clap types) used by `main.rs` to recover "everything the user typed
  after the subcommand name" from the raw argv, needed because a matched built-in's
  `ArgMatches` doesn't generically expose that. Walks forward from `args[1]`, skipping
  the small fixed set of global flags (`--dir`/`-d` + its value; `--debug`,
  `--start-ssh-agent`, `--stop-ssh-agent`, `--restart-ssh-agent`), then requires the next
  token to equal `subcommand_name` — if it doesn't (parsing quirk / new flag added later
  without updating this list), fall back to an empty vec rather than guessing wrong args.

### 2. `src/main.rs` — wire in the clash check

Verified: `ArgMatches::subcommand_name()` (available via `matches`, still in scope after
`Cli::from_arg_matches(&matches)` at line 398) reliably returns the exact typed
subcommand token in both the matched-builtin case and the external/unmatched case — this
is the same string that ends up as `args[0]` in `Commands::External(Vec<String>)`.

Insert between the existing `let command = match cli.command {...}` (line 432, handles
the "no command" case) and the big dispatch `match command { ... }`:

```rust
let command = if let Commands::External(_) = &command {
    command // already unambiguous, no custom-script shadowing possible
} else if let Some(name) = matches.subcommand_name() {
    match commands::custom::resolve_clash(
        &project_dir,
        name,
        &commands::custom::InteractiveClashPromptProvider,
    )? {
        Some(script_path) => {
            let trailing_args = commands::custom::split_trailing_args(&args, name);
            commands::custom::run_script(&project_dir, &script_path, &trailing_args)?;
            return Ok(());
        }
        None => command,
    }
} else {
    command
};
```

`execute_external_script` (main.rs:603-653) is refactored to call
`commands::custom::find_script_path` + `commands::custom::run_script` instead of its
inlined duplicate logic.

### 3. `src/commands/create_script.rs` — generator parity with `_desc_`

After the existing `Text::new("Description of the command:")` prompt, add:

```rust
let should_override = Confirm::new(
    "Should this command override a built-in command of the same name, if one exists?",
)
.with_default(false)
.prompt()?;
```

If `true`, emit an extra block right after the existing `_desc_` block in the generated
template:

```bash
if [[ "$1" == "_override_" ]]; then
    echo "true"
    exit 0
fi
```

(`inquire::Confirm` needs adding to the `use inquire::{...}` import.)

`migrate.rs`'s generated `cap.sh` template is left as-is — migrated scripts shouldn't
silently start overriding built-ins by default.

### 4. `README.md` — document the new behavior

Add a short section after the existing `_desc_` example (around line 241), explaining:
the interactive prompt on name clash, that Enter/Escape keeps the built-in (today's
default), the `_override_` block to skip the prompt, and that
`create-control-script` can generate it.

## Tests

New `tests/custom_tests.rs` using `tempfile::TempDir` (matching this repo's established
pattern, e.g. `tests/release_tests.rs`, `tests/common/mod.rs`):

- `find_script_path`: finds in root, finds in htdocs, htdocs takes precedence over root,
  appends `.sh` when needed, returns `None` when missing.
- `get_override`: `true` when script echoes `"true"` for `_override_`; `false` when the
  block is absent; `false` when the script exits nonzero.
- `resolve_clash`: `None` when no script exists; `Some(path)` without invoking the prompt
  when `_override_` is true (assert via a mock provider that panics if called); prompt
  invoked and respected for both `Builtin` and `Custom` choices (mock
  `ClashPromptProvider` returning a fixed `ClashChoice`).
- `split_trailing_args`: plain case, `--dir <value>` before the name, `--debug` before
  the name, mismatch/fallback-to-empty case.

`main.rs`'s wiring itself is not unit-tested (same accepted gap as `execute_external_script`
and `check_managed` today — main.rs is a binary crate); it's a thin call-through of the
now-tested `resolve_clash` / `split_trailing_args` / `run_script`.

## Verification

1. `cargo build` and `cargo nextest run` (new `custom_tests.rs` plus full suite, no
   regressions).
2. Manual check in a scratch managed project:
   - Create `control-scripts/build.sh` (clashing with the built-in `build` command,
     containing just an echo + the `_desc_` block). Run `cargo run -- build` and confirm
     the interactive prompt appears, and that choosing each option runs the expected
     thing.
   - Add the `_override_` block (echo `"true"`) to that script and confirm
     `cargo run -- build` now runs the script directly with no prompt.
   - Remove the clashing script and confirm `cargo run -- build` behaves exactly as
     before (no prompt, no behavior change).
   - Run `docker-control create-control-script`, answer "yes" to the new override
     prompt, and confirm the generated script contains the `_override_` block.
3. `cargo clippy` clean.
