//! The Files panel: the workspace as a tree. A click opens a folder or opens the file in an
//! editor tab (focusing the tab when it's already open; there are no preview tabs). The
//! context menu creates, renames, trashes, copies paths and reveals in the file manager.
//! Folders are read only when opened; watcher events re-read the folders they touch.

use std::path::{Path, PathBuf};

use egui::accesskit::Role;
use egui::text::{CCursor, CCursorRange};
use egui::{
    Align, CornerRadius, Key, Layout, RichText, ScrollArea, Sense, TextEdit, Ui, WidgetInfo, vec2,
};

use super::{Action, Panels};
use crate::state::UiState;
use crate::theme::{self, BOLD, icon};

#[cfg(test)]
mod tests;
pub mod tree;

pub use tree::{Entry, Tree};

/// Moves a path to the trash. Swapped out in tests, which must not fill the user's trash.
pub type Trash = Box<dyn Fn(&Path) -> Result<(), String>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditKind {
    NewFile,
    NewFolder,
    Rename(PathBuf),
}

/// A name being typed in the tree: a new entry in `dir`, or a rename.
#[derive(Debug, Clone)]
struct InlineEdit {
    kind: EditKind,
    dir: PathBuf,
    name: String,
    error: Option<String>,
    /// Focus (and the initial selection) has been set up.
    focused: bool,
}

pub struct FilesUi {
    /// The tree of the open workspace folder (none in Scratch).
    pub tree: Option<Tree>,
    pub trash: Trash,
    edit: Option<InlineEdit>,
    /// A delete waiting for the user's yes (the app draws the dialog).
    pub confirm_delete: Option<PathBuf>,
    /// Scroll this row into view on the next frame.
    scroll_to: Option<PathBuf>,
}

impl Default for FilesUi {
    fn default() -> Self {
        FilesUi {
            tree: None,
            trash: Box::new(tree::system_trash),
            edit: None,
            confirm_delete: None,
            scroll_to: None,
        }
    }
}

impl FilesUi {
    /// Follow the workspace: a new root gets a fresh tree.
    pub fn set_root(&mut self, root: Option<&Path>) {
        if self.tree.as_ref().map(Tree::root) != root {
            self.tree = root.map(|r| Tree::new(r.to_path_buf()));
            self.edit = None;
            self.confirm_delete = None;
            self.scroll_to = None;
        }
    }

    /// Paths changed on disk: re-read the folders showing them.
    pub fn changed(&mut self, paths: &[PathBuf]) {
        if let Some(t) = &mut self.tree {
            for p in paths {
                t.invalidate(p);
            }
        }
    }

    /// Open the folders above `path` and scroll to it.
    pub fn reveal(&mut self, path: &Path) -> bool {
        let shown = self.tree.as_mut().is_some_and(|t| t.reveal(path));
        if shown {
            self.scroll_to = Some(path.to_path_buf());
        }
        shown
    }

    pub fn start_new(&mut self, dir: PathBuf, folder: bool) {
        if let Some(t) = &mut self.tree {
            t.set_expanded(&dir, true);
        }
        self.edit = Some(InlineEdit {
            kind: if folder {
                EditKind::NewFolder
            } else {
                EditKind::NewFile
            },
            dir,
            name: String::new(),
            error: None,
            focused: false,
        });
    }

    pub fn start_rename(&mut self, path: PathBuf) {
        let Some(dir) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.edit = Some(InlineEdit {
            kind: EditKind::Rename(path),
            dir,
            name,
            error: None,
            focused: false,
        });
    }

    /// The typed name: create or rename. On an error the field stays open and says why.
    /// `open(path)`: an editor buffer is open at or under `path`.
    fn commit(&mut self, actions: &mut Vec<Action>, open: impl Fn(&Path) -> bool) {
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        let result = match &edit.kind {
            EditKind::NewFile => tree::new_file(&edit.dir, &edit.name),
            EditKind::NewFolder => tree::new_folder(&edit.dir, &edit.name),
            // Moving onto a path an open buffer holds (an unsaved `:e notes.md`, say) would
            // replace that buffer. Names taken on disk are reported first.
            EditKind::Rename(from) => {
                tree::check_name(&edit.dir, &edit.name, Some(from)).and_then(|target| {
                    if target != *from && open(&target) {
                        Err(format!(
                            "\u{201c}{}\u{201d} is open in an editor; close it first",
                            edit.name.trim()
                        ))
                    } else {
                        tree::rename(from, &edit.name)
                    }
                })
            }
        };
        match result {
            Ok(path) => {
                let edit = self.edit.take().unwrap();
                if let Some(t) = &mut self.tree {
                    t.invalidate(&path);
                    if let EditKind::Rename(from) = &edit.kind {
                        t.invalidate(from);
                        if t.is_expanded(from) {
                            t.set_expanded(from, false);
                            t.set_expanded(&path, true);
                        }
                    }
                }
                match edit.kind {
                    EditKind::NewFile => actions.push(Action::OpenFile(path.clone())),
                    EditKind::Rename(from) if from != path => actions.push(Action::Renamed {
                        from,
                        to: path.clone(),
                    }),
                    _ => {}
                }
                self.scroll_to = Some(path);
            }
            Err(e) => {
                edit.error = Some(e);
                edit.focused = false;
            }
        }
    }
}

/// What a row asked for this frame (applied once the rows are drawn).
enum Do {
    Open(PathBuf),
    Toggle(PathBuf),
    New(PathBuf, bool),
    Rename(PathBuf),
    Delete(PathBuf),
    Copy(String),
    Reveal(PathBuf),
}

enum Row {
    Entry {
        depth: usize,
        entry: Entry,
        open: bool,
    },
    Edit(usize),
}

pub fn show(ui: &mut Ui, p: &mut Panels) {
    let pal = theme::palette_of(ui);
    let root = p.state.workspace.root.clone();
    p.files.set_root(root.as_deref());
    let Some(root) = root else {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(theme::ico(icon::FOLDER).size(28.0).color(pal.text_faint));
            ui.label(RichText::new("No folder open").color(pal.text_dim));
            ui.label(
                RichText::new("File → Open Folder… shows its files here")
                    .small()
                    .color(pal.text_faint),
            );
        });
        return;
    };
    let mut todo = Vec::new();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(&p.state.workspace.name)
                .family(egui::FontFamily::Name(BOLD.into()))
                .size(16.0)
                .color(pal.text),
        )
        .on_hover_text(root.display().to_string());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if labeled_icon(ui, icon::ARROWS_IN_LINE_VERTICAL, "Collapse all").clicked()
                && let Some(t) = &mut p.files.tree
            {
                t.collapse_all();
            }
            if labeled_icon(ui, icon::ARROW_CLOCKWISE, "Refresh").clicked()
                && let Some(t) = &mut p.files.tree
            {
                t.refresh();
            }
            let reveal = ui.add_enabled_ui(p.active_file.is_some(), |ui| {
                labeled_icon(ui, icon::CROSSHAIR, "Reveal active file")
            });
            if reveal.inner.clicked()
                && let Some(f) = p.active_file.clone()
                && !p.files.reveal(&f)
            {
                p.state.error("The active file isn't in this folder");
            }
            if labeled_icon(ui, icon::FOLDER_PLUS, "New folder").clicked() {
                todo.push(Do::New(root.clone(), true));
            }
            if labeled_icon(ui, icon::FILE_PLUS, "New file").clicked() {
                todo.push(Do::New(root.clone(), false));
            }
        });
    });
    if let Some(err) = p.files.edit.as_ref().and_then(|e| e.error.clone()) {
        ui.label(theme::itext_styled(
            icon::WARNING,
            &err,
            theme::small_size(),
            pal.err,
        ));
    }
    ui.add_space(2.0);

    let rows = flatten(p.files, &root);
    let row_h = (theme::body_size() * 1.65).round();
    let mut scroll = ScrollArea::vertical()
        .id_salt("files-tree")
        .auto_shrink([false, false]);
    if let Some(target) = p.files.scroll_to.take()
        && let Some(i) = rows
            .iter()
            .position(|r| matches!(r, Row::Entry { entry, .. } if entry.path == target))
    {
        let spacing = ui.spacing().item_spacing.y;
        scroll = scroll.vertical_scroll_offset(((i as f32) * (row_h + spacing) - 80.0).max(0.0));
    }
    let active = p.active_file.clone();
    let mut commit = false;
    let mut cancel = false;
    scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
        let last = range.end == rows.len();
        for row in &rows[range] {
            match row {
                Row::Entry { depth, entry, open } => {
                    let at = RowAt {
                        depth: *depth,
                        open: *open,
                        selected: active.as_deref() == Some(entry.path.as_path()),
                        root: &root,
                    };
                    entry_row(ui, &pal, row_h, &at, entry, &mut todo);
                }
                Row::Edit(depth) => {
                    let Some(edit) = p.files.edit.as_mut() else {
                        continue;
                    };
                    let (c, x) = edit_row(ui, row_h, *depth, edit);
                    commit |= c;
                    cancel |= x;
                }
            }
        }
        // The empty space below the last row (down to the bottom of the view): a menu for
        // the folder itself.
        if last {
            let h = (ui.clip_rect().bottom() - ui.cursor().top()).max(row_h);
            let (_, bg) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
            bg.context_menu(|ui| {
                new_items(ui, &root, &mut todo);
            });
        }
    });
    if commit {
        let editors = &*p.editors;
        p.files.commit(p.actions, |target| {
            editors.keys().any(|k| k.0.starts_with(target))
        });
    } else if cancel {
        p.files.edit = None;
    }

    for d in todo {
        match d {
            Do::Open(path) => p.actions.push(Action::OpenFile(path)),
            Do::Toggle(dir) => {
                if let Some(t) = &mut p.files.tree {
                    let open = t.is_expanded(&dir);
                    t.set_expanded(&dir, !open);
                }
            }
            Do::New(dir, folder) => p.files.start_new(dir, folder),
            Do::Rename(path) => p.files.start_rename(path),
            Do::Delete(path) => p.files.confirm_delete = Some(path),
            Do::Copy(text) => ui.ctx().copy_text(text),
            Do::Reveal(path) => {
                if let Err(e) = tree::reveal_in_file_manager(&path) {
                    p.state.error(e);
                }
            }
        }
    }
}

/// The rows to draw: open folders' contents, depth first, with the inline name field where
/// it goes (first in its folder when new, in place of the row when renaming).
fn flatten(files: &mut FilesUi, root: &Path) -> Vec<Row> {
    fn walk(
        tree: &mut Tree,
        edit: Option<&InlineEdit>,
        dir: &Path,
        depth: usize,
        out: &mut Vec<Row>,
    ) {
        if let Some(e) = edit
            && !matches!(e.kind, EditKind::Rename(_))
            && e.dir == dir
        {
            out.push(Row::Edit(depth));
        }
        for entry in tree.children(dir).to_vec() {
            let renaming = edit.is_some_and(|e| e.kind == EditKind::Rename(entry.path.clone()));
            let open = entry.is_dir && tree.is_expanded(&entry.path);
            let path = entry.path.clone();
            out.push(if renaming {
                Row::Edit(depth)
            } else {
                Row::Entry { depth, entry, open }
            });
            if open {
                walk(tree, edit, &path, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(tree) = &mut files.tree {
        walk(tree, files.edit.as_ref(), root, 0, &mut out);
    }
    out
}

const INDENT: f32 = 14.0;

/// Where a row sits: its depth, whether its folder is open, whether it's the active file.
struct RowAt<'a> {
    depth: usize,
    open: bool,
    selected: bool,
    root: &'a Path,
}

fn entry_row(
    ui: &mut Ui,
    pal: &theme::Palette,
    row_h: f32,
    at: &RowAt,
    e: &Entry,
    todo: &mut Vec<Do>,
) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, &e.name));
    let (open, depth, selected) = (at.open, at.depth, at.selected);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        if selected {
            painter.rect_filled(
                rect,
                CornerRadius::same(5),
                theme::with_alpha(pal.accent, 40),
            );
        } else if resp.hovered() || resp.has_focus() {
            painter.rect_filled(rect, CornerRadius::same(5), pal.bg_hover);
        }
        let color = if e.ignored { pal.text_faint } else { pal.text };
        let mut x = rect.left() + 4.0 + depth as f32 * INDENT;
        let cy = rect.center().y;
        let icon_font = egui::FontId::new(
            theme::small_size() + 1.0,
            egui::FontFamily::Name(theme::ICONS.into()),
        );
        if e.is_dir {
            let caret = if open {
                icon::CARET_DOWN
            } else {
                icon::CARET_RIGHT
            };
            painter.text(
                egui::pos2(x, cy),
                egui::Align2::LEFT_CENTER,
                caret,
                icon_font.clone(),
                pal.text_faint,
            );
        }
        x += 14.0;
        let (glyph, tint) = entry_icon(pal, e, open);
        painter.text(
            egui::pos2(x, cy),
            egui::Align2::LEFT_CENTER,
            glyph,
            egui::FontId::new(
                theme::body_size(),
                egui::FontFamily::Name(theme::ICONS.into()),
            ),
            if e.ignored { pal.text_faint } else { tint },
        );
        x += theme::body_size() + 6.0;
        painter.text(
            egui::pos2(x, cy),
            egui::Align2::LEFT_CENTER,
            &e.name,
            egui::FontId::proportional(theme::body_size() - 1.0),
            color,
        );
    }
    if resp.clicked() {
        todo.push(if e.is_dir {
            Do::Toggle(e.path.clone())
        } else {
            Do::Open(e.path.clone())
        });
    }
    let resp = resp.on_hover_text(e.path.display().to_string());
    resp.context_menu(|ui| {
        let dir = if e.is_dir {
            e.path.clone()
        } else {
            e.path.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        new_items(ui, &dir, todo);
        ui.separator();
        if ui.button(theme::itext(icon::PENCIL, "Rename")).clicked() {
            todo.push(Do::Rename(e.path.clone()));
            ui.close();
        }
        if ui.button(theme::itext(icon::TRASH, "Delete…")).clicked() {
            todo.push(Do::Delete(e.path.clone()));
            ui.close();
        }
        ui.separator();
        if ui.button(theme::itext(icon::COPY, "Copy path")).clicked() {
            todo.push(Do::Copy(e.path.display().to_string()));
            ui.close();
        }
        if ui
            .button(theme::itext(icon::COPY, "Copy relative path"))
            .clicked()
        {
            let rel = e.path.strip_prefix(at.root).unwrap_or(&e.path);
            todo.push(Do::Copy(rel.display().to_string()));
            ui.close();
        }
        if ui
            .button(theme::itext(
                icon::ARROW_SQUARE_OUT,
                "Reveal in file manager",
            ))
            .clicked()
        {
            todo.push(Do::Reveal(e.path.clone()));
            ui.close();
        }
    });
}

fn new_items(ui: &mut Ui, dir: &Path, todo: &mut Vec<Do>) {
    if ui
        .button(theme::itext(icon::FILE_PLUS, "New file"))
        .clicked()
    {
        todo.push(Do::New(dir.to_path_buf(), false));
        ui.close();
    }
    if ui
        .button(theme::itext(icon::FOLDER_PLUS, "New folder"))
        .clicked()
    {
        todo.push(Do::New(dir.to_path_buf(), true));
        ui.close();
    }
}

/// The inline name field. Returns (commit, cancel).
fn edit_row(ui: &mut Ui, row_h: f32, depth: usize, edit: &mut InlineEdit) -> (bool, bool) {
    let mut out = (false, false);
    ui.horizontal(|ui| {
        ui.set_height(row_h);
        ui.add_space(4.0 + depth as f32 * INDENT + 14.0);
        let glyph = match edit.kind {
            EditKind::NewFolder => icon::FOLDER,
            _ => icon::FILE,
        };
        ui.label(theme::ico(glyph).color(theme::palette_of(ui).text_dim));
        let id = ui.scope_id().with("files-edit");
        let hint = match edit.kind {
            EditKind::NewFile => "File name",
            EditKind::NewFolder => "Folder name",
            EditKind::Rename(_) => "New name",
        };
        let r = TextEdit::singleline(&mut edit.name)
            .id(id)
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .show(ui);
        if !edit.focused {
            r.response.request_focus();
            // Renaming selects the name without its extension, so typing replaces just that.
            let stem = match edit.name.rfind('.') {
                Some(i) if i > 0 => edit.name[..i].chars().count(),
                _ => edit.name.chars().count(),
            };
            let mut state = r.state.clone();
            state
                .cursor
                .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(stem))));
            state.store(ui.ctx(), r.response.id);
            edit.focused = true;
        } else if r.response.lost_focus() {
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                out.0 = true;
            } else {
                out.1 = true;
            }
        }
    });
    out
}

fn entry_icon(pal: &theme::Palette, e: &Entry, open: bool) -> (&'static str, egui::Color32) {
    if e.is_dir {
        return (
            if open {
                icon::FOLDER_OPEN
            } else {
                icon::FOLDER
            },
            pal.accent,
        );
    }
    let ext = Path::new(&e.name)
        .extension()
        .map(|x| x.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let code = pal.accent2;
    match ext.as_str() {
        "rs" => (icon::FILE_RS, pal.warn),
        "ts" | "mts" | "cts" => (icon::FILE_TS, code),
        "tsx" => (icon::FILE_TSX, code),
        "js" | "mjs" | "cjs" => (icon::FILE_JS, pal.warn),
        "jsx" => (icon::FILE_JSX, code),
        "py" => (icon::FILE_PY, code),
        "c" | "h" => (icon::FILE_C, code),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => (icon::FILE_CPP, code),
        "css" | "scss" => (icon::FILE_CSS, code),
        "html" | "htm" => (icon::FILE_HTML, pal.accent3),
        "md" | "markdown" => (icon::FILE_MD, pal.text_dim),
        "json" | "toml" | "yaml" | "yml" | "ron" => (icon::BRACKETS_CURLY, pal.accent3),
        "ini" | "cfg" | "conf" => (icon::FILE_INI, pal.text_dim),
        "txt" | "log" => (icon::FILE_TXT, pal.text_dim),
        "lock" => (icon::FILE_LOCK, pal.text_dim),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" => (icon::FILE_IMAGE, pal.ok),
        "svg" => (icon::FILE_SVG, pal.ok),
        "pdf" => (icon::FILE_PDF, pal.err),
        "zip" | "gz" | "tar" | "xz" | "zst" | "7z" => (icon::FILE_ZIP, pal.text_dim),
        "sh" | "bash" | "zsh" | "fish" => (icon::TERMINAL, pal.ok),
        _ => (icon::FILE, pal.text_dim),
    }
}

/// An icon button whose accessible name is its hover text.
fn labeled_icon(ui: &mut Ui, glyph: &str, label: &str) -> egui::Response {
    let r = theme::icon_button(ui, glyph, label);
    r.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), label));
    r
}

/// "Move ‹name› to the trash?" for a delete from the tree. A modal, so editor keys wait.
pub fn delete_dialog(ctx: &egui::Context, files: &mut FilesUi, state: &mut UiState) {
    let Some(path) = files.confirm_delete.clone() else {
        return;
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let folder = path.is_dir();
    let mut choice = None;
    let modal = egui::Modal::new(egui::Id::unique("files-delete")).show(ctx, |ui| {
        ui.set_max_width(400.0);
        theme::heading(ui, icon::TRASH, &format!("Move {name} to the trash?"));
        ui.label(if folder {
            "The folder and everything in it go to the trash; you can restore them from there."
        } else {
            "You can restore it from the trash."
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let pal = theme::palette_of(ui);
            let del = egui::Button::new(theme::itext_styled(
                icon::TRASH,
                "Move to Trash",
                theme::body_size(),
                egui::Color32::WHITE,
            ))
            .fill(pal.err);
            let r = ui.add(del);
            r.widget_info(|| WidgetInfo::labeled(Role::Button, true, "Move to Trash"));
            if r.clicked() {
                choice = Some(true);
            }
            if ui.button("Cancel").clicked() {
                choice = Some(false);
            }
        });
    });
    if modal.should_close() && choice.is_none() {
        choice = Some(false);
    }
    match choice {
        Some(true) => {
            files.confirm_delete = None;
            match (files.trash)(&path) {
                Ok(()) => {
                    files.changed(std::slice::from_ref(&path));
                    state.info(format!("Moved {name} to the trash"));
                }
                Err(e) => state.error(format!("Couldn't move {name} to the trash: {e}")),
            }
        }
        Some(false) => files.confirm_delete = None,
        None => {}
    }
}
