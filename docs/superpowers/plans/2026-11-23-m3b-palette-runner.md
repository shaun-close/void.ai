# void.ai Milestone 3b (Palette, Moon Runner, Environments) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Shift+Shift palette (Search Everywhere) and a moon-powered runner with grouped run configs, task chains and switchable environment profiles.

**Architecture:** A `runner` module in void-core does the work: moon discovery, run configs and env profiles with per-workspace storage, and running chains through the engine with events like tool output. The GUI adds a Runs panel, run-output dock tabs, an env pill in the status bar, and the palette as a popup over the dock.

**Tech Stack:** M3a's stack, plus `nucleo-matcher` for fuzzy ranking and `vte` for parsing ANSI colours in run output.

**Spec:** `docs/superpowers/specs/2026-11-23-m3-editor-design.md` (goals 5–7). **Depends on:** M3a (editor tabs open at file:line).

## Global Constraints

- **Runs are the user's:** they are never sandboxed, and env-profile values never reach the agent sandbox, logs or conversation files.
- **Moon on PATH:** moon is the one the workspace resolves through `~/.proto/shims` on PATH. `moon query tasks` prints JSON; there is no `--json` flag in moon 2.5.0.
- **Storage:** `~/.local/share/void.ai/workspaces/<id>/runs.toml` holds groups and configs; `env.toml` holds profiles and the active one.
- **Env profile values:** `value = "…"`, `env = "VAR"`, or `cmd = "…"` (stdout trimmed, 10 s timeout). They're resolved at run start and kept only in memory.
- **Global keys:**
  - Shift+Shift (two Shift presses with no other key between them, within 400 ms) and Ctrl+Shift+P open the palette.
  - Ctrl+Shift+F10 re-runs the last run.
  - Esc closes the palette.
- **Commits:** commit after each task, with a green gate.

## Review Focus

1. **A chain step fails:** with stop-on-failure the chain halts and reports which step failed; with continue, the next step runs. → `chain_stops_on_failure`, `chain_continues_when_set` (T2).
2. **An env command fails or hangs:** the run doesn't start, the error names the profile and variable, and the value is never printed. → `env_cmd_failure_blocks_run_without_leaking` (T1/T2).
3. **Stop during a chain:** the process group dies and later steps don't start. → `stop_kills_chain` (T2).
4. **Odd output:** ANSI codes, `\r` progress bars and very long lines don't break the output panel, and locations still parse. → `parse_locations_from_rustc_tsc_generic`, `ansi_spans_and_cr_handling` (T2/T3).
5. **Palette with thousands of files:** stays responsive (ranking is capped and incremental), and Shift+Shift doesn't fire on Shift+letter typing. → `double_shift_detector`, `palette_ranks_10k_files_fast` (T4).

---

### Task 1: Runner model, discovery and storage (void-core)

**Files:** `crates/void-core/src/runner/{mod.rs,moon.rs,config.rs,env.rs}`

**Interfaces (produces):**
- `pub struct MoonTask { project: String, id: String, target: String /*project:id*/, command: String, description: Option<String> }`
- `pub fn parse_moon_tasks(json: &str) -> Result<Vec<MoonTask>, String>`, and `pub async fn discover(root: &Path) -> Result<Vec<MoonTask>, String>`, which runs `moon query tasks` in `root`.
- `pub enum Step { Moon { target: String, args: Vec<String> }, Shell { command: String } }`
- `pub struct StepConfig { step: Step, on_failure: OnFailure /*Stop|Continue*/ }`
- `pub struct RunConfig { id: String, name: String, steps: Vec<StepConfig>, env_profile: Option<String> /*None = active*/ }`
- `pub struct Group { name: String, configs: Vec<RunConfig> }`
- `pub struct Runs { groups: Vec<Group> }`, with `load(path)` and `save(path)` (TOML; a corrupt file is set aside with a warning).
- `pub enum EnvValue { Value(String), Env(String), Cmd(String) }`
- `pub struct EnvProfile { name: String, vars: BTreeMap<String, EnvValue> }`
- `pub struct Envs { active: Option<String>, profiles: Vec<EnvProfile> }`, with `load` and `save`, and `resolve(&self, name) -> Result<Vec<(String, String)>, String>`. The error names the profile and the variable but never includes a value.

- [ ] **Step 1: Write failing tests**
  - `parse_moon_tasks_from_real_output`: a fixture captured from void.ai's own `moon query tasks`.
  - `runs_toml_round_trip`
  - `corrupt_runs_set_aside`
  - `env_resolution_literal_env_cmd`
  - `env_cmd_failure_blocks_run_without_leaking`: a failing `cmd` gives an error naming the variable, without stdout or stderr content that could be a secret. Cap stderr at 200 chars and redact any line containing the variable's value.
- [ ] **Steps 2–5:** run the tests and watch them fail, implement, run the tests and the gate, then commit `feat(core): moon discovery, run configs and env profiles`.

---

### Task 2: Running chains in the engine

**Files:** `crates/void-core/src/runner/run.rs`, `engine.rs`.

**Interfaces:**
- **Commands:**
  - `RunConfig { config_id }` and `RunTask { target, args }` (an ad-hoc task with the active env)
  - `StopRun(RunId)` and `RerunLast`
  - `RefreshTasks`
  - `SaveRuns(Runs)`, `SaveEnvs(Envs)`, `SetActiveEnv(Option<String>)`
- **Events:**
  - `Tasks(Vec<MoonTask>)`, `RunsLoaded { runs, envs }`
  - `RunStarted { run: RunId, name, steps: usize }`
  - `RunStep { run, index, label }`
  - `RunOutput { run, text }`
  - `RunFinished { run, ok: bool, failed_step: Option<usize>, duration_ms }`
- `pub fn parse_locations(line: &str, root: &Path) -> Vec<Location { path: PathBuf, line: usize, col: Option<usize>, span: Range<usize> }>` handles:
  - rustc's `--> src/x.rs:3:5`;
  - tsc's `src/x.ts(3,5)`;
  - generic `path:line:col` and `path:line`.

  Only paths that exist under the root count.

**Behaviour:**
- **Steps:** each step runs as `moon run <target> -- <args>` or `bash --noprofile --norc -c <command>`, with:
  - `cwd` = the workspace root;
  - the environment = the process environment plus the resolved profile;
  - its own process group, with stdout and stderr streamed.
- **Chains:** a step exiting non-zero stops the chain or continues, per its setting.
- **Stopping:** Stop kills the group and marks the run as stopped.
- **Re-run last:** re-runs the last config or task.
- **Concurrency:** several runs at once.
- **Moon watching:** void.ai watches `moon.yml`/`.moon/` and refreshes tasks.

- [ ] **Step 1: Write failing tests** (shell steps; moon steps are covered by one live test that is ignored by default):
  - `chain_runs_in_order`
  - `chain_stops_on_failure`
  - `chain_continues_when_set`
  - `stop_kills_chain`
  - `env_applied_to_steps` (a step echoes `$FOO`)
  - `rerun_last`
  - `parse_locations_from_rustc_tsc_generic`
  - `live_moon_run_format_check` (ignored): run `workspace:format` in void.ai and expect exit 0.
- [ ] **Steps 2–5:** run the tests and watch them fail, implement, run the tests and the gate, then commit `feat(core): run chains with env profiles`.

---

### Task 3: Runs panel, run output tabs, env pill

**Files:** `apps/void-gui/src/panels/{runs.rs,run_output.rs}`, `layout.rs` (`Tab::Runs` and `Tab::RunOutput { run: RunId }`), `state.rs`, `app.rs`.

**Behaviour:**
- **Runs panel:**
  - **Tasks:** moon tasks grouped by project, collapsible. Double-click runs one; the context menu offers "Run" and "Add to run config…".
  - **My run configs:** in their groups. You can create, rename and delete groups and configs, and drag configs between groups.
  - **Config editor:** name, environment profile, and steps (moon target picked from the discovered tasks or free text, args, or a shell command), each with a stop/continue toggle.
- **Environments section:** profiles with editable vars. The value kind is value, env or command, and literal values are masked with a reveal toggle. One profile is set active.
- **Run output tab:**
  - one per run: header with name, status, current step, duration, Stop and Re-run;
  - output rendered with ANSI colours (`vte`) on the void palette;
  - `\r` overwrites the line (progress bars);
  - clickable locations open an editor tab at that line (M3a);
  - auto-scroll unless scrolled up.
- **Status bar:** an env pill with the active profile name ("no env" when none). Clicking it opens a menu to switch.
- [ ] **Step 1: Write failing tests:**
  - `ansi_spans_and_cr_handling` (pure)
  - `runs_tree_moves_config_between_groups` (pure state op)
  - kittest `run_output_location_opens_editor` (a scripted run's output contains `src/a.rs:2:3`; clicking it opens an editor tab at line 2)
  - `env_pill_switches_profile`
- [ ] **Steps 2–5:** run the tests and watch them fail, implement, run the tests and the gate, take screenshots, then commit `feat(gui): runs panel, run output and env switcher`.

---

### Task 4: Shift+Shift palette

**Files:** `apps/void-gui/src/palette/{mod.rs,sources.rs,shift.rs}`, `app.rs`.

**Interfaces:**
- `pub struct DoubleShift`, with `feed(&mut self, event, now) -> bool`: true on the second Shift *release* within 400 ms with no other key between.
- `pub enum Item { File(PathBuf), Tab(TabRef), Command(CommandId), Task(String), RunConfig(String), Env(String), Symbol { name, line } }`.
- `pub fn rank(query: &str, items: &[Item], limit: usize) -> Vec<(Item, score)>`, using `nucleo-matcher`.
- **Prefixes:** `>` commands, `@` symbols, `!` runs, `$` envs, `/` files only.

**Behaviour:**
- **The popup:** a centered panel with a search field and results grouped by kind, each with an icon and secondary text (path, project, kind). Up/Down move, Enter acts, Esc closes, and Ctrl+Enter opens a file in a new tab.
- **Sources:**
  - **files:** the walk of the workspace, cached and refreshed by the watcher;
  - **open tabs and recent files**, with recent files ranked first when the query is empty;
  - **commands:** every menu action, registered in a table;
  - **moon tasks and run configs**;
  - **env profiles**;
  - **symbols:** the current editor tab's `Syntax::outline`.
- **Actions:**
  - a file opens a tab, and a tab is focused;
  - a command runs;
  - a task or config runs;
  - an env profile is switched to;
  - a symbol jumps to its line.
- [ ] **Step 1: Write failing tests:**
  - `double_shift_detector` (Shift, Shift fires; Shift+A, Shift does not; slow presses don't)
  - `palette_prefixes_filter_kinds`
  - `palette_ranks_10k_files_fast` (< 50 ms for 10 000 paths in release; prints timing)
  - kittest `palette_opens_file` (Shift, Shift, type `model.rs`, Enter: an editor tab opens)
- [ ] **Steps 2–5:** run the tests and watch them fail, implement, run the tests and the gate, take a screenshot, then commit `feat(gui): Shift+Shift palette`.

---

### Task 5: Verification

- [ ] Full gate green, screenshots inspected (palette, runs panel, run output with a clickable error, env pill).
- [ ] Live ignored test `live_moon_run_format_check` passes.
- [ ] kittest e2e: build a run config "check" with steps `workspace:format` then `workspace:lint` and env profile "dev" (`RUST_LOG=debug`), run it from the palette (`!check`), and wait for success.
- [ ] Commit `test: palette and runner end-to-end checks`.
