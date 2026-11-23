//! Outside changes: a watcher on the workspace folder (built off the UI thread, since a
//! recursive watch registers every folder below it), one on the folders of files open from
//! elsewhere, and a re-check of every open file when the window comes back into focus. Each
//! frame the app drains the changed paths into the open buffers and the file tree.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use void_editor::Watcher;

use super::VoidApp;
use crate::layout::EditorView;

#[derive(Default)]
pub struct DiskWatch {
    /// The folder the root watcher is (being) built for.
    root: Option<PathBuf>,
    watcher: Option<Watcher>,
    pending: Option<mpsc::Receiver<Result<Watcher, String>>>,
    /// Non-recursive watches on folders of files open from outside the workspace.
    extra: Option<Watcher>,
    extra_dirs: HashSet<PathBuf>,
    /// Paths delivered so far (tests wait on these instead of sleeping).
    #[cfg(test)]
    seen: HashSet<PathBuf>,
    /// Tests: drop every event, to show something else did the work.
    #[cfg(test)]
    paused: bool,
}

impl DiskWatch {
    /// The workspace folder is being watched (always true in Scratch, which has none).
    #[cfg(test)]
    pub fn ready(&self) -> bool {
        self.root.is_none() || self.watcher.is_some()
    }

    #[cfg(test)]
    pub fn pause(&mut self) {
        self.paused = true;
    }

    #[cfg(test)]
    pub fn saw(&self, path: &Path) -> bool {
        self.seen.contains(path)
    }

    /// Start watching `root` when it changed. Returns an error to show when watching failed.
    fn follow_root(&mut self, root: Option<&Path>, ctx: &egui::Context) -> Option<String> {
        if self.root.as_deref() != root {
            self.root = root.map(Path::to_path_buf);
            self.watcher = None;
            self.pending = None;
            if let Some(root) = root {
                let (tx, rx) = mpsc::channel();
                let (root, ctx) = (root.to_path_buf(), ctx.clone());
                std::thread::spawn(move || {
                    let wake = ctx.clone();
                    let w = Watcher::new(&[root], move || wake.request_repaint())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(w);
                    ctx.request_repaint();
                });
                self.pending = Some(rx);
            }
        }
        let rx = self.pending.as_ref()?;
        match rx.try_recv() {
            Ok(Ok(w)) => {
                self.watcher = Some(w);
                self.pending = None;
                None
            }
            Ok(Err(e)) => {
                self.pending = None;
                Some(format!("Not watching the folder for outside changes: {e}"))
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                None
            }
        }
    }

    /// Watch the folders holding `dirs` (files open from outside the workspace).
    fn sync_extra(&mut self, dirs: HashSet<PathBuf>, ctx: &egui::Context) {
        if dirs == self.extra_dirs {
            return;
        }
        if self.extra.is_none() {
            let wake = ctx.clone();
            self.extra = Watcher::new(&[], move || wake.request_repaint()).ok();
        }
        let Some(w) = &mut self.extra else {
            return;
        };
        for gone in self.extra_dirs.difference(&dirs) {
            let _ = w.unwatch(gone);
        }
        for new in dirs.difference(&self.extra_dirs) {
            let _ = w.watch_dir(new);
        }
        self.extra_dirs = dirs;
    }

    fn drain(&mut self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for w in [&self.watcher, &self.extra].into_iter().flatten() {
            paths.extend(w.drain());
        }
        #[cfg(test)]
        if self.paused {
            return Vec::new();
        }
        #[cfg(test)]
        self.seen.extend(paths.iter().cloned());
        paths
    }
}

impl VoidApp {
    /// Hand this frame's outside changes to the open buffers and the file tree.
    pub(super) fn poll_disk(&mut self, ctx: &egui::Context) {
        let root = self.state.workspace.root.clone();
        if let Some(err) = self.disk.follow_root(root.as_deref(), ctx) {
            self.state.error(err);
        }
        self.files.set_root(root.as_deref());
        let outside: HashSet<PathBuf> = self
            .editors
            .keys()
            .filter(|k| k.1 == EditorView::Disk)
            .filter(|k| root.as_ref().is_none_or(|r| !k.0.starts_with(r)))
            .filter_map(|k| k.0.parent().map(Path::to_path_buf))
            .collect();
        self.disk.sync_extra(outside, ctx);

        let mut paths = self.disk.drain();
        // Review accepts and rejects write the workspace themselves: check those files now
        // instead of waiting for (or relying on) the watcher.
        if let Some(root) = &root {
            paths.extend(self.state.take_touched().iter().map(|p| root.join(p)));
        } else {
            self.state.take_touched();
        }
        if !paths.is_empty() {
            self.editors.check_disk(&paths);
            self.files.changed(&paths);
        }
        if ctx.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::WindowFocused(true)))
        }) {
            self.editors.check_all_disk();
        }
    }

    /// A rename from the Files panel: buffers and their tabs follow the file.
    pub(super) fn follow_rename(&mut self, from: &Path, to: &Path) {
        self.editors.rename_path(from, to);
        for (_, tab) in self.dock.iter_all_tabs_mut() {
            if let crate::layout::Tab::Editor { path, .. } = tab
                && path.starts_with(from)
            {
                *path = crate::editor::moved_path(path, from, to);
            }
        }
    }
}
