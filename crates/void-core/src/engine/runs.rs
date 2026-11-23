//! The engine's side of the runner: the workspace's run configs and environment profiles,
//! moon task discovery, and runs (chains of steps) going on alongside chats, several at once.
//! Runs are the user's own: never sandboxed, and resolved environment values only ever reach
//! the steps' processes (never an event, a log or an error).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use super::{Command, Event, Msg, State};
use crate::runner::run::{ChainSpec, RunUpdate, Sink, run_chain};
use crate::runner::{
    Envs, MoonTask, OnFailure, RunId, RunOrigin, Runs, Step, StepConfig, discover,
};

#[derive(Default)]
pub(super) struct RunsState {
    runs: Runs,
    envs: Envs,
    /// Why `runs.toml` couldn't be read: it exists, so it's never saved over.
    runs_unreadable: Option<String>,
    /// The same for `env.toml`.
    envs_unreadable: Option<String>,
    active: HashMap<RunId, ActiveRun>,
    next_id: u64,
    /// What `RerunLast` repeats (in the current workspace).
    last: Option<RunOrigin>,
    /// Only the newest discovery's result counts.
    tasks_generation: u64,
    /// The workspace's moon tasks as last discovered (for moon steps' project folders).
    tasks: Vec<MoonTask>,
}

struct ActiveRun {
    cancel: CancellationToken,
    abort: AbortHandle,
    started: Instant,
}

impl State {
    pub(super) fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::RunConfig { config_id } => self.start_run(RunOrigin::Config { id: config_id }),
            Command::RunTask { target, args } => self.start_run(RunOrigin::Task { target, args }),
            Command::StopRun(run) => {
                if let Some(a) = self.runs.active.get(&run) {
                    a.cancel.cancel();
                }
            }
            Command::RerunLast => match self.runs.last.clone() {
                Some(req) => self.start_run(req),
                None => self.emit(Event::Info("Nothing to re-run yet.".into())),
            },
            Command::RefreshTasks => self.refresh_tasks(false),
            Command::SaveRuns(runs) => self.save_runs(runs),
            Command::SaveEnvs(envs) => self.save_envs(envs),
            Command::SetActiveEnv(active) => {
                if let Some(name) = &active
                    && !self.runs.envs.profiles.iter().any(|p| p.name == *name)
                {
                    self.emit(Event::Error(format!(
                        "There's no environment profile called '{name}'."
                    )));
                    return;
                }
                let envs = Envs {
                    active,
                    ..self.runs.envs.clone()
                };
                self.save_envs(envs);
            }
            _ => {}
        }
    }

    /// On opening a workspace: its run configs and profiles (`RunsLoaded`), then its moon
    /// tasks in the background.
    pub(super) fn load_runs(&mut self) {
        let (runs_path, env_path) = self.store.runner_paths(&self.workspace.id);
        let mut warnings = Vec::new();
        let mut errors = Vec::new();
        let st = &mut self.runs;
        st.last = None;
        st.tasks.clear();
        (st.runs, st.runs_unreadable) = match Runs::load(&runs_path) {
            Ok((runs, warning)) => {
                warnings.extend(warning);
                (runs, None)
            }
            Err(e) => {
                errors.push(e.clone());
                (Runs::default(), Some(e))
            }
        };
        (st.envs, st.envs_unreadable) = match Envs::load(&env_path) {
            Ok((envs, warning)) => {
                warnings.extend(warning);
                (envs, None)
            }
            Err(e) => {
                errors.push(e.clone());
                (Envs::default(), Some(e))
            }
        };
        for e in errors {
            self.emit(Event::Error(format!(
                "{e} — it won't be saved over until that's fixed."
            )));
        }
        self.emit_runs_loaded((!warnings.is_empty()).then(|| warnings.join("\n")));
        self.refresh_tasks(true);
    }

    fn emit_runs_loaded(&self, warning: Option<String>) {
        self.emit(Event::RunsLoaded {
            runs: self.runs.runs.clone(),
            envs: self.runs.envs.clone(),
            warning,
        });
    }

    fn save_runs(&mut self, runs: Runs) {
        if let Some(e) = &self.runs.runs_unreadable {
            self.emit(Event::Error(format!("Not saving run configs: {e}")));
            // What's kept, for a UI that showed the change already.
            self.emit_runs_loaded(None);
            return;
        }
        let (path, _) = self.store.runner_paths(&self.workspace.id);
        if let Err(e) = runs.save(&path) {
            self.emit(Event::Error(format!(
                "Couldn't save the run configs ({}): {e}",
                path.display()
            )));
        }
        self.runs.runs = runs;
        self.emit_runs_loaded(None);
    }

    fn save_envs(&mut self, envs: Envs) {
        if let Some(e) = &self.runs.envs_unreadable {
            self.emit(Event::Error(format!(
                "Not saving environment profiles: {e}"
            )));
            self.emit_runs_loaded(None);
            return;
        }
        let (_, path) = self.store.runner_paths(&self.workspace.id);
        if let Err(e) = envs.save(&path) {
            self.emit(Event::Error(format!(
                "Couldn't save the environment profiles ({}): {e}",
                path.display()
            )));
        }
        self.runs.envs = envs;
        self.emit_runs_loaded(None);
    }

    /// Ask moon for the tasks (only in a moon workspace: one with a `.moon/` folder; elsewhere
    /// there are none). `clear`: a new workspace, so drop the old list while moon answers (a
    /// failed refresh otherwise keeps it).
    fn refresh_tasks(&mut self, clear: bool) {
        self.runs.tasks_generation += 1;
        let generation = self.runs.tasks_generation;
        let Some(root) = self
            .workspace
            .root
            .clone()
            .filter(|r| r.join(".moon").is_dir())
        else {
            self.runs.tasks.clear();
            self.emit(Event::Tasks(Vec::new()));
            return;
        };
        if clear {
            self.emit(Event::Tasks(Vec::new()));
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = discover(&root).await;
            let _ = tx.send(Msg::Tasks { generation, result });
        });
    }

    pub(super) fn tasks_found(&mut self, generation: u64, result: Result<Vec<MoonTask>, String>) {
        if generation != self.runs.tasks_generation {
            return;
        }
        match result {
            Ok(tasks) => {
                self.runs.tasks = tasks.clone();
                self.emit(Event::Tasks(tasks));
            }
            Err(e) => self.emit(Event::Error(format!("Couldn't list moon tasks: {e}"))),
        }
    }

    fn start_run(&mut self, req: RunOrigin) {
        let Some(root) = self.workspace.root.clone() else {
            self.emit(Event::Error(
                "Runs need a workspace folder — open one first.".into(),
            ));
            return;
        };
        let (name, steps, profile) = match &req {
            RunOrigin::Config { id } => {
                let found = self
                    .runs
                    .runs
                    .groups
                    .iter()
                    .flat_map(|g| &g.configs)
                    .find(|c| c.id == *id);
                let Some(c) = found else {
                    self.emit(Event::Error(
                        "That run config doesn't exist any more.".into(),
                    ));
                    return;
                };
                // A pinned profile that's gone is refused here: never the active one instead.
                if let Some(p) = &c.env_profile
                    && !self.runs.envs.profiles.iter().any(|e| e.name == *p)
                {
                    let name = c.name.clone();
                    self.emit(Event::Error(format!(
                        "Run config '{name}' uses environment profile '{p}', which doesn't \
                         exist any more. Pick a profile in the config's editor."
                    )));
                    return;
                }
                (c.name.clone(), c.steps.clone(), c.env_profile.clone())
            }
            RunOrigin::Task { target, args } => {
                let name = std::iter::once(target.as_str())
                    .chain(args.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" ");
                let step = StepConfig {
                    step: Step::Moon {
                        target: target.clone(),
                        args: args.clone(),
                    },
                    on_failure: OnFailure::Stop,
                };
                (name, vec![step], None)
            }
        };
        let env = profile
            .or_else(|| self.runs.envs.active.clone())
            .map(|p| (self.runs.envs.clone(), p));
        self.runs.last = Some(req.clone());
        self.runs.next_id += 1;
        let run = RunId(self.runs.next_id);
        self.emit(Event::RunStarted {
            run,
            name,
            steps: steps.len(),
            origin: req,
        });
        let tx = self.tx.clone();
        let sink: Sink = Arc::new(move |update| {
            let _ = tx.send(Msg::Run { run, update });
        });
        let cancel = CancellationToken::new();
        let spec = ChainSpec {
            steps,
            root,
            env,
            tasks: self.runs.tasks.clone(),
        };
        let task = tokio::spawn(run_chain(spec, cancel.clone(), sink));
        self.runs.active.insert(
            run,
            ActiveRun {
                cancel,
                abort: task.abort_handle(),
                started: Instant::now(),
            },
        );
    }

    pub(super) fn run_update(&mut self, run: RunId, update: RunUpdate) {
        if !self.runs.active.contains_key(&run) {
            return;
        }
        match update {
            RunUpdate::Step {
                index,
                label,
                base_dirs,
            } => self.emit(Event::RunStep {
                run,
                index,
                label,
                base_dirs,
            }),
            RunUpdate::Output(text) => self.emit(Event::RunOutput { run, text }),
            RunUpdate::Finished {
                ok,
                failed_step,
                exit_code,
                stopped,
                error,
            } => {
                let Some(a) = self.runs.active.remove(&run) else {
                    return;
                };
                self.emit(Event::RunFinished {
                    run,
                    ok,
                    failed_step,
                    exit_code,
                    stopped,
                    error,
                    duration_ms: a.started.elapsed().as_millis() as u64,
                });
            }
        }
    }

    /// void is quitting: end every run (dropping a run's task kills its step's process group).
    pub(super) fn shutdown_runs(&mut self) {
        for (_, a) in self.runs.active.drain() {
            a.cancel.cancel();
            a.abort.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::super::{Command, Engine, EngineHandle, EngineOptions, Event};
    use crate::backend::make_backend;
    use crate::runner::{
        EnvProfile, EnvValue, Envs, Group, OnFailure, RunConfig, RunId, RunOrigin, Runs, Step,
        StepConfig,
    };
    use crate::store::Store;

    struct Rig {
        _dir: tempfile::TempDir,
        root: PathBuf,
        store: Store,
        handle: EngineHandle,
        /// Every event received so far.
        seen: Vec<Event>,
    }

    fn start(root: Option<PathBuf>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let root = root.unwrap_or_else(|| {
            let r = dir.path().join("ws");
            std::fs::create_dir_all(&r).unwrap();
            r
        });
        let store = Store::new(dir.path().join("data"));
        let handle = Engine::start_with_options(
            dir.path().join("config.toml"),
            store.clone(),
            Arc::new(|| {}),
            Arc::new(make_backend),
            EngineOptions {
                sandbox: false,
                ..EngineOptions::default()
            },
        );
        let mut r = Rig {
            _dir: dir,
            root,
            store,
            handle,
            seen: Vec::new(),
        };
        r.open();
        r
    }

    fn rig() -> Rig {
        start(None)
    }

    impl Rig {
        fn open(&mut self) {
            self.handle
                .send(Command::OpenWorkspace(Some(self.root.clone())));
            self.until(|e| {
                matches!(e, Event::WorkspaceOpened { workspace, .. } if workspace.root.is_some())
            });
            self.until(|e| matches!(e, Event::RunsLoaded { .. }));
        }

        fn next_within(&mut self, limit: Duration) -> Event {
            let deadline = Instant::now() + limit;
            loop {
                if let Some(e) = self.handle.try_recv() {
                    self.seen.push(e.clone());
                    return e;
                }
                assert!(Instant::now() < deadline, "timed out waiting for an event");
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn until(&mut self, mut pred: impl FnMut(&Event) -> bool) -> Event {
            loop {
                let e = self.next_within(Duration::from_secs(10));
                if pred(&e) {
                    return e;
                }
            }
        }

        fn save(&mut self, configs: Vec<RunConfig>) {
            self.handle.send(Command::SaveRuns(Runs {
                groups: vec![Group {
                    name: "g".into(),
                    configs,
                }],
            }));
            self.until(|e| matches!(e, Event::RunsLoaded { .. }));
        }

        fn set_envs(&mut self, envs: Envs) {
            self.handle.send(Command::SaveEnvs(envs));
            self.until(|e| matches!(e, Event::RunsLoaded { .. }));
        }

        fn started(&mut self) -> (RunId, String, usize) {
            match self.until(|e| matches!(e, Event::RunStarted { .. })) {
                Event::RunStarted {
                    run, name, steps, ..
                } => (run, name, steps),
                _ => unreachable!(),
            }
        }

        /// The run's events up to and including its `RunFinished`.
        fn finish(&mut self, run: RunId) -> Vec<Event> {
            let mut out = Vec::new();
            loop {
                let e = self.next_within(Duration::from_secs(15));
                let (mine, done) = match &e {
                    Event::RunStep { run: r, .. } | Event::RunOutput { run: r, .. } => {
                        (*r == run, false)
                    }
                    Event::RunFinished { run: r, .. } => (*r == run, *r == run),
                    _ => (false, false),
                };
                if mine {
                    out.push(e);
                }
                if done {
                    return out;
                }
            }
        }

        /// Wait for output of `run` containing `needle` (collecting nothing else).
        fn output_until(&mut self, run: RunId, needle: &str) {
            let mut text = String::new();
            self.until(|e| {
                if let Event::RunOutput { run: r, text: t } = e
                    && *r == run
                {
                    text.push_str(t);
                }
                text.contains(needle)
            });
        }

        fn run(&mut self, config: &RunConfig) -> (RunId, Vec<Event>) {
            self.handle.send(Command::RunConfig {
                config_id: config.id.clone(),
            });
            let (run, name, steps) = self.started();
            assert_eq!(name, config.name);
            assert_eq!(steps, config.steps.len());
            (run, self.finish(run))
        }
    }

    fn shell(command: &str) -> StepConfig {
        StepConfig {
            step: Step::Shell {
                command: command.into(),
            },
            on_failure: OnFailure::Stop,
        }
    }

    fn cont(command: &str) -> StepConfig {
        StepConfig {
            on_failure: OnFailure::Continue,
            ..shell(command)
        }
    }

    fn output(events: &[Event]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Event::RunOutput { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn steps(events: &[Event]) -> Vec<(usize, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::RunStep { index, label, .. } => Some((*index, label.clone())),
                _ => None,
            })
            .collect()
    }

    /// (ok, failed_step, exit_code, stopped, error)
    fn finished(events: &[Event]) -> (bool, Option<usize>, Option<i32>, bool, Option<String>) {
        match events.last() {
            Some(Event::RunFinished {
                ok,
                failed_step,
                exit_code,
                stopped,
                error,
                ..
            }) => (*ok, *failed_step, *exit_code, *stopped, error.clone()),
            other => panic!("not finished: {other:?}"),
        }
    }

    fn envs(profiles: &[(&str, &[(&str, EnvValue)])], active: Option<&str>) -> Envs {
        Envs {
            active: active.map(str::to_string),
            profiles: profiles
                .iter()
                .map(|(name, vars)| EnvProfile {
                    name: name.to_string(),
                    vars: vars
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.clone()))
                        .collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn chain_runs_in_order() {
        let mut r = rig();
        let cfg = RunConfig::new(
            "three",
            vec![shell("echo one"), shell("echo two >&2"), shell("pwd")],
        );
        r.save(vec![cfg.clone()]);
        let (_, ev) = r.run(&cfg);
        assert_eq!(
            steps(&ev),
            vec![
                (0, "echo one".to_string()),
                (1, "echo two >&2".to_string()),
                (2, "pwd".to_string()),
            ]
        );
        let root = r.root.canonicalize().unwrap();
        assert_eq!(output(&ev), format!("one\ntwo\n{}\n", root.display()));
        // Shell steps resolve output paths against the workspace root only.
        for e in &ev {
            if let Event::RunStep { base_dirs, .. } = e {
                assert_eq!(base_dirs, &vec![root.clone()]);
            }
        }
        // Each step's output comes after its RunStep and before the next one.
        let order: Vec<String> = ev
            .iter()
            .filter_map(|e| match e {
                Event::RunStep { index, .. } => Some(format!("step{index}")),
                Event::RunOutput { text, .. } => Some(text.trim().to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(&order[..4], ["step0", "one", "step1", "two"]);
        assert_eq!(finished(&ev), (true, None, None, false, None));
        match ev.last().unwrap() {
            Event::RunFinished { duration_ms, .. } => assert!(*duration_ms < 10_000),
            _ => unreachable!(),
        }
    }

    #[test]
    fn chain_stops_on_failure() {
        let mut r = rig();
        let cfg = RunConfig::new(
            "stops",
            vec![
                shell("echo a"),
                shell("echo b; exit 3"),
                shell("echo never"),
            ],
        );
        r.save(vec![cfg.clone()]);
        let (_, ev) = r.run(&cfg);
        assert_eq!(steps(&ev).len(), 2);
        assert_eq!(output(&ev), "a\nb\n");
        assert_eq!(finished(&ev), (false, Some(1), Some(3), false, None));
    }

    #[test]
    fn chain_continues_when_set() {
        let mut r = rig();
        let cfg = RunConfig::new(
            "continues",
            vec![cont("exit 2"), shell("echo after"), cont("exit 4")],
        );
        r.save(vec![cfg.clone()]);
        let (_, ev) = r.run(&cfg);
        assert_eq!(steps(&ev).len(), 3);
        assert_eq!(output(&ev), "after\n");
        // Not ok overall; the first failure is the one reported.
        assert_eq!(finished(&ev), (false, Some(0), Some(2), false, None));
    }

    #[test]
    fn stop_kills_chain() {
        let mut r = rig();
        let cfg = RunConfig::new(
            "long",
            vec![shell("echo started; sleep 30"), shell("echo never")],
        );
        r.save(vec![cfg.clone()]);
        r.handle.send(Command::RunConfig {
            config_id: cfg.id.clone(),
        });
        let (run, _, _) = r.started();
        r.output_until(run, "started");
        let t = Instant::now();
        r.handle.send(Command::StopRun(run));
        let ev = r.finish(run);
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(steps(&ev).is_empty(), "no later step: {ev:?}");
        assert!(!output(&ev).contains("never"));
        let (ok, failed, _, stopped, error) = finished(&ev);
        assert!(!ok && stopped && error.is_none());
        assert_eq!(failed, Some(0));
    }

    /// No process of the group `pgid` exists any more (zombies included).
    fn group_gone(pgid: i32) -> bool {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(-pgid, 0) == -1 }
    }

    /// Live (not zombie) processes whose process group is `pgid`.
    fn live_in_group(pgid: i32) -> Vec<i32> {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| {
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                    return false;
                };
                // After "(comm)": state ppid pgrp …
                let rest: Vec<&str> = stat
                    .rsplit_once(')')
                    .map_or("", |(_, r)| r)
                    .split_whitespace()
                    .collect();
                rest.len() > 2 && rest[0] != "Z" && rest[2].parse() == Ok(pgid)
            })
            .collect()
    }

    #[test]
    fn stop_kills_step_group() {
        let mut r = rig();
        let pidfile = r.root.join("pgid");
        // The step and its helpers ignore SIGTERM, so only the SIGKILL after 3 s ends them.
        let cmd = format!(
            "trap '' TERM; echo $$ > {}; sleep 300 & (sleep 300; true) & echo ready; wait",
            pidfile.display()
        );
        let cfg = RunConfig::new("stubborn", vec![shell(&cmd)]);
        r.save(vec![cfg.clone()]);
        r.handle.send(Command::RunConfig {
            config_id: cfg.id.clone(),
        });
        let (run, _, _) = r.started();
        r.output_until(run, "ready");
        let pgid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!group_gone(pgid));
        let t = Instant::now();
        r.handle.send(Command::StopRun(run));
        let ev = r.finish(run);
        let took = t.elapsed();
        assert!(
            took >= Duration::from_millis(2500) && took < Duration::from_secs(6),
            "{took:?}"
        );
        assert!(finished(&ev).3, "stopped");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !group_gone(pgid) {
            assert!(
                Instant::now() < deadline,
                "a process of group {pgid} survived Stop"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn quitting_kills_running_steps() {
        let mut r = rig();
        let pidfile = r.root.join("pgid");
        let cmd = format!(
            "echo $$ > {}; sleep 300 & echo ready; wait",
            pidfile.display()
        );
        let cfg = RunConfig::new("left running", vec![shell(&cmd)]);
        r.save(vec![cfg.clone()]);
        r.handle.send(Command::RunConfig {
            config_id: cfg.id.clone(),
        });
        let (run, _, _) = r.started();
        r.output_until(run, "ready");
        let pgid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        drop(r);
        // The test process stays the leader's parent with no runtime left to reap it, so a
        // zombie remains: count only live processes.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !live_in_group(pgid).is_empty() {
            assert!(
                Instant::now() < deadline,
                "group {pgid} outlived the engine"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn env_applied_to_steps() {
        let mut r = rig();
        r.set_envs(envs(
            &[
                ("dev", &[("FOO", EnvValue::Value("bar".into()))]),
                (
                    "other",
                    &[
                        ("FOO", EnvValue::Value("baz".into())),
                        ("FROM_CMD", EnvValue::Cmd("echo computed".into())),
                    ],
                ),
            ],
            Some("dev"),
        ));
        let active = RunConfig::new(
            "active",
            vec![shell("echo \"foo=$FOO\"; test -n \"$HOME\" && echo home")],
        );
        let mut pinned = RunConfig::new("pinned", vec![shell("echo \"foo=$FOO $FROM_CMD\"")]);
        pinned.env_profile = Some("other".into());
        r.save(vec![active.clone(), pinned.clone()]);
        let (_, ev) = r.run(&active);
        assert_eq!(output(&ev), "foo=bar\nhome\n");
        let (_, ev) = r.run(&pinned);
        assert_eq!(output(&ev), "foo=baz computed\n");

        // No active profile: nothing added.
        r.handle.send(Command::SetActiveEnv(None));
        r.until(|e| matches!(e, Event::RunsLoaded { envs, .. } if envs.active.is_none()));
        let (_, ev) = r.run(&active);
        assert_eq!(output(&ev), "foo=\nhome\n");

        // A profile that doesn't resolve fails the run before any step starts.
        r.set_envs(envs(
            &[(
                "broken",
                &[("TOKEN", EnvValue::Env("VOID_SURELY_UNSET_VAR".into()))],
            )],
            Some("broken"),
        ));
        let (_, ev) = r.run(&active);
        assert!(steps(&ev).is_empty());
        let (ok, failed, _, stopped, error) = finished(&ev);
        assert!(!ok && !stopped && failed.is_none());
        let error = error.unwrap();
        assert!(
            error.contains("broken") && error.contains("TOKEN"),
            "{error}"
        );
    }

    /// A config pinned to a profile that's since been deleted is refused, naming the profile,
    /// before anything runs: it never falls back to the active profile.
    #[test]
    fn pinned_profile_gone_refuses_the_run() {
        let mut r = rig();
        r.set_envs(envs(
            &[("prod", &[("DB", EnvValue::Value("prod-db".into()))])],
            Some("prod"),
        ));
        let marker = r.root.join("ran");
        let mut cfg = RunConfig::new(
            "migrate",
            vec![shell(&format!("touch {}", marker.display()))],
        );
        cfg.env_profile = Some("staging".into());
        r.save(vec![cfg.clone()]);
        let before = r.seen.len();
        r.handle.send(Command::RunConfig {
            config_id: cfg.id.clone(),
        });
        let Event::Error(e) = r.until(|e| matches!(e, Event::Error(_))) else {
            unreachable!()
        };
        assert!(e.contains("staging") && e.contains("migrate"), "{e}");
        assert!(!e.contains("prod-db"), "{e}");
        assert!(
            !r.seen[before..]
                .iter()
                .any(|e| matches!(e, Event::RunStarted { .. })),
            "the run never started"
        );
        assert!(!marker.exists());
    }

    #[test]
    fn rerun_last() {
        let mut r = rig();
        r.handle.send(Command::RerunLast);
        r.until(|e| matches!(e, Event::Info(_)));
        r.set_envs(envs(
            &[("dev", &[("V", EnvValue::Value("1".into()))])],
            Some("dev"),
        ));
        let cfg = RunConfig::new("again", vec![shell("echo \"v=$V\"")]);
        r.save(vec![cfg.clone()]);
        let (first, ev) = r.run(&cfg);
        assert_eq!(output(&ev), "v=1\n");
        r.set_envs(envs(
            &[("dev", &[("V", EnvValue::Value("2".into()))])],
            Some("dev"),
        ));
        r.handle.send(Command::RerunLast);
        let (second, name, steps) = r.started();
        assert_ne!(first, second);
        assert_eq!((name.as_str(), steps), ("again", 1));
        let ev = r.finish(second);
        assert_eq!(output(&ev), "v=2\n");
    }

    /// `RunStarted` says exactly what started the run (two configs can share a name; a
    /// config can be named like a task), and a re-run repeats that.
    #[test]
    fn run_started_carries_its_origin() {
        let mut r = rig();
        let origin = |r: &mut Rig| match r.until(|e| matches!(e, Event::RunStarted { .. })) {
            Event::RunStarted { origin, .. } => origin,
            _ => unreachable!(),
        };
        let a = RunConfig::new("test", vec![shell("true")]);
        let b = RunConfig::new("test", vec![shell("true")]);
        r.save(vec![a, b.clone()]);
        r.handle.send(Command::RunConfig {
            config_id: b.id.clone(),
        });
        assert_eq!(origin(&mut r), RunOrigin::Config { id: b.id.clone() });
        let task = RunOrigin::Task {
            target: "app:build".into(),
            args: vec!["--release".into()],
        };
        r.handle.send(Command::RunTask {
            target: "app:build".into(),
            args: vec!["--release".into()],
        });
        assert_eq!(origin(&mut r), task);
        r.handle.send(Command::RerunLast);
        assert_eq!(origin(&mut r), task);
    }

    #[test]
    fn concurrent_runs_are_independent() {
        let mut r = rig();
        let slow = RunConfig::new("slow", vec![shell("echo going; sleep 30")]);
        let quick = RunConfig::new("quick", vec![shell("echo quick")]);
        r.save(vec![slow.clone(), quick.clone()]);
        r.handle.send(Command::RunConfig {
            config_id: slow.id.clone(),
        });
        let (a, _, _) = r.started();
        r.output_until(a, "going");
        let (b, ev) = r.run(&quick);
        assert_ne!(a, b);
        assert_eq!(output(&ev), "quick\n");
        assert!(finished(&ev).0);
        r.handle.send(Command::StopRun(a));
        let ev = r.finish(a);
        assert!(finished(&ev).3);
    }

    #[test]
    fn secrets_never_in_events() {
        let secret = "s3cr3t-literal-9f8e7d";
        let computed = "c0mputed-secret-1a2b3c";
        let mut r = rig();
        r.set_envs(envs(
            &[(
                "dev",
                &[
                    ("API_KEY", EnvValue::Value(secret.into())),
                    ("TOKEN", EnvValue::Cmd(format!("echo {computed}"))),
                ],
            )],
            Some("dev"),
        ));
        let cfg = RunConfig::new(
            "leaky",
            vec![shell("echo \"key=$API_KEY tok=$TOKEN\""), shell("exit 1")],
        );
        r.save(vec![cfg.clone()]);
        let (run, ev) = r.run(&cfg);
        assert_eq!(output(&ev), format!("key={secret} tok={computed}\n"));
        // A profile whose cmd prints the secret and then fails.
        r.set_envs(envs(
            &[(
                "dev",
                &[(
                    "TOKEN",
                    EnvValue::Cmd(format!("echo {computed}; echo {computed} >&2; exit 1")),
                )],
            )],
            Some("dev"),
        ));
        let (_, ev) = r.run(&cfg);
        assert!(finished(&ev).4.is_some());
        for e in &r.seen {
            if matches!(e, Event::RunOutput { run: r2, .. } if *r2 == run) {
                continue;
            }
            let dbg = format!("{e:?}");
            assert!(!dbg.contains(secret), "{dbg}");
            assert!(!dbg.contains(computed), "{dbg}");
        }
        // The values are in env.toml only as configured (the literal; not the cmd's output).
        let ws = r.store.workspace_for(&r.root).unwrap();
        let (_, env_path) = r.store.runner_paths(&ws.id);
        let text = std::fs::read_to_string(env_path).unwrap();
        assert!(!text.contains(&format!("= \"{computed}\"")));
    }

    #[test]
    fn runs_and_envs_persist_per_workspace() {
        let mut r = rig();
        let cfg = RunConfig::new("kept", vec![shell("true")]);
        r.save(vec![cfg.clone()]);
        r.set_envs(envs(
            &[("dev", &[("A", EnvValue::Value("1".into()))])],
            Some("dev"),
        ));
        r.handle.send(Command::OpenWorkspace(None));
        r.until(|e| matches!(e, Event::RunsLoaded { runs, .. } if runs.groups.is_empty()));
        r.open();
        match r.seen.last().unwrap() {
            Event::RunsLoaded { runs, envs, .. } => {
                assert_eq!(runs.groups[0].configs, vec![cfg]);
                assert_eq!(envs.active.as_deref(), Some("dev"));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn unreadable_runs_file_is_never_overwritten() {
        let mut r = rig();
        let id = r.store.workspace_for(&r.root).unwrap().id;
        let (runs_path, _) = r.store.runner_paths(&id);
        // A directory where runs.toml should be: unreadable, but not corrupt.
        std::fs::create_dir_all(&runs_path).unwrap();
        r.handle.send(Command::OpenWorkspace(None));
        r.until(|e| matches!(e, Event::RunsLoaded { .. }));
        r.handle.send(Command::OpenWorkspace(Some(r.root.clone())));
        r.until(|e| matches!(e, Event::Error(m) if m.contains("runs.toml")));
        r.until(|e| matches!(e, Event::RunsLoaded { .. }));
        r.handle.send(Command::SaveRuns(Runs::default()));
        r.until(|e| matches!(e, Event::Error(m) if m.contains("runs.toml")));
        assert!(runs_path.is_dir());
        // env.toml is fine, so it still saves.
        r.set_envs(Envs::default());
    }

    /// A refused save is answered with what's really kept, so a UI that showed the change
    /// early goes back to it.
    #[test]
    fn refused_save_reports_the_state_kept() {
        let mut r = rig();
        let id = r.store.workspace_for(&r.root).unwrap().id;
        let (runs_path, env_path) = r.store.runner_paths(&id);
        std::fs::create_dir_all(&runs_path).unwrap();
        std::fs::create_dir_all(&env_path).unwrap();
        r.handle.send(Command::OpenWorkspace(None));
        r.until(|e| matches!(e, Event::RunsLoaded { .. }));
        r.handle.send(Command::OpenWorkspace(Some(r.root.clone())));
        r.until(|e| matches!(e, Event::RunsLoaded { .. }));
        let runs = Runs {
            groups: vec![Group {
                name: "g".into(),
                configs: vec![RunConfig::new("x", vec![shell("true")])],
            }],
        };
        r.handle.send(Command::SaveRuns(runs));
        r.until(|e| matches!(e, Event::Error(m) if m.contains("Not saving run configs")));
        match r.next_within(Duration::from_secs(5)) {
            Event::RunsLoaded { runs, .. } => assert!(runs.groups.is_empty()),
            e => panic!("expected RunsLoaded, got {e:?}"),
        }
        r.handle.send(Command::SaveEnvs(envs(
            &[("dev", &[("V", EnvValue::Value("1".into()))])],
            Some("dev"),
        )));
        r.until(|e| matches!(e, Event::Error(m) if m.contains("Not saving environment")));
        match r.next_within(Duration::from_secs(5)) {
            Event::RunsLoaded { envs, .. } => assert_eq!(envs, Envs::default()),
            e => panic!("expected RunsLoaded, got {e:?}"),
        }
    }

    #[test]
    fn tasks_empty_outside_moon_workspace() {
        let mut r = rig();
        r.handle.send(Command::RefreshTasks);
        // Not a moon workspace: no tasks, and opening it reported no error.
        let e = r.until(|e| matches!(e, Event::Tasks(_) | Event::Error(_)));
        assert!(matches!(&e, Event::Tasks(t) if t.is_empty()), "{e:?}");
        assert!(!r.seen.iter().any(|e| matches!(e, Event::Error(_))));
    }

    #[test]
    #[ignore = "needs moon (proto) and runs cargo fmt on void.ai"]
    fn live_moon_run_format_check() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut r = start(Some(repo.canonicalize().unwrap()));
        // Opening clears the list at once; moon's answer follows (or an error).
        let tasks = r.until(|e| {
            matches!(e, Event::Tasks(t) if !t.is_empty()) || matches!(e, Event::Error(_))
        });
        assert!(
            matches!(&tasks, Event::Tasks(t) if t.iter().any(|t| t.target == "workspace:format")),
            "{tasks:?}"
        );
        // Each task knows its project's folder.
        let Event::Tasks(tasks) = tasks else {
            unreachable!()
        };
        let gui = tasks.iter().find(|t| t.target == "void-gui:run").unwrap();
        assert_eq!(gui.source.as_deref(), Some("apps/void-gui"));
        r.handle.send(Command::RunTask {
            target: "workspace:format".into(),
            args: vec![],
        });
        let (run, name, count) = r.started();
        assert_eq!((name.as_str(), count), ("workspace:format", 1));
        let mut ev = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let e = r.next_within(deadline.saturating_duration_since(Instant::now()));
            let done = matches!(&e, Event::RunFinished { run: x, .. } if *x == run);
            ev.push(e);
            if done {
                break;
            }
        }
        let out = output(&ev);
        assert_eq!(
            steps(&ev),
            vec![(0, "moon run workspace:format".to_string())]
        );
        assert!(finished(&ev).0, "{out}");
        // A root-level project: paths are relative to the workspace root.
        let root = repo.canonicalize().unwrap();
        assert!(ev.iter().any(
            |e| matches!(e, Event::RunStep { base_dirs, .. } if *base_dirs == vec![root.clone()])
        ));
    }
}
