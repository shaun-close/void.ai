//! Disk and chat views of a file, and the pending-changes banner (egui_kittest, offline).

use std::path::{Path, PathBuf};

use egui::Key;
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use void_core::engine::{Command, Event};
use void_core::model::{Conversation, ConversationId};
use void_core::overlay::Overlay;
use void_core::store::Store;

use super::tests::{shown, text, until};
use crate::app::VoidApp;
use crate::layout::{EditorView, Tab};

const CHAT: &str = "Tidy the parser";

/// A folder workspace whose `a.txt` says "disk text", and a chat ("Tidy the parser") whose
/// pending changes say "void's text" there. The app opens the folder.
struct Rig {
    _t: tempfile::TempDir,
    file: PathBuf,
    conv: ConversationId,
    overlay: Overlay,
    h: Harness<'static, VoidApp>,
}

fn rig() -> Rig {
    rig_with(&[CHAT])
}

/// Like [`rig`], with one chat per title, each with its own pending version of `a.txt`
/// (`conv` and `overlay` are the first one's).
fn rig_with(titles: &[&str]) -> Rig {
    let t = tempfile::tempdir().unwrap();
    let proj = t.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir(&proj).unwrap();
    let file = proj.join("a.txt");
    std::fs::write(&file, "disk text\n").unwrap();
    let data = t.path().join("data");
    let store = Store::new(&data);
    let ws = store.workspace_for(&proj).unwrap();
    let mut chats = Vec::new();
    for title in titles {
        let mut c = Conversation::new(ws.id.clone());
        c.title = (*title).into();
        store.save(&c).unwrap();
        let overlay = Overlay::open(&store.overlay_dir(&ws.id, &c.id), &proj).unwrap();
        overlay.write(Path::new("a.txt"), b"void's text\n").unwrap();
        chats.push((c.id, overlay));
    }
    let (conv, overlay) = chats.swap_remove(0);
    let cfg = t.path().join("config.toml");
    let open = Some(proj.clone());
    let mut h = Harness::builder()
        .with_size([1200.0, 800.0])
        .with_max_steps(10)
        .with_accessibility_check(false)
        .build_eframe(move |cc| VoidApp::new(cc, open, cfg, data));
    let n = titles.len();
    until(&mut h, "pending changes listed on open", |a| {
        a.state.changes.values().filter(|c| !c.is_empty()).count() == n
    });
    h.step();
    Rig {
        _t: t,
        file,
        conv,
        overlay,
        h,
    }
}

fn editor_view(app: &VoidApp) -> Option<EditorView> {
    app.dock.iter_all_tabs().find_map(|(_, t)| match t {
        Tab::Editor { view, .. } => Some(view.clone()),
        _ => None,
    })
}

/// What the tab's view switch shows.
fn view_switch(h: &Harness<'_, VoidApp>) -> String {
    h.get_by_role(Role::ComboBox).value().unwrap_or_default()
}

/// `:w<CR>`.
fn write(h: &mut Harness<'_, VoidApp>) {
    text(h, ":");
    text(h, "w");
    h.key_press(Key::Enter);
    h.step();
}

fn steps(h: &mut Harness<'_, VoidApp>, n: usize) {
    for _ in 0..n {
        h.step();
    }
}

#[test]
fn disk_tab_shows_pending_banner() {
    let mut r = rig();
    r.h.state_mut()
        .open_editor(r.file.clone(), EditorView::Disk, None);
    steps(&mut r.h, 2);
    assert!(shown(&r.h).contains("disk text"), "{:?}", shown(&r.h));
    assert!(
        r.h.query_by_label_contains(&format!("void has pending changes in {CHAT}"))
            .is_some()
    );
    // The view switch says which version the tab shows.
    assert!(
        view_switch(&r.h).contains("Disk"),
        "{:?}",
        view_switch(&r.h)
    );

    // Review: the panel comes forward on that chat and file.
    r.h.get_by_role_and_label(Role::Button, "Review").click();
    steps(&mut r.h, 2);
    assert_eq!(r.h.state().review.conversation, Some(r.conv));
    assert_eq!(r.h.state().review.selected, Some(PathBuf::from("a.txt")));

    // Unsaved Disk edits survive switching the tab to void's version.
    r.h.state_mut()
        .open_editor(r.file.clone(), EditorView::Disk, None);
    steps(&mut r.h, 2);
    text(&mut r.h, "imine ");
    r.h.key_press(Key::Escape);
    r.h.step();
    r.h.get_by_label_contains("View void's version").click();
    steps(&mut r.h, 3);
    assert_eq!(editor_view(r.h.state()), Some(EditorView::Chat(r.conv)));
    assert!(shown(&r.h).contains("void's text"), "{:?}", shown(&r.h));
    let disk = (r.file.clone(), EditorView::Disk);
    assert!(
        r.h.state().editors.is_dirty(&disk),
        "the Disk buffer's edits are kept"
    );
    assert_eq!(std::fs::read_to_string(&r.file).unwrap(), "disk text\n");
}

#[test]
fn chat_view_tab_shows_overlay_content() {
    let mut r = rig();
    let key = (r.file.clone(), EditorView::Chat(r.conv));
    r.h.state_mut()
        .open_editor(r.file.clone(), EditorView::Chat(r.conv), None);
    steps(&mut r.h, 2);
    assert!(shown(&r.h).contains("void's text"), "{:?}", shown(&r.h));
    assert!(!shown(&r.h).contains("disk text"));
    assert!(r.h.query_by_label_contains("a.txt · chat").is_some());
    assert!(
        view_switch(&r.h).contains(&format!("void's version · {CHAT}")),
        "the tab says whose version it shows: {:?}",
        view_switch(&r.h)
    );
    // No "pending changes" banner on void's own version.
    assert!(
        r.h.query_by_label_contains("void has pending changes")
            .is_none()
    );

    // :w writes the chat's pending changes, never the real file.
    text(&mut r.h, "ifresh ");
    r.h.key_press(Key::Escape);
    r.h.step();
    write(&mut r.h);
    until(&mut r.h, "saved to the overlay", |a| {
        !a.editors.is_dirty(&key)
    });
    assert_eq!(
        r.overlay.read(Path::new("a.txt")).unwrap(),
        b"fresh void's text\n"
    );
    assert_eq!(std::fs::read_to_string(&r.file).unwrap(), "disk text\n");

    // While the chat's turn runs, a save is refused and the buffer stays dirty.
    r.h.state_mut().state.active_turns.insert(r.conv);
    text(&mut r.h, "ix");
    r.h.key_press(Key::Escape);
    r.h.step();
    write(&mut r.h);
    steps(&mut r.h, 3);
    assert!(r.h.state().editors.is_dirty(&key));
    assert!(
        r.h.query_by_label_contains("void is working in this chat")
            .is_some()
    );
    assert_eq!(
        r.overlay.read(Path::new("a.txt")).unwrap(),
        b"fresh void's text\n"
    );
}

#[test]
fn chat_view_follows_void_writes() {
    let mut r = rig();
    let key = (r.file.clone(), EditorView::Chat(r.conv));
    r.h.state_mut()
        .open_editor(r.file.clone(), EditorView::Chat(r.conv), None);
    steps(&mut r.h, 2);
    // The agent writes the file again (a later turn): a clean chat view reloads.
    r.overlay
        .write(Path::new("a.txt"), b"void's second try\n")
        .unwrap();
    r.h.state().engine.send(Command::ListChanges(r.conv));
    until(&mut r.h, "reloaded from the overlay", |a| {
        a.editors
            .doc(&key)
            .is_some_and(|d| d.text() == "void's second try")
    });
    // With unsaved edits it asks instead, and a save doesn't overwrite void's write.
    text(&mut r.h, "imine ");
    r.h.key_press(Key::Escape);
    r.h.step();
    r.overlay
        .write(Path::new("a.txt"), b"void's third try\n")
        .unwrap();
    r.h.state().engine.send(Command::ListChanges(r.conv));
    until(&mut r.h, "flagged", |a| {
        a.editors.doc(&key).is_some_and(|d| d.disk_changed())
    });
    steps(&mut r.h, 2);
    assert!(
        r.h.query_by_label_contains("void changed its version")
            .is_some()
    );
    write(&mut r.h);
    steps(&mut r.h, 3);
    assert!(r.h.query_by_label_contains("since it was read").is_some());
    assert_eq!(
        r.overlay.read(Path::new("a.txt")).unwrap(),
        b"void's third try\n"
    );
    assert!(r.h.state().editors.is_dirty(&key));
}

#[test]
fn several_chats_collapse_into_a_menu() {
    let mut r = rig_with(&[CHAT, "Second opinion"]);
    r.h.state_mut()
        .open_editor(r.file.clone(), EditorView::Disk, None);
    steps(&mut r.h, 2);
    assert!(
        r.h.query_by_label_contains("2 chats have pending changes")
            .is_some()
    );
    assert!(
        r.h.query_by_label_contains("void has pending changes in")
            .is_none()
    );
    r.h.get_by_label_contains("Pick a chat").click();
    steps(&mut r.h, 2);
    let views: Vec<_> = r.h.query_all_by_label("View void's version").collect();
    assert_eq!(views.len(), 2, "one per chat");
    views[0].click();
    steps(&mut r.h, 3);
    assert_eq!(editor_view(r.h.state()), Some(EditorView::Chat(r.conv)));
}

/// Make the chat's layer unwritable (the engine's write fails), or writable again.
fn lock_overlay(r: &Rig, locked: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if locked { 0o555 } else { 0o755 };
    std::fs::set_permissions(r.overlay.upper(), std::fs::Permissions::from_mode(mode)).unwrap();
}

fn message(app: &VoidApp, key: &crate::editor::DocKey) -> String {
    app.editors
        .doc(key)
        .and_then(|d| d.core.message.clone())
        .map(|m| m.text)
        .unwrap_or_default()
}

#[test]
fn chat_view_wq_closes_after_save() {
    let mut r = rig();
    let key = (r.file.clone(), EditorView::Chat(r.conv));
    r.h.state_mut()
        .open_editor(r.file.clone(), EditorView::Chat(r.conv), None);
    steps(&mut r.h, 2);
    text(&mut r.h, "imine ");
    r.h.key_press(Key::Escape);
    r.h.step();
    text(&mut r.h, ":");
    text(&mut r.h, "wq");
    r.h.key_press(Key::Enter);
    r.h.step();
    assert!(
        !message(r.h.state(), &key).contains("E37"),
        "{}",
        message(r.h.state(), &key)
    );
    until(&mut r.h, "tab closed after the save", |a| {
        editor_view(a).is_none()
    });
    assert_eq!(
        r.overlay.read(Path::new("a.txt")).unwrap(),
        b"mine void's text\n"
    );

    // When the engine can't write, the tab stays open, dirty, and says why.
    r.h.state_mut()
        .open_editor(r.file.clone(), EditorView::Chat(r.conv), None);
    steps(&mut r.h, 2);
    text(&mut r.h, "ix");
    r.h.key_press(Key::Escape);
    r.h.step();
    lock_overlay(&r, true);
    text(&mut r.h, ":");
    text(&mut r.h, "wq");
    r.h.key_press(Key::Enter);
    until(&mut r.h, "save failed", |a| {
        message(a, &key).contains("Couldn't save")
    });
    steps(&mut r.h, 3);
    lock_overlay(&r, false);
    assert_eq!(editor_view(r.h.state()), Some(EditorView::Chat(r.conv)));
    assert!(r.h.state().editors.is_dirty(&key));
}

#[test]
fn second_chat_save_refused_keeps_doc_dirty() {
    let mut r = rig();
    let key = (r.file.clone(), EditorView::Chat(r.conv));
    r.h.state_mut()
        .open_editor(r.file.clone(), EditorView::Chat(r.conv), None);
    steps(&mut r.h, 2);
    let app = r.h.state_mut();
    app.editors.doc_mut(&key).unwrap().core.replace(0..0, "a");
    app.editors.save(&key).unwrap();
    app.editors.doc_mut(&key).unwrap().core.replace(0..0, "b");
    app.editors.save(&key).unwrap();
    // Both in flight; the engine writes the first and refuses the second.
    let sent = std::mem::take(&mut app.editors.overlay_saves);
    assert_eq!(sent.len(), 2);
    assert_ne!(sent[0].request, sent[1].request);
    app.state.apply(Event::OverlaySaved {
        request: sent[0].request,
        conversation: r.conv,
        path: r.file.clone(),
    });
    app.state.apply(Event::OverlaySaveFailed {
        request: sent[1].request,
        conversation: r.conv,
        path: r.file.clone(),
        error: "refused for the test".into(),
    });
    steps(&mut r.h, 2);
    assert!(
        r.h.state().editors.is_dirty(&key),
        "the second version never landed"
    );
    assert!(message(r.h.state(), &key).contains("refused for the test"));
    // An error for some other request leaves a later save alone. (Forced: the synthetic
    // answers above never wrote the layer, so it looks moved.)
    let app = r.h.state_mut();
    app.editors.save_chat(&key, true).unwrap();
    let sent = std::mem::take(&mut app.editors.overlay_saves);
    app.state.apply(Event::OverlaySaveFailed {
        request: sent[0].request + 100,
        conversation: r.conv,
        path: r.file.clone(),
        error: "not yours".into(),
    });
    app.state.apply(Event::OverlaySaved {
        request: sent[0].request,
        conversation: r.conv,
        path: r.file.clone(),
    });
    steps(&mut r.h, 2);
    assert!(!r.h.state().editors.is_dirty(&key));
}

#[test]
fn quit_waits_for_chat_saves() {
    let mut r = rig();
    let key = (r.file.clone(), EditorView::Chat(r.conv));
    r.h.state_mut()
        .open_editor(r.file.clone(), EditorView::Chat(r.conv), None);
    steps(&mut r.h, 2);
    text(&mut r.h, "ix");
    r.h.key_press(Key::Escape);
    r.h.step();
    lock_overlay(&r, true);
    let close = |h: &mut Harness<'_, VoidApp>| {
        h.input_mut()
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .events
            .push(egui::ViewportEvent::Close);
        h.step();
        h.step();
    };
    close(&mut r.h);
    assert!(
        r.h.query_by_label_contains("Save changes before quitting")
            .is_some()
    );
    r.h.get_by_label("Save all").click();
    until(&mut r.h, "save failed", |a| {
        message(a, &key).contains("Couldn't save")
    });
    steps(&mut r.h, 3);
    assert!(
        !r.h.state().quit_anyway,
        "a refused save keeps the app open"
    );
    assert!(r.h.state().editors.is_dirty(&key));
    assert!(
        r.h.query_by_label_contains("Save changes before quitting")
            .is_some(),
        "the dialog stays up with the error"
    );
    lock_overlay(&r, false);
    r.h.get_by_label("Save all").click();
    until(&mut r.h, "quit after the save", |a| a.quit_anyway);
    assert_eq!(
        r.overlay.read(Path::new("a.txt")).unwrap(),
        b"xvoid's text\n"
    );
}
