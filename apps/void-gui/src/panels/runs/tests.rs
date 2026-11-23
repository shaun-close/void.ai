//! The runner's UI driven headlessly through the real app (egui_kittest, offline). Runs are
//! scripted: their events are fed into the app's state, no process is started.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use void_core::engine::{Command, Event};
use void_core::runner::{EnvProfile, EnvValue, Envs, RunId};

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
