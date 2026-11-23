//! The palette: ranking (pure), and the real app driven headlessly (egui_kittest, offline).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::accesskit::Role;
use egui::{Event, Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};

use super::*;
use crate::app::VoidApp;
use crate::layout::{EditorView, Tab};

fn names(ranked: &[(Item, Score)]) -> Vec<String> {
    ranked.iter().map(|(i, _)| format!("{i:?}")).collect()
}

fn sample() -> Vec<Item> {
    vec![
        Item::File("src/format.rs".into()),
        Item::Tab(TabRef {
            id: 1,
            path: "/w/src/runner/format_args.rs".into(),
            label: "src/runner/format_args.rs".into(),
        }),
        Item::Command(CommandId::ResetLayout),
        Item::Command(CommandId::ShowRuns),
        Item::Task("void-core:format".into()),
        Item::RunConfig {
            id: "c1".into(),
            name: "Format all".into(),
        },
        Item::Env("formal".into()),
        Item::Env("dev".into()),
        Item::Symbol {
            name: "format_line".into(),
            line: 3,
        },
    ]
}

#[test]
fn palette_prefixes_filter_kinds() {
    let items = sample();
    let kinds = |q: &str| -> Vec<Kind> {
        let mut k: Vec<Kind> = rank(q, &items, 50).iter().map(|(i, _)| i.kind()).collect();
        k.dedup();
        k
    };
    // No prefix: everything that matches, of any kind.
    let all = rank("form", &items, 50);
    assert_eq!(all.len(), 6, "{:?}", names(&all));
    // `>` commands, `@` symbols, `!` runs (tasks and configs), `$` envs, `/` files (and the
    // open tabs, which are files).
    assert_eq!(kinds(">lay"), vec![Kind::Command]);
    assert_eq!(kinds("@form"), vec![Kind::Symbol]);
    let runs = rank("!form", &items, 50);
    assert_eq!(runs.len(), 2, "{:?}", names(&runs));
    assert!(
        runs.iter()
            .all(|(i, _)| matches!(i.kind(), Kind::Task | Kind::RunConfig))
    );
    assert_eq!(kinds("$form"), vec![Kind::Env]);
    let files = rank("/form", &items, 50);
    assert_eq!(files.len(), 2, "{:?}", names(&files));
    assert!(
        files
            .iter()
            .all(|(i, _)| matches!(i.kind(), Kind::File | Kind::Tab))
    );
    // A space after the prefix is fine; a prefix alone lists its whole kind, in order.
    assert_eq!(kinds("> lay"), vec![Kind::Command]);
    assert_eq!(
        rank("$", &items, 50)
            .into_iter()
            .map(|(i, _)| i)
            .collect::<Vec<_>>(),
        vec![Item::Env("formal".into()), Item::Env("dev".into())]
    );
    // Commands match their label.
    assert_eq!(
        rank(">reset", &items, 50)[0].0,
        Item::Command(CommandId::ResetLayout)
    );
    // `limit` caps the answer.
    assert_eq!(rank("form", &items, 3).len(), 3);
    // Best first.
    let scores: Vec<Score> = all.iter().map(|(_, s)| *s).collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
}

/// 10 000 paths shaped like a real tree.
fn ten_thousand() -> Vec<Item> {
    let words = [
        "core", "editor", "runner", "panels", "theme", "engine", "store", "sandbox", "vim",
        "layout",
    ];
    let mut items: Vec<Item> = (0..10_000)
        .map(|i| {
            let (a, b, c) = (words[i % 10], words[(i / 10) % 10], words[(i / 100) % 10]);
            Item::File(format!("crates/{a}/src/{b}/{c}_{i}.rs").into())
        })
        .collect();
    items[7_777] = Item::File("apps/void-gui/src/model.rs".into());
    items
}

/// The bar is for release builds (< 50 ms for 10 000 paths); debug builds under the gate's
/// parallel load get a generous bound, and the work counters catch the regressions a timer
/// can't see reliably there: re-scoring everything per keystroke, or sorting every match.
#[test]
fn palette_ranks_10k_files_fast() {
    let items = ten_thousand();
    let bar = if cfg!(debug_assertions) {
        Duration::from_millis(2_000)
    } else {
        Duration::from_millis(50)
    };
    const LIMIT: usize = 50;

    // Cold: one query over everything.
    let t = Instant::now();
    let mut ranker = Ranker::default();
    let top = ranker.rank("model.rs", &items, LIMIT);
    let cold = t.elapsed();
    assert_eq!(
        top.first().map(|(i, _)| &items[*i]),
        Some(&Item::File("apps/void-gui/src/model.rs".into()))
    );
    assert_eq!(ranker.stats.scored, 10_000);

    // A query everything matches: the answer and the sort stay capped at `limit`.
    let t = Instant::now();
    let all = Ranker::default().rank("rs", &items, LIMIT);
    let broad = t.elapsed();
    let mut r = Ranker::default();
    r.rank("rs", &items, LIMIT);
    assert_eq!(r.stats.matched, 10_000);
    assert_eq!(all.len(), LIMIT);
    assert!(
        r.stats.sorted <= LIMIT,
        "sorted {} of {} matches",
        r.stats.sorted,
        r.stats.matched
    );

    // Typing a query a character at a time re-scores only the previous matches.
    let mut ranker = Ranker::default();
    let mut scored = Vec::new();
    let mut slowest = Duration::ZERO;
    let t = Instant::now();
    let typed = "runner/vim";
    for n in 1..=typed.len() {
        let k = Instant::now();
        let matched_before = ranker.stats.matched;
        ranker.rank(&typed[..n], &items, LIMIT);
        slowest = slowest.max(k.elapsed());
        if n > 1 {
            assert!(
                ranker.stats.scored <= matched_before,
                "keystroke {n} scored {} items, but only {matched_before} could still match",
                ranker.stats.scored
            );
        }
        scored.push(ranker.stats.scored);
    }
    let typing = t.elapsed();
    let total: usize = scored.iter().sum();
    println!(
        "palette_ranks_10k_files_fast: cold {cold:?}, all-match {broad:?}, typing {} keys {typing:?} \
         (slowest key {slowest:?}, scored {total} items: {scored:?}); bar {bar:?}",
        typed.len()
    );
    // Re-scoring everything would be 10 000 per key.
    assert!(
        total < 10_000 * typed.len() / 2,
        "typing scored {total} items in all: {scored:?}"
    );
    assert!(cold < bar, "cold rank took {cold:?} (bar {bar:?})");
    assert!(broad < bar, "all-match rank took {broad:?} (bar {bar:?})");
    assert!(slowest < bar, "a keystroke took {slowest:?} (bar {bar:?})");
}

/// A prefix change or a deleted character starts over (the narrowing only holds when the
/// new query extends the old one).
#[test]
fn palette_ranker_starts_over_when_the_query_widens() {
    let items = ten_thousand();
    let mut r = Ranker::default();
    let narrow = r.rank("model", &items, 10);
    assert_eq!(narrow.len(), 1);
    let wider = r.rank("mod", &items, 10);
    assert!(wider.len() > 1, "{wider:?}");
    assert_eq!(r.stats.scored, 10_000, "started over");
    assert!(r.rank("/mod", &items, 10).len() > 1);
    assert_eq!(r.stats.scored, 10_000, "a new prefix starts over");
}

// ---- The real app ----

fn harness(dir: &Path) -> Harness<'static, VoidApp> {
    let (cfg, data) = (dir.join("config.toml"), dir.join("data"));
    let mut h = Harness::builder()
        .with_size([1200.0, 800.0])
        .with_max_steps(10)
        .with_accessibility_check(false)
        .build_eframe(move |cc| VoidApp::new(cc, None, cfg, data));
    until(&mut h, "workspace", |a| a.state.workspace.name == "Scratch");
    h.step();
    h
}

fn until(h: &mut Harness<'_, VoidApp>, what: &str, done: impl Fn(&VoidApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        h.step();
        if done(h.state()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Two taps of Shift, delivered in one frame.
fn shift_shift(h: &mut Harness<'_, VoidApp>) {
    for m in [
        Modifiers::SHIFT,
        Modifiers::NONE,
        Modifiers::SHIFT,
        Modifiers::NONE,
    ] {
        h.event(Event::ModifiersChanged(m));
    }
    h.step();
    h.step();
}

fn text(h: &mut Harness<'_, VoidApp>, s: &str) {
    h.event(Event::Text(s.into()));
    h.step();
    h.step();
}

fn shown(h: &Harness<'_, VoidApp>) -> String {
    h.get_by_role(Role::MultilineTextInput)
        .value()
        .unwrap_or_default()
}

fn editor_paths(app: &VoidApp) -> Vec<PathBuf> {
    app.dock
        .iter_all_tabs()
        .filter_map(|(_, t)| match t {
            Tab::Editor { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

/// A folder workspace with a few files; returns its root once the palette has its files.
fn workspace(h: &mut Harness<'_, VoidApp>, dir: &Path, files: &[(&str, &str)]) -> PathBuf {
    let root = dir.join("proj");
    for (path, text) in files {
        let f = root.join(path);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }
    h.state()
        .engine
        .send(void_core::engine::Command::OpenWorkspace(Some(
            root.clone(),
        )));
    until(h, "folder workspace", |a| a.state.workspace.name == "proj");
    // The walk lands all at once.
    until(h, "palette files", |a| !a.palette.files.files().is_empty());
    root
}

/// One file open in a focused editor tab (Scratch).
fn editor_on(h: &mut Harness<'_, VoidApp>, dir: &Path, name: &str, text: &str) -> PathBuf {
    let file = dir.join(name);
    std::fs::write(&file, text).unwrap();
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    assert!(h.query_by_label("NORMAL").is_some(), "editor focused");
    file
}

#[test]
fn palette_opens_file() {
    let tmp = tempfile::tempdir().unwrap();
    let mut h = harness(tmp.path());
    let root = workspace(
        &mut h,
        tmp.path(),
        &[
            ("src/model.rs", "pub struct Model;\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("docs/models.md", "# Models\n"),
            ("target/debug/model.rs", "ignored\n"),
            (".gitignore", "target/\n"),
        ],
    );
    assert!(
        !h.state()
            .palette
            .files
            .files()
            .iter()
            .any(|p| p.starts_with("target")),
        "gitignored files stay out of the palette"
    );
    shift_shift(&mut h);
    assert!(
        h.state().palette.is_open(),
        "Shift, Shift opens the palette"
    );
    text(&mut h, "model.rs");
    assert_eq!(h.state().palette.query(), "model.rs");
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert!(!h.state().palette.is_open(), "Enter closes it");
    assert_eq!(editor_paths(h.state()), vec![root.join("src/model.rs")]);
}

/// R1: outline lines are zero-based, editor lines one-based: the palette adds one.
#[test]
fn palette_symbol_jumps_to_its_line() {
    let tmp = tempfile::tempdir().unwrap();
    let mut h = harness(tmp.path());
    let file = editor_on(
        &mut h,
        tmp.path(),
        "lib.rs",
        "// one\n// two\n// three\n\nfn target() {}\n\nfn other() {}\n",
    );
    let key = (file, EditorView::Disk);
    until(&mut h, "parsed", |a| {
        a.editors.doc(&key).is_some_and(|d| d.highlighted())
    });
    shift_shift(&mut h);
    text(&mut h, "@target");
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert!(!h.state().palette.is_open());
    // `fn target` is on zero-based line 4: editor line 5.
    assert!(
        h.query_by_label("5:1").is_some(),
        "cursor on line 5, column 1"
    );
}

/// R3: Shift+letter in a focused Vim editor types the letter, and the Shift tap after it
/// doesn't open the palette.
#[test]
fn palette_shift_letter_still_types_in_vim() {
    let tmp = tempfile::tempdir().unwrap();
    let mut h = harness(tmp.path());
    editor_on(&mut h, tmp.path(), "notes.txt", "world\n");
    text(&mut h, "i");
    assert!(h.query_by_label("INSERT").is_some());
    let s = Modifiers::SHIFT;
    h.event(Event::ModifiersChanged(s));
    h.event(Event::Key {
        key: Key::H,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: s,
    });
    h.event(Event::Text("H".into()));
    h.event(Event::Key {
        key: Key::H,
        physical_key: None,
        pressed: false,
        repeat: false,
        modifiers: s,
    });
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.event(Event::ModifiersChanged(s));
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.step();
    h.step();
    assert!(
        !h.state().palette.is_open(),
        "Shift+H, Shift is not Shift, Shift"
    );
    assert!(shown(&h).starts_with("Hworld"), "{:?}", shown(&h));
    assert!(h.query_by_label("INSERT").is_some(), "still inserting");
}

/// R3: Esc closes the palette before the editor sees it (Vim stays in Insert mode), and the
/// editor gets its keys back afterwards. Ctrl+Shift+P opens it too.
#[test]
fn palette_esc_closes_without_the_editor_seeing_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut h = harness(tmp.path());
    editor_on(&mut h, tmp.path(), "notes.txt", "world\n");
    text(&mut h, "i");
    h.key_press_modifiers(Modifiers::CTRL | Modifiers::SHIFT, Key::P);
    h.step();
    h.step();
    assert!(
        h.state().palette.is_open(),
        "Ctrl+Shift+P opens the palette"
    );
    h.key_press(Key::Escape);
    h.step();
    h.step();
    assert!(!h.state().palette.is_open(), "Esc closes it");
    assert!(
        h.query_by_label("INSERT").is_some(),
        "the editor never saw the Esc"
    );
    text(&mut h, "z");
    assert!(shown(&h).starts_with("zworld"), "{:?}", shown(&h));
}

/// R3: while the palette is up, typed keys go to it, never to the editor behind it.
#[test]
fn palette_editor_gets_no_keys_while_open() {
    let tmp = tempfile::tempdir().unwrap();
    let mut h = harness(tmp.path());
    let file = editor_on(&mut h, tmp.path(), "notes.txt", "one\ntwo\n");
    shift_shift(&mut h);
    assert!(h.state().palette.is_open());
    // Normal-mode Vim would delete a line and a char.
    text(&mut h, "ddx");
    h.key_press(Key::ArrowDown);
    h.step();
    assert_eq!(h.state().palette.query(), "ddx");
    let key = (file, EditorView::Disk);
    assert!(!h.state().editors.is_dirty(&key), "buffer untouched");
    h.key_press(Key::Escape);
    h.step();
    h.step();
    assert!(shown(&h).starts_with("one\ntwo"), "{:?}", shown(&h));
    assert!(!h.state().palette.is_open());
}

/// The palette's files follow the file tree's rules: `.git` and gitignored files stay out
/// (a deeper `.gitignore` overrides, `!` re-includes), other dotfiles stay in.
#[test]
fn palette_files_follow_the_tree_ignore_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (path, text) in [
        (".gitignore", "*.log\nbuild/\n"),
        (".git/HEAD", "ref: refs/heads/main\n"),
        (".moon/workspace.yml", "projects: []\n"),
        ("run.log", "x"),
        ("build/out.rs", "x"),
        ("src/lib.rs", "x"),
        ("src/.gitignore", "!keep.log\n"),
        ("src/keep.log", "x"),
        ("src/drop.log", "x"),
    ] {
        let f = root.join(path);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }
    let files = crate::panels::files::tree::walk_files(root, 100);
    let want: Vec<PathBuf> = [
        ".gitignore",
        ".moon/workspace.yml",
        "src/.gitignore",
        "src/keep.log",
        "src/lib.rs",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    assert_eq!(files, want);
    assert_eq!(
        crate::panels::files::tree::walk_files(root, 2).len(),
        2,
        "capped"
    );
}

/// Environment profiles are listed by name; their values never reach the palette.
#[test]
fn palette_lists_env_names_never_values() {
    use void_core::runner::{EnvProfile, EnvValue, Envs, Runs};
    let envs = Envs {
        active: Some("dev".into()),
        profiles: vec![EnvProfile {
            name: "dev".into(),
            vars: [(
                "API_TOKEN".to_string(),
                EnvValue::Value("s3cret-value".into()),
            )]
            .into(),
        }],
    };
    let c = sources::collect(&sources::Lists {
        root: None,
        files: &[],
        recent: &[],
        tabs: Vec::new(),
        tasks: &[],
        runs: &Runs::default(),
        envs: &envs,
        symbols: Vec::new(),
    });
    assert!(c.items.contains(&Item::Env("dev".into())));
    let all = format!("{:?}{:?}", c.items, c.details);
    assert!(
        !all.contains("s3cret") && !all.contains("API_TOKEN"),
        "{all}"
    );
}

/// With an empty query the palette lists recent files, most recent first; Ctrl+Enter opens
/// the pick in another tab.
#[test]
fn palette_recent_files_first_and_ctrl_enter_opens_another_tab() {
    let tmp = tempfile::tempdir().unwrap();
    let mut h = harness(tmp.path());
    let root = workspace(
        &mut h,
        tmp.path(),
        &[("a.rs", "// a\n"), ("b.rs", "// b\n"), ("c.rs", "// c\n")],
    );
    for name in ["a.rs", "b.rs"] {
        h.state_mut()
            .open_editor(root.join(name), EditorView::Disk, None);
        h.step();
        h.step();
    }
    shift_shift(&mut h);
    let titles: Vec<String> = h
        .query_all_by_role(Role::Button)
        .filter_map(|n| n.accesskit_node().label())
        .filter(|l| l.ends_with(".rs"))
        .collect();
    assert_eq!(titles, vec!["b.rs", "a.rs"], "recent files, newest first");
    assert!(h.query_by_label("Recent files").is_some());
    // Down to a.rs, Ctrl+Enter: a second tab on it.
    h.key_press(Key::ArrowDown);
    h.step();
    h.key_press_modifiers(Modifiers::CTRL, Key::Enter);
    h.step();
    h.step();
    assert!(!h.state().palette.is_open());
    let a = root.join("a.rs");
    let on_a = editor_paths(h.state()).iter().filter(|p| **p == a).count();
    assert_eq!(on_a, 2, "{:?}", editor_paths(h.state()));
}

/// Commands run, environment profiles switch, from the palette.
#[test]
fn palette_runs_commands_and_switches_env() {
    use void_core::runner::{EnvProfile, Envs};
    let tmp = tempfile::tempdir().unwrap();
    let mut h = harness(tmp.path());
    workspace(&mut h, tmp.path(), &[("a.rs", "// a\n")]);
    let profile = |name: &str| EnvProfile {
        name: name.into(),
        vars: Default::default(),
    };
    h.state()
        .engine
        .send(void_core::engine::Command::SaveEnvs(Envs {
            active: Some("dev".into()),
            profiles: vec![profile("dev"), profile("prod")],
        }));
    until(&mut h, "profiles", |a| a.state.envs.profiles.len() == 2);
    shift_shift(&mut h);
    text(&mut h, "$prod");
    h.key_press(Key::Enter);
    h.step();
    until(&mut h, "prod active", |a| {
        a.state.envs.active.as_deref() == Some("prod")
    });

    let chats = |a: &VoidApp| {
        a.dock
            .iter_all_tabs()
            .filter(|(_, t)| matches!(t, Tab::Chat { .. }))
            .count()
    };
    let before = chats(h.state());
    shift_shift(&mut h);
    text(&mut h, ">new chat tab");
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert_eq!(chats(h.state()), before + 1);
}

/// A file created after the walk shows up the next time the palette opens (the watcher marks
/// the list stale; opening walks again).
#[test]
fn palette_files_follow_the_watcher() {
    let tmp = tempfile::tempdir().unwrap();
    let mut h = harness(tmp.path());
    let root = workspace(&mut h, tmp.path(), &[("a.rs", "// a\n")]);
    until(&mut h, "watcher", |a| a.disk.ready());
    let new = root.join("fresh_module.rs");
    std::fs::write(&new, "// new\n").unwrap();
    until(&mut h, "watcher saw it", |a| a.disk.saw(&new));
    shift_shift(&mut h);
    until(&mut h, "walked again", |a| {
        a.palette
            .files
            .files()
            .contains(&PathBuf::from("fresh_module.rs"))
    });
    text(&mut h, "fresh_mod");
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert_eq!(editor_paths(h.state()), vec![new]);
}

/// Recent files are files on disk: a chat-view tab is listed as that open tab (picking it
/// focuses it), never as a recent file that would reopen as the Disk view.
#[test]
fn palette_recent_skips_chat_views() {
    let tmp = tempfile::tempdir().unwrap();
    let mut h = harness(tmp.path());
    let a = tmp.path().join("a.rs");
    let b = tmp.path().join("b.rs");
    std::fs::write(&a, "// a\n").unwrap();
    std::fs::write(&b, "// b\n").unwrap();
    h.state_mut().open_editor(a.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    let chat = EditorView::Chat(void_core::model::ConversationId::new());
    h.state_mut().open_editor(b.clone(), chat.clone(), None);
    h.step();
    h.step();
    let tabs = |app: &VoidApp| -> Vec<(PathBuf, EditorView)> {
        app.dock
            .iter_all_tabs()
            .filter_map(|(_, t)| match t {
                Tab::Editor { path, view, .. } => Some((path.clone(), view.clone())),
                _ => None,
            })
            .collect()
    };
    let before = tabs(h.state());
    assert_eq!(before.len(), 2);
    assert_eq!(h.state().palette.recent.paths(), std::slice::from_ref(&a));
    shift_shift(&mut h);
    assert!(
        h.query_by_label("b.rs").is_none(),
        "no recent entry for b.rs"
    );
    assert!(
        h.query_by_label("b.rs · chat").is_some(),
        "the chat tab is listed"
    );
    // The first row (the newest recent file, a.rs) opens nothing new.
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert_eq!(tabs(h.state()), before);
}

/// The whole runner path through the real app and engine: a saved run config "check" and an
/// env profile, started from the palette with `!check`, run by real moon on this repo.
/// Not part of the normal gate: it runs `moon run workspace:format` and `workspace:lint`
/// (clippy; minutes) and needs moon on PATH (proto's shim). Run with:
/// `cargo test -p void-gui live_palette_runs_check_config -- --ignored --nocapture`
#[test]
#[ignore = "needs moon (proto); runs cargo fmt and clippy on void.ai (minutes)"]
fn live_palette_runs_check_config() {
    use void_core::engine::Command;
    use void_core::runner::{
        EnvProfile, EnvValue, Envs, Group, OnFailure, RunConfig, Runs, Step, StepConfig,
    };

    if std::process::Command::new("moon")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("moon is not on PATH; skipping");
        return;
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    // Config and data (runs.toml, env.toml) live in a temp dir, never the user's own.
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, data) = (tmp.path().join("config.toml"), tmp.path().join("data"));
    let open = Some(repo.clone());
    let mut h = Harness::builder()
        .with_size([1200.0, 800.0])
        .with_max_steps(10)
        .with_accessibility_check(false)
        .build_eframe(move |cc| VoidApp::new(cc, open, cfg, data));
    let want = repo.clone();
    until(&mut h, "repo workspace and its runs", |a| {
        a.state.workspace.root.as_deref() == Some(want.as_path()) && a.state.runner_loaded
    });

    // Saved through the engine's SaveRuns/SaveEnvs (what the Runs panel's editor sends);
    // driving the editor's widgets adds nothing the panel's own tests don't cover.
    let step = |step| StepConfig {
        step,
        on_failure: OnFailure::Stop,
    };
    let check = RunConfig::new(
        "check",
        vec![
            step(Step::Moon {
                target: "workspace:format".into(),
                args: vec![],
            }),
            step(Step::Moon {
                target: "workspace:lint".into(),
                args: vec![],
            }),
            // The label never contains the value, so finding it in the output means the
            // step's process saw the profile's variable.
            step(Step::Shell {
                command: "echo \"level=$RUST_LOG\"".into(),
            }),
        ],
    );
    h.state().engine.send(Command::SaveRuns(Runs {
        groups: vec![Group {
            name: "g".into(),
            configs: vec![check],
        }],
    }));
    h.state().engine.send(Command::SaveEnvs(Envs {
        active: Some("dev".into()),
        profiles: vec![EnvProfile {
            name: "dev".into(),
            vars: [("RUST_LOG".to_string(), EnvValue::Value("debug".into()))].into(),
        }],
    }));
    until(&mut h, "runs and envs saved", |a| {
        a.state.runs.groups.len() == 1 && a.state.envs.profiles.len() == 1
    });

    shift_shift(&mut h);
    assert!(h.state().palette.is_open());
    text(&mut h, "!check");
    h.key_press(Key::Enter);
    h.step();
    assert!(!h.state().palette.is_open(), "Enter closes the palette");

    // Real moon, real clippy: generous bound.
    let deadline = Instant::now() + Duration::from_secs(900);
    let run = loop {
        h.step();
        if let Some((id, _)) = h
            .state()
            .state
            .run_views
            .iter()
            .find(|(_, v)| v.finished.is_some())
        {
            break *id;
        }
        assert!(Instant::now() < deadline, "the run did not finish in time");
        std::thread::sleep(Duration::from_millis(50));
    };
    let v = &h.state().state.run_views[&run];
    let out = v.term.plain_text();
    let fin = v.finished.as_ref().unwrap();
    assert_eq!(v.name, "check");
    assert!(fin.ok, "run failed: {:?}\n{out}", fin.error);
    assert_eq!(v.steps, 3);
    assert!(
        out.contains("level=debug"),
        "the dev profile's variable reached the shell step:\n{out}"
    );
    assert!(
        h.state().dock.find_tab(&Tab::RunOutput { run }).is_some(),
        "the run has an output tab"
    );
}
