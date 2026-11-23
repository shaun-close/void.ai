//! The runner's UI driven headlessly through the real app (egui_kittest, offline). Runs are
//! scripted: their events are fed into the app's state, no process is started.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use void_core::engine::{Command, Event};
use void_core::runner::{
    EnvProfile, EnvValue, Envs, Group, OnFailure, RunConfig, RunId, RunOrigin, Runs, Step,
    StepConfig,
};

use crate::app::VoidApp;
use crate::layout::{EditorView, Tab};

/// A workspace folder `proj` inside a temp dir (config and data live next to it).
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let ws = t.path().canonicalize().unwrap().join("proj");
    fs::create_dir(&ws).unwrap();
    (t, ws)
}

fn harness(t: &Path, ws: &Path) -> Harness<'static, VoidApp> {
    let (cfg, data) = (t.join("config.toml"), t.join("data"));
    let open = Some(ws.to_path_buf());
    let mut h = Harness::builder()
        .with_size([1280.0, 800.0])
        .with_max_steps(10)
        .with_accessibility_check(false)
        .build_eframe(move |cc| VoidApp::new(cc, open, cfg, data));
    let want = ws.to_path_buf();
    until(&mut h, "workspace and its runs", |a| {
        a.state.workspace.root.as_deref() == Some(want.as_path()) && a.state.runner_loaded
    });
    h
}

fn until(h: &mut Harness<'_, VoidApp>, what: &str, done: impl Fn(&VoidApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        h.step();
        if done(h.state()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn steps(h: &mut Harness<'_, VoidApp>, n: usize) {
    for _ in 0..n {
        h.step();
    }
}

#[test]
fn run_output_location_opens_editor() {
    let (t, ws) = workspace();
    fs::create_dir(ws.join("src")).unwrap();
    let file = ws.join("src/a.rs");
    fs::write(&file, "fn main() {\n    let x = 1;\n}\n").unwrap();
    let mut h = harness(t.path(), &ws);
    let run = RunId(901);
    {
        let st = &mut h.state_mut().state;
        st.apply(Event::RunStarted {
            run,
            name: "check".into(),
            steps: 1,
            origin: RunOrigin::Task {
                target: "app:check".into(),
                args: vec![],
            },
        });
        st.apply(Event::RunStep {
            run,
            index: 0,
            label: "cargo check".into(),
            base_dirs: vec![ws.clone()],
        });
        st.apply(Event::RunOutput {
            run,
            text:
                "\x1b[1m\x1b[91merror\x1b[0m: unused variable\n  \x1b[94m--> \x1b[0msrc/a.rs:2:3\n"
                    .into(),
        });
    }
    steps(&mut h, 4);
    assert!(
        h.state().dock.find_tab(&Tab::RunOutput { run }).is_some(),
        "the run got an output tab"
    );
    h.get_by_label("src/a.rs:2:3").click();
    steps(&mut h, 3);
    let key = (file.clone(), EditorView::Disk);
    let editor_open = h
        .state()
        .dock
        .iter_all_tabs()
        .any(|(_, t)| matches!(t, Tab::Editor { path, .. } if *path == file));
    assert!(editor_open, "an editor tab on src/a.rs");
    let doc = h.state().editors.doc(&key).expect("buffer");
    assert_eq!(doc.core.line_col().0, 1, "at line 2 (zero-based 1)");
}

#[test]
fn env_pill_switches_profile() {
    let (t, ws) = workspace();
    let mut h = harness(t.path(), &ws);
    let profile = |name: &str| EnvProfile {
        name: name.into(),
        vars: [("API_URL".to_string(), EnvValue::Value("x".into()))].into(),
    };
    h.state().engine.send(Command::SaveEnvs(Envs {
        active: None,
        profiles: vec![profile("dev"), profile("prod")],
    }));
    until(&mut h, "profiles", |a| a.state.envs.profiles.len() == 2);
    h.get_by_label_contains("no env").click();
    steps(&mut h, 2);
    h.get_by_label("prod").click();
    until(&mut h, "prod active", |a| {
        a.state.envs.active.as_deref() == Some("prod")
    });
    steps(&mut h, 2);
    assert!(h.query_by_label_contains("no env").is_none());
    // And back to none.
    h.get_by_label_contains("prod").click();
    steps(&mut h, 2);
    h.get_by_label("No environment").click();
    until(&mut h, "none active", |a| a.state.envs.active.is_none());
}

#[test]
fn moon_config_change_refreshes_tasks() {
    let (t, ws) = workspace();
    let mut h = harness(t.path(), &ws);
    until(&mut h, "watcher", |a| a.disk.ready());
    // A stale task list; the engine answers a refresh outside a moon workspace with none.
    h.state_mut().state.tasks = vec![void_core::runner::MoonTask {
        project: "app".into(),
        id: "build".into(),
        target: "app:build".into(),
        command: "cargo build".into(),
        description: None,
        source: None,
        run_from_workspace_root: false,
    }];
    steps(&mut h, 2);
    assert_eq!(
        h.state().state.tasks.len(),
        1,
        "no refresh without a change"
    );
    fs::write(ws.join("moon.yml"), "tasks: {}\n").unwrap();
    until(&mut h, "tasks asked for again", |a| {
        a.state.tasks.is_empty()
    });
}

/// Show the Runs tab (it shares a leaf with Chats and Files).
fn show_runs(h: &mut Harness<'_, VoidApp>) {
    crate::layout::ensure_tab(&mut h.state_mut().dock, Tab::Runs);
    steps(h, 3);
}

#[test]
fn new_config_is_saved_through_the_engine() {
    let (t, ws) = workspace();
    let mut h = harness(t.path(), &ws);
    show_runs(&mut h);
    h.get_by_label_contains("New config").click();
    until(&mut h, "config saved", |a| {
        a.state.runs.groups.len() == 1 && a.state.runs.groups[0].configs.len() == 1
    });
    // The editor opened on it: add a shell step and save.
    steps(&mut h, 2);
    h.get_by_label_contains("shell step").click();
    steps(&mut h, 2);
    h.get_by_label(&format!("{}Save", crate::theme::icon::FLOPPY))
        .click();
    until(&mut h, "step saved", |a| {
        a.state.runs.groups[0].configs[0].steps.len() == 1
    });
    // It reached runs.toml, which the engine owns.
    let id = h.state().state.workspace.id.clone();
    let (runs_path, _) = void_core::store::Store::new(t.path().join("data")).runner_paths(&id);
    let (on_disk, _) = void_core::runner::Runs::load(&runs_path).unwrap();
    assert_eq!(on_disk, h.state().state.runs);
}

#[test]
fn runs_warning_shown_until_dismissed() {
    let (t, ws) = workspace();
    let mut h = harness(t.path(), &ws);
    show_runs(&mut h);
    let (runs, envs) = (h.state().state.runs.clone(), h.state().state.envs.clone());
    h.state_mut().state.apply(Event::RunsLoaded {
        runs,
        envs,
        warning: Some("runs.toml was unreadable; kept as runs.toml.bad-1".into()),
    });
    steps(&mut h, 2);
    // In the panel's banner, and in the status bar.
    let shown = |h: &Harness<'_, VoidApp>| {
        h.query_all_by_label_contains("kept as runs.toml.bad-1")
            .count()
    };
    assert_eq!(shown(&h), 2);
    h.get_by_label(crate::theme::icon::X_CIRCLE).click();
    steps(&mut h, 2);
    assert_eq!(shown(&h), 1, "the banner went; the status line stays");
}

#[test]
fn ctrl_shift_f10_reruns_last() {
    let (t, ws) = workspace();
    let mut h = harness(t.path(), &ws);
    h.key_press_modifiers(
        egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        egui::Key::F10,
    );
    // Nothing ran yet, and the engine says so.
    until(&mut h, "the engine's answer", |a| {
        a.state
            .status
            .as_ref()
            .is_some_and(|s| s.text.contains("Nothing to re-run"))
    });
}

/// Finish a scripted run (its tab then offers Re-run).
fn script_run(h: &mut Harness<'_, VoidApp>, run: RunId, name: &str, origin: RunOrigin) {
    let st = &mut h.state_mut().state;
    st.apply(Event::RunStarted {
        run,
        name: name.into(),
        steps: 1,
        origin,
    });
    st.apply(Event::RunFinished {
        run,
        ok: true,
        failed_step: None,
        exit_code: Some(0),
        stopped: false,
        error: None,
        duration_ms: 5,
    });
    steps(h, 3);
}

/// Whether the shown output tab's Re-run button is enabled.
fn rerun_enabled(h: &Harness<'_, VoidApp>) -> bool {
    let label = format!("{}Re-run", crate::theme::icon::ARROW_CLOCKWISE);
    !h.get_by_label(&label).accesskit_node().is_disabled()
}

fn click_rerun(h: &Harness<'_, VoidApp>) {
    let label = format!("{}Re-run", crate::theme::icon::ARROW_CLOCKWISE);
    h.get_by_label(&label).click();
}

/// Two configs share a name: the output tab's Re-run runs the one that ran, by id.
#[test]
fn output_rerun_runs_exactly_what_ran() {
    let (t, ws) = workspace();
    let mut h = harness(t.path(), &ws);
    let cfg = || {
        RunConfig::new(
            "test",
            vec![StepConfig {
                step: Step::Shell {
                    command: "true".into(),
                },
                on_failure: OnFailure::Stop,
            }],
        )
    };
    let (first, second) = (cfg(), cfg());
    h.state().engine.send(Command::SaveRuns(Runs {
        groups: vec![Group {
            name: "g".into(),
            configs: vec![first, second.clone()],
        }],
    }));
    until(&mut h, "configs", |a| {
        a.state
            .runs
            .groups
            .first()
            .is_some_and(|g| g.configs.len() == 2)
    });
    let ran = RunOrigin::Config {
        id: second.id.clone(),
    };
    script_run(&mut h, RunId(950), "test", ran.clone());
    click_rerun(&h);
    // (Its tab takes over the finished run's, which is then forgotten.)
    until(&mut h, "the re-run started", |a| {
        a.state.run_views.keys().any(|id| *id != RunId(950))
    });
    let again: Vec<RunOrigin> = h
        .state()
        .state
        .run_views
        .iter()
        .filter(|(id, _)| **id != RunId(950))
        .map(|(_, v)| v.origin.clone())
        .collect();
    assert_eq!(again, [ran]);
}

/// A run of a config that's since been deleted can't be re-run; a task always can.
#[test]
fn output_rerun_disabled_when_its_config_is_gone() {
    let (t, ws) = workspace();
    let mut h = harness(t.path(), &ws);
    script_run(
        &mut h,
        RunId(960),
        "app:build",
        RunOrigin::Config { id: "gone".into() },
    );
    assert!(!rerun_enabled(&h));
    script_run(
        &mut h,
        RunId(961),
        "renamed",
        RunOrigin::Task {
            target: "app:build".into(),
            args: vec!["--release".into()],
        },
    );
    assert!(rerun_enabled(&h));
}

/// The first run's output opens in a new strip split off the editor's leaf; the editor
/// keeps the focus it had.
#[test]
fn output_split_keeps_editor_focus() {
    let (t, ws) = workspace();
    let file = ws.join("a.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let mut h = harness(t.path(), &ws);
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    steps(&mut h, 3);
    let focused_editor = |h: &mut Harness<'_, VoidApp>| {
        matches!(
            h.state_mut().dock.find_active_focused(),
            Some((_, Tab::Editor { path, .. })) if *path == file
        )
    };
    assert!(focused_editor(&mut h), "editor focused to begin with");
    script_run(
        &mut h,
        RunId(970),
        "app:build",
        RunOrigin::Task {
            target: "app:build".into(),
            args: vec![],
        },
    );
    assert!(
        h.state()
            .dock
            .find_tab(&Tab::RunOutput { run: RunId(970) })
            .is_some(),
        "output tab opened"
    );
    assert!(focused_editor(&mut h), "editor still focused");
}

/// Saving profiles whose input the save normalises (variables out of order, stray
/// whitespace) still releases the draft: nothing left unsaved, no "changed elsewhere".
#[test]
fn env_draft_released_after_a_normalising_save() {
    use super::{EnvsDraft, Kind, ProfileDraft, VarDraft};
    let (t, ws) = workspace();
    let mut h = harness(t.path(), &ws);
    show_runs(&mut h);
    h.state_mut().runs.show_envs = true;
    steps(&mut h, 3);
    let var = |name: &str, kind, value: &str| VarDraft {
        name: name.into(),
        kind,
        value: value.into(),
    };
    h.state_mut().runs.envs = Some((
        vec![],
        EnvsDraft {
            profiles: vec![ProfileDraft {
                name: " dev ".into(),
                was: None,
                vars: vec![var("B ", Kind::Value, "2"), var("A", Kind::Env, " HOME ")],
            }],
        },
    ));
    steps(&mut h, 2);
    let save = format!("{}Save", crate::theme::icon::FLOPPY);
    h.get_by_label(&save).click();
    until(&mut h, "saved", |a| a.state.envs.profiles.len() == 1);
    steps(&mut h, 3);
    assert_eq!(
        h.state().state.envs.profiles[0].vars["A"],
        EnvValue::Env("HOME".into())
    );
    assert!(
        h.get_by_label(&save).accesskit_node().is_disabled(),
        "nothing left to save"
    );
    assert!(h.query_by_label_contains("Changed elsewhere").is_none());
}

/// A config pinned to a deleted profile keeps the reference, and its editor says so.
#[test]
fn editor_shows_a_missing_profile() {
    let (t, ws) = workspace();
    let mut h = harness(t.path(), &ws);
    let mut cfg = RunConfig::new("deploy", vec![]);
    cfg.env_profile = Some("staging".into());
    h.state().engine.send(Command::SaveEnvs(Envs {
        active: Some("prod".into()),
        profiles: vec![EnvProfile {
            name: "prod".into(),
            vars: Default::default(),
        }],
    }));
    h.state().engine.send(Command::SaveRuns(Runs {
        groups: vec![Group {
            name: "g".into(),
            configs: vec![cfg.clone()],
        }],
    }));
    until(&mut h, "loaded", |a| {
        a.state.envs.profiles.len() == 1 && a.state.runs.groups.len() == 1
    });
    show_runs(&mut h);
    let runs = h.state().state.runs.clone();
    h.state_mut().runs.edit_config(&runs, &cfg.id);
    steps(&mut h, 3);
    assert!(h.query_by_value("staging (missing)").is_some());
    assert!(h.query_by_label_contains("won't run").is_some());
}
