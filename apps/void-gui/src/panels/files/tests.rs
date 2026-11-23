//! The Files panel driven headlessly through the real app (egui_kittest, offline).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{Event, Key};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};

use crate::app::VoidApp;
use crate::layout::{self, EditorView, Tab};

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
        .with_size([1200.0, 800.0])
        .with_max_steps(10)
        .with_accessibility_check(false)
        .build_eframe(move |cc| VoidApp::new(cc, open, cfg, data));
    let want = ws.to_path_buf();
    until(&mut h, "workspace", |a| {
        a.state.workspace.root.as_deref() == Some(want.as_path())
    });
    layout::ensure_tab(&mut h.state_mut().dock, Tab::Files);
    h.step();
    h.step();
    h
}

fn until(h: &mut Harness<'_, VoidApp>, what: &str, done: impl Fn(&VoidApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
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

/// (name, is_dir, ignored) of a folder's rows, in display order.
fn rows(h: &mut Harness<'_, VoidApp>, dir: &Path) -> Vec<(String, bool, bool)> {
    let tree = h
        .state_mut()
        .files
        .tree
        .as_mut()
        .expect("tree for the workspace");
    tree.children(dir)
        .iter()
        .map(|e| (e.name.clone(), e.is_dir, e.ignored))
        .collect()
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

fn type_text(h: &mut Harness<'_, VoidApp>, s: &str) {
    h.event(Event::Text(s.into()));
    steps(h, 2);
}

fn enter(h: &mut Harness<'_, VoidApp>) {
    h.key_press(Key::Enter);
    steps(h, 3);
}

/// Right-click a row and pick a context-menu entry (menu entries start with their icon,
/// which tells them apart from the header's buttons of the same name).
fn menu(h: &mut Harness<'_, VoidApp>, row: &str, entry: &str) {
    h.get_by_label(row).click_secondary();
    steps(h, 2);
    let item = h
        .query_all_by_label_contains(entry)
        .find(|n| {
            n.accesskit_node()
                .label()
                .is_some_and(|l| l != entry && l.ends_with(entry))
        })
        .unwrap_or_else(|| panic!("no menu entry {entry:?}"));
    item.click();
    steps(h, 3);
}

#[test]
fn tree_lists_and_dims_ignored() {
    let (t, ws) = workspace();
    for d in [".git", "src/gen", "target/debug", "Zeta"] {
        fs::create_dir_all(ws.join(d)).unwrap();
    }
    fs::write(ws.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(ws.join(".gitignore"), "target/\n*.log\n").unwrap();
    fs::write(ws.join("src/.gitignore"), "gen/\n").unwrap();
    fs::write(ws.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(ws.join("target/debug/app"), "bin").unwrap();
    for f in ["a.log", "README.md", "beta.txt"] {
        fs::write(ws.join(f), "x").unwrap();
    }
    let mut h = harness(t.path(), &ws);

    // Folders first, then files, each sorted ignoring case; .git hidden; ignored ones dimmed.
    let s = |n: &str, d: bool, i: bool| (n.to_string(), d, i);
    assert_eq!(
        rows(&mut h, &ws),
        vec![
            s("src", true, false),
            s("target", true, true),
            s("Zeta", true, false),
            s(".gitignore", false, false),
            s("a.log", false, true),
            s("beta.txt", false, false),
            s("README.md", false, false),
        ]
    );
    // Lazy: folders are read when they're opened, not before.
    let tree = h.state().files.tree.as_ref().unwrap();
    assert!(!tree.is_loaded(&ws.join("src")));
    assert!(!tree.is_loaded(&ws.join("target")));
    assert!(h.query_by_label(".git").is_none(), "hidden");
    assert!(h.query_by_label("main.rs").is_none(), "src still closed");

    // A nested .gitignore applies inside its folder; an ignored folder's contents are too.
    assert_eq!(
        rows(&mut h, &ws.join("src")),
        vec![
            s("gen", true, true),
            s(".gitignore", false, false),
            s("main.rs", false, false)
        ]
    );
    assert_eq!(
        rows(&mut h, &ws.join("target")),
        vec![s("debug", true, true)]
    );

    // Clicking a folder opens it; clicking a file opens it in an editor tab.
    h.get_by_label("src").click();
    steps(&mut h, 3);
    assert!(
        h.state()
            .files
            .tree
            .as_ref()
            .unwrap()
            .is_expanded(&ws.join("src"))
    );
    h.get_by_label("main.rs").click();
    steps(&mut h, 3);
    assert_eq!(editor_paths(h.state()), vec![ws.join("src/main.rs")]);
    // A second click focuses that tab instead of opening another.
    layout::ensure_tab(&mut h.state_mut().dock, Tab::Files);
    steps(&mut h, 2);
    h.get_by_label("main.rs").click();
    steps(&mut h, 3);
    assert_eq!(editor_paths(h.state()).len(), 1);

    // The tree follows the disk (watcher): a new file shows up in the open folder.
    until(&mut h, "watcher", |a| a.disk.ready());
    fs::write(ws.join("src/lib.rs"), "").unwrap();
    layout::ensure_tab(&mut h.state_mut().dock, Tab::Files);
    let src = ws.join("src");
    until(&mut h, "lib.rs listed", |a| {
        a.files.tree.as_ref().is_some_and(|t| {
            t.cached(&src)
                .is_some_and(|c| c.iter().any(|e| e.name == "lib.rs"))
        })
    });
    steps(&mut h, 2);
    assert!(h.query_by_label("lib.rs").is_some());
}

#[test]
fn tree_new_rename_delete() {
    let (t, ws) = workspace();
    fs::create_dir(ws.join("src")).unwrap();
    fs::write(ws.join("notes.txt"), "hello\n").unwrap();
    fs::write(ws.join("other.txt"), "x").unwrap();
    let mut h = harness(t.path(), &ws);

    // An open, edited file: renaming it moves the tab and keeps the edits.
    let notes = ws.join("notes.txt");
    h.state_mut()
        .open_editor(notes.clone(), EditorView::Disk, None);
    steps(&mut h, 2);
    h.state_mut()
        .editors
        .doc_mut(&(notes.clone(), EditorView::Disk))
        .unwrap()
        .core
        .replace(0..0, "edit ");
    layout::ensure_tab(&mut h.state_mut().dock, Tab::Files);
    steps(&mut h, 2);

    menu(&mut h, "notes.txt", "Rename");
    // The name without its extension is selected, so typing replaces it.
    type_text(&mut h, "todo");
    enter(&mut h);
    let todo = ws.join("todo.txt");
    assert!(todo.exists() && !notes.exists());
    assert_eq!(fs::read_to_string(&todo).unwrap(), "hello\n", "not saved");
    assert_eq!(editor_paths(h.state()), vec![todo.clone()]);
    let key = (todo.clone(), EditorView::Disk);
    assert!(h.state().editors.is_dirty(&key), "edits followed the file");
    assert!(h.state().editors.doc(&(notes, EditorView::Disk)).is_none());
    assert!(h.query_by_label_contains("todo.txt ●").is_some());

    // Names with a slash, or that are taken, are refused and the field stays open.
    menu(&mut h, "other.txt", "Rename");
    type_text(&mut h, "a/b");
    enter(&mut h);
    assert!(h.query_by_label_contains("can't contain").is_some());
    assert!(ws.join("other.txt").exists());
    h.key_press(Key::Escape);
    steps(&mut h, 2);
    assert!(
        h.query_by_label_contains("can't contain").is_none(),
        "Esc cancels"
    );
    menu(&mut h, "other.txt", "Rename");
    type_text(&mut h, "todo");
    enter(&mut h);
    assert!(h.query_by_label_contains("already exists").is_some());
    assert_eq!(fs::read_to_string(&todo).unwrap(), "hello\n");
    h.key_press(Key::Escape);
    steps(&mut h, 2);

    // New file inside a folder: created empty, the folder opens, the file opens in a tab.
    menu(&mut h, "src", "New file");
    type_text(&mut h, "lib.rs");
    enter(&mut h);
    assert_eq!(fs::read_to_string(ws.join("src/lib.rs")).unwrap(), "");
    assert!(editor_paths(h.state()).contains(&ws.join("src/lib.rs")));
    // New folder at the top, from the panel's header.
    layout::ensure_tab(&mut h.state_mut().dock, Tab::Files);
    steps(&mut h, 2);
    h.get_by_label("New folder").click();
    steps(&mut h, 2);
    type_text(&mut h, "docs");
    enter(&mut h);
    assert!(ws.join("docs").is_dir());

    // Delete asks first (a modal: editor keys are blocked), then moves to the trash.
    let trash = t.path().join("trash");
    fs::create_dir(&trash).unwrap();
    let bin = trash.clone();
    h.state_mut().files.trash = Box::new(move |p: &Path| {
        fs::rename(p, bin.join(p.file_name().unwrap())).map_err(|e| e.to_string())
    });
    menu(&mut h, "other.txt", "Delete…");
    assert!(
        h.query_by_label_contains("Move other.txt to the trash?")
            .is_some()
    );
    assert!(h.state().editors.modal_open());
    h.get_by_label("Cancel").click();
    steps(&mut h, 2);
    assert!(ws.join("other.txt").exists(), "cancel keeps it");
    menu(&mut h, "other.txt", "Delete…");
    h.get_by_label("Move to Trash").click();
    steps(&mut h, 3);
    assert!(!ws.join("other.txt").exists());
    assert!(trash.join("other.txt").exists(), "trashed, not deleted");
    assert!(!h.state().editors.modal_open());

    // No trash available: the error is reported and nothing is deleted.
    h.state_mut().files.trash = Box::new(|_: &Path| Err("no trash can here".into()));
    menu(&mut h, "docs", "Delete…");
    h.get_by_label("Move to Trash").click();
    steps(&mut h, 3);
    assert!(ws.join("docs").is_dir());
    let status = h
        .state()
        .state
        .status
        .clone()
        .expect("an error in the status bar");
    assert!(
        status.error && status.text.contains("no trash can here"),
        "{status:?}"
    );
}
