//! The Files panel's model: a lazily read tree of the workspace with .gitignore matching, and
//! the file operations its context menu offers. Folders are read when they're opened and
//! cached; watcher events mark only the folders they touch as stale.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// One row of a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// Absolute.
    pub path: PathBuf,
    pub is_dir: bool,
    /// Matched by a .gitignore (shown dimmed).
    pub ignored: bool,
}

pub struct Tree {
    root: PathBuf,
    /// Folders read so far, by absolute path.
    dirs: HashMap<PathBuf, Vec<Entry>>,
    /// Read folders that changed on disk: read again when next shown.
    stale: HashSet<PathBuf>,
    expanded: HashSet<PathBuf>,
    /// Each read folder's own .gitignore (an empty matcher when it has none).
    ignores: HashMap<PathBuf, Gitignore>,
}

impl Tree {
    pub fn new(root: PathBuf) -> Tree {
        Tree {
            root,
            dirs: HashMap::new(),
            stale: HashSet::new(),
            expanded: HashSet::new(),
            ignores: HashMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder's rows (folders first, then files, by name ignoring case), read from disk
    /// the first time and after it changed.
    pub fn children(&mut self, dir: &Path) -> &[Entry] {
        if !self.dirs.contains_key(dir) || self.stale.contains(dir) {
            self.stale.remove(dir);
            if dir.join(".gitignore").exists() || dir == self.root {
                self.ignores.remove(dir);
            }
            let rows = self.read(dir);
            self.dirs.insert(dir.to_path_buf(), rows);
        }
        &self.dirs[dir]
    }

    /// Rows already read, without touching the disk.
    #[cfg(test)]
    pub fn cached(&self, dir: &Path) -> Option<&[Entry]> {
        self.dirs.get(dir).map(Vec::as_slice)
    }

    #[cfg(test)]
    pub fn is_loaded(&self, dir: &Path) -> bool {
        self.dirs.contains_key(dir)
    }

    pub fn is_expanded(&self, dir: &Path) -> bool {
        dir == self.root || self.expanded.contains(dir)
    }

    pub fn set_expanded(&mut self, dir: &Path, open: bool) {
        if open {
            self.expanded.insert(dir.to_path_buf());
        } else {
            self.expanded.remove(dir);
        }
    }

    pub fn collapse_all(&mut self) {
        self.expanded.clear();
    }

    /// Open every folder above `path` so its row shows. False when it's outside the tree.
    pub fn reveal(&mut self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        let mut dir = self.root.clone();
        let parts: Vec<_> = rel.components().collect();
        for c in parts.iter().take(parts.len().saturating_sub(1)) {
            dir.push(c);
            self.expanded.insert(dir.clone());
        }
        true
    }

    /// Something changed at `path` (a watcher event): re-read the folders that show it. Events
    /// in folders never opened (a build's `target/`) cost a lookup and nothing more.
    pub fn invalidate(&mut self, path: &Path) {
        if path == self.root || !path.starts_with(&self.root) {
            // A rescan of everything (or of a folder holding the root).
            if self.root.starts_with(path) {
                self.stale.extend(self.dirs.keys().cloned());
                self.ignores.clear();
            }
            return;
        }
        if let Some(parent) = path.parent()
            && self.dirs.contains_key(parent)
        {
            self.stale.insert(parent.to_path_buf());
        }
        if path.file_name().is_some_and(|n| n == ".gitignore")
            && let Some(parent) = path.parent()
        {
            // New rules for the whole subtree.
            self.ignores.remove(parent);
            let under: Vec<PathBuf> = self
                .dirs
                .keys()
                .filter(|d| d.starts_with(parent))
                .cloned()
                .collect();
            self.stale.extend(under);
        }
        // A folder that was itself replaced or removed: forget what it held.
        if self.dirs.contains_key(path) {
            let under: Vec<PathBuf> = self
                .dirs
                .keys()
                .filter(|d| d.starts_with(path))
                .cloned()
                .collect();
            self.stale.extend(under);
        }
    }

    /// Re-read everything shown.
    pub fn refresh(&mut self) {
        self.stale.extend(self.dirs.keys().cloned());
        self.ignores.clear();
    }

    fn read(&mut self, dir: &Path) -> Vec<Entry> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let dir_ignored = self.is_ignored(dir, true);
        let mut rows: Vec<Entry> = rd
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != ".git")
            .map(|e| {
                let path = e.path();
                // Follow symlinks for the folder/file split; a broken link is a file.
                let is_dir = fs::metadata(&path).is_ok_and(|m| m.is_dir());
                let ignored = dir_ignored || self.matched(dir, &path, is_dir);
                Entry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    path,
                    is_dir,
                    ignored,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        rows
    }

    /// `path` (or a folder above it, inside the tree) is ignored.
    fn is_ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        if path == self.root || !path.starts_with(&self.root) {
            return false;
        }
        let Some(parent) = path.parent() else {
            return false;
        };
        let parent = parent.to_path_buf();
        // Known from the parent's rows when it's been read.
        if let Some(e) = self
            .dirs
            .get(&parent)
            .and_then(|rows| rows.iter().find(|e| e.path == path))
        {
            return e.ignored;
        }
        self.is_ignored(&parent, true) || self.matched(&parent, path, is_dir)
    }

    /// The .gitignore files from `dir` up to the root decide, the nearest one first (as git
    /// does: a deeper file overrides, `!pattern` re-includes).
    fn matched(&mut self, dir: &Path, path: &Path, is_dir: bool) -> bool {
        let mut at = Some(dir);
        while let Some(d) = at {
            if !d.starts_with(&self.root) {
                break;
            }
            match self.matcher(d).matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
            at = d.parent();
        }
        false
    }

    fn matcher(&mut self, dir: &Path) -> &Gitignore {
        let root = self.root.clone();
        self.ignores.entry(dir.to_path_buf()).or_insert_with(|| {
            let mut b = GitignoreBuilder::new(dir);
            let _ = b.add(dir.join(".gitignore"));
            if dir == root {
                let _ = b.add(dir.join(".git/info/exclude"));
            }
            b.build().unwrap_or_else(|_| Gitignore::empty())
        })
    }
}

/// A name for a new or renamed entry: one path component that isn't taken in `dir`.
pub fn check_name(dir: &Path, name: &str, current: Option<&Path>) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Type a name".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err("A name can't contain / or \\".into());
    }
    if name == "." || name == ".." {
        return Err("That name is reserved".into());
    }
    let target = dir.join(name);
    if Some(target.as_path()) != current && fs::symlink_metadata(&target).is_ok() {
        return Err(format!("\u{201c}{name}\u{201d} already exists"));
    }
    Ok(target)
}

/// Create an empty file (never replacing one that appeared meanwhile).
pub fn new_file(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let target = check_name(dir, name, None)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|e| describe(&target, e))?;
    Ok(target)
}

pub fn new_folder(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let target = check_name(dir, name, None)?;
    fs::create_dir(&target).map_err(|e| describe(&target, e))?;
    Ok(target)
}

/// Rename within the same folder.
pub fn rename(path: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = path.parent().ok_or("Can't rename the folder itself")?;
    let target = check_name(dir, name, Some(path))?;
    if target == path {
        return Ok(target);
    }
    // `rename` would replace a file created since the check; refuse instead.
    if fs::symlink_metadata(&target).is_ok() {
        return Err(format!("\u{201c}{}\u{201d} already exists", name.trim()));
    }
    fs::rename(path, &target).map_err(|e| describe(path, e))?;
    Ok(target)
}

fn describe(path: &Path, e: io::Error) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    format!("{name}: {e}")
}

/// The real trash (freedesktop.org trash on Linux). Never falls back to deleting.
pub fn system_trash(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| e.to_string())
}

/// Open the folder holding `path` in the desktop's file manager, without waiting for it.
pub fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(opener)
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Couldn't run {opener}: {e}"))?;
    // Reap it when it exits so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_one_component_and_free() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a.txt"), "").unwrap();
        assert!(check_name(t.path(), "a/b", None).unwrap_err().contains("/"));
        assert!(check_name(t.path(), "..", None).is_err());
        assert!(check_name(t.path(), "  ", None).is_err());
        assert!(
            check_name(t.path(), "a.txt", None)
                .unwrap_err()
                .contains("already exists")
        );
        let a = t.path().join("a.txt");
        assert_eq!(check_name(t.path(), "a.txt", Some(&a)).unwrap(), a);
        assert_eq!(
            check_name(t.path(), " b.txt ", None).unwrap(),
            t.path().join("b.txt")
        );
    }

    #[test]
    fn whitelist_in_a_nested_gitignore_wins() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        fs::create_dir(root.join("logs")).unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("logs/.gitignore"), "!keep.log\n").unwrap();
        fs::write(root.join("logs/keep.log"), "").unwrap();
        fs::write(root.join("logs/drop.log"), "").unwrap();
        let mut tree = Tree::new(root.clone());
        let rows: Vec<(String, bool)> = tree
            .children(&root.join("logs"))
            .iter()
            .map(|e| (e.name.clone(), e.ignored))
            .collect();
        assert_eq!(
            rows,
            [
                (".gitignore".to_string(), false),
                ("drop.log".to_string(), true),
                ("keep.log".to_string(), false)
            ]
        );
    }

    #[test]
    fn invalidate_marks_only_the_touched_folder() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        fs::create_dir(root.join("a")).unwrap();
        let mut tree = Tree::new(root.clone());
        assert!(tree.children(&root.join("a")).is_empty());
        let _ = tree.children(&root);
        fs::write(root.join("a/new.txt"), "").unwrap();
        tree.invalidate(&root.join("a/new.txt"));
        assert!(tree.stale.contains(&root.join("a")));
        assert!(!tree.stale.contains(&root));
        assert_eq!(tree.children(&root.join("a")).len(), 1);
        // Events in folders never read are ignored.
        tree.invalidate(&root.join("target/debug/x.o"));
        assert!(tree.stale.is_empty());
        assert!(!tree.is_loaded(&root.join("target/debug")));
    }
}
