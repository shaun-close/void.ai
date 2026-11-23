//! What the palette searches: the workspace's files (walked off the UI thread, cached, and
//! walked again after the watcher reports changes), recent files, and the app's other lists.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use void_core::runner::{Envs, MoonTask, Runs};
use void_editor::Symbol;

use super::{Item, TabRef};
use crate::app::disk::Debounce;
use crate::commands::{COMMANDS, def};

/// Walks stop here (a home folder opened as a workspace shouldn't eat the memory).
pub const MAX_FILES: usize = 200_000;

/// The workspace's files, relative to its root.
#[derive(Default)]
pub struct FileIndex {
    root: Option<PathBuf>,
    files: Vec<PathBuf>,
    /// Bumped whenever `files` changes.
    version: u64,
    /// A walk of this root has landed.
    loaded: bool,
    /// Something changed under the root since the last walk started.
    stale: bool,
    walking: Option<mpsc::Receiver<Vec<PathBuf>>>,
    /// A burst of changes (a build writing `target/`) settles before the next walk.
    quiet: Debounce,
}

impl FileIndex {
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Follow the workspace folder (None in Scratch: no files).
    pub fn set_root(&mut self, root: Option<&Path>) {
        if self.root.as_deref() == root {
            return;
        }
        self.root = root.map(Path::to_path_buf);
        self.files.clear();
        self.version += 1;
        self.walking = None;
        self.loaded = false;
        self.stale = root.is_some();
        self.quiet = Debounce::default();
    }

    /// Paths changed on disk (watcher events).
    pub fn changed(&mut self, paths: &[PathBuf], now: Instant) {
        if let Some(root) = &self.root
            && paths
                .iter()
                .any(|p| p.starts_with(root) || root.starts_with(p))
        {
            self.stale = true;
            self.quiet.poke(now);
        }
    }

    /// Take a finished walk, and start one when the list is stale and someone looks at it
    /// (`wanted`: the palette is open), or when there's no list yet.
    pub fn poll(&mut self, ctx: &egui::Context, now: Instant, wanted: bool) {
        if let Some(rx) = &self.walking {
            match rx.try_recv() {
                Ok(files) => {
                    self.files = files;
                    self.version += 1;
                    self.loaded = true;
                    self.walking = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.walking = None,
            }
        }
        if !self.stale || self.walking.is_some() || !(wanted || !self.loaded) {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        if let Err(wait) = self.quiet.due(now) {
            ctx.request_repaint_after(wait);
            return;
        }
        self.stale = false;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let files = crate::panels::files::tree::walk_files(&root, MAX_FILES);
            let _ = tx.send(files);
            ctx.request_repaint();
        });
        self.walking = Some(rx);
    }
}

/// Files opened most recently first (this session, this workspace).
#[derive(Default)]
pub struct Recent {
    paths: Vec<PathBuf>,
}

impl Recent {
    const KEEP: usize = 30;

    pub fn touch(&mut self, path: &Path) {
        self.paths.retain(|p| p != path);
        self.paths.insert(0, path.to_path_buf());
        self.paths.truncate(Self::KEEP);
    }

    pub fn clear(&mut self) {
        self.paths.clear();
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// Everything the palette can list, gathered by the app when it opens.
pub struct Lists<'a> {
    pub root: Option<&'a Path>,
    pub files: &'a [PathBuf],
    /// Absolute, most recent first.
    pub recent: &'a [PathBuf],
    /// Editor tabs: `(id, absolute path, shows a chat's version)`.
    pub tabs: Vec<(u32, PathBuf, bool)>,
    pub tasks: &'a [MoonTask],
    pub runs: &'a Runs,
    pub envs: &'a Envs,
    /// The current editor tab's outline.
    pub symbols: Vec<Symbol>,
}

/// The palette's list: items with their secondary text, recent files and open tabs first.
#[derive(Default)]
pub struct Candidates {
    pub items: Vec<Item>,
    /// One line under each item: its folder, project, menu or kind.
    pub details: Vec<String>,
    /// How many items at the front are recent files and open tabs (an empty query shows them).
    pub front: usize,
}

impl Candidates {
    fn push(&mut self, item: Item, detail: String) {
        self.items.push(item);
        self.details.push(detail);
    }
}

/// `path` as the palette shows it: relative inside the workspace.
fn shown(root: Option<&Path>, path: &Path) -> PathBuf {
    root.and_then(|r| path.strip_prefix(r).ok())
        .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
}

/// The folder part of a shown path ("" at the top).
fn folder(rel: &Path) -> String {
    rel.parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn collect(l: &Lists) -> Candidates {
    let mut c = Candidates::default();
    // Recent files first; a file open in a tab is still listed as the file (opening it
    // focuses the tab).
    let mut listed: HashSet<PathBuf> = HashSet::new();
    for path in l.recent {
        let rel = shown(l.root, path);
        if listed.insert(rel.clone()) {
            let detail = folder(&rel);
            c.push(Item::File(rel), detail);
        }
    }
    for (id, path, chat) in &l.tabs {
        let rel = shown(l.root, path);
        if !*chat && listed.contains(&rel) {
            continue;
        }
        if !*chat {
            listed.insert(rel.clone());
        }
        let mut label = rel.to_string_lossy().into_owned();
        if *chat {
            label.push_str(" · chat");
        }
        let detail = format!("open tab · {}", folder(&rel));
        c.push(
            Item::Tab(TabRef {
                id: *id,
                path: path.clone(),
                label,
            }),
            detail,
        );
    }
    c.front = c.items.len();
    for cmd in COMMANDS {
        let mut detail = format!("{} menu", cmd.menu);
        if let Some(keys) = def(cmd.id).keys {
            detail.push_str(&format!(" · {keys}"));
        }
        c.push(Item::Command(cmd.id), detail);
    }
    for t in l.tasks {
        c.push(
            Item::Task(t.target.clone()),
            format!("moon task · {}", t.command),
        );
    }
    for g in &l.runs.groups {
        for rc in &g.configs {
            let n = rc.steps.len();
            c.push(
                Item::RunConfig {
                    id: rc.id.clone(),
                    name: rc.name.clone(),
                },
                format!(
                    "run config · {} · {n} step{}",
                    g.name,
                    if n == 1 { "" } else { "s" }
                ),
            );
        }
    }
    // Names only: a profile's values never leave the runner.
    for p in &l.envs.profiles {
        let active = l.envs.active.as_deref() == Some(p.name.as_str());
        c.push(
            Item::Env(p.name.clone()),
            if active {
                "environment · active".into()
            } else {
                "environment".into()
            },
        );
    }
    for s in &l.symbols {
        c.push(
            Item::Symbol {
                name: s.name.clone(),
                line: s.line,
            },
            format!("{:?} · line {}", s.kind, s.line + 1).to_lowercase(),
        );
    }
    for f in l.files {
        if !listed.contains(f) {
            c.push(Item::File(f.clone()), folder(f));
        }
    }
    c
}
