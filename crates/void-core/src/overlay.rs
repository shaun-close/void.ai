//! A conversation's pending changes: an overlayfs upper layer over the workspace.
//!
//! ```text
//! <dir>/upper/       overlayfs upper layer (what bwrap mounts; whiteouts are char devices 0:0,
//!                    replaced directories carry `user.overlay.opaque = "y"`)
//! <dir>/work/        overlayfs work directory
//! <dir>/base/        the workspace's content when each path was first changed (`<p>.void-absent`
//!                    marks "didn't exist"); diffs and conflict checks compare against it
//! <dir>/checkpoint/  copy of upper + base taken at the start of an agent turn (Retry restores it)
//! <dir>/meta.json    rejected changes not yet reported to the agent
//! ```
//!
//! The file tools work on the merged view in userspace with the same rules overlayfs uses.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use similar::{DiffOp, DiffTag, TextDiff};

use crate::fsutil;

const ABSENT: &str = ".void-absent";
const OPAQUE: &str = "user.overlay.opaque";

#[derive(Clone, Debug)]
pub struct Overlay {
    dir: Arc<PathBuf>,
    ws: Arc<PathBuf>,
}

/// Where a path resolves in the merged view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Missing,
    File(PathBuf),
    Dir(PathBuf),
    /// A symlink (never followed by the merged view).
    Link(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
    #[serde(default)]
    pub is_link: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    ReplacedDir,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Relative to the workspace.
    pub path: PathBuf,
    pub kind: ChangeKind,
    /// Matched by .gitignore (shown only because a file tool wrote it on purpose).
    pub ignored: bool,
    /// The real file changed since this change was made.
    pub conflict: bool,
    /// For a symlink: its target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub index: usize,
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    /// `' '` context, `'-'` removed, `'+'` added; text without the line ending.
    pub lines: Vec<(char, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("{} changed on disk since void edited it", .0.display())]
    Conflict(PathBuf),
    #[error("{} is outside the workspace", .0.display())]
    Outside(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Default, Serialize, Deserialize)]
struct Meta {
    rejected: Vec<String>,
}

fn is_whiteout(m: &fs::Metadata) -> bool {
    m.file_type().is_char_device() && m.rdev() == 0
}

fn is_opaque(p: &Path) -> bool {
    xattr::get(p, OPAQUE)
        .ok()
        .flatten()
        .is_some_and(|v| v == b"y")
}

fn components(rel: &Path) -> Vec<&std::ffi::OsStr> {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect()
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[derive(Clone, Copy, PartialEq)]
enum Pick {
    /// Start from old; the chosen hunk takes the new side.
    Accept,
    /// Start from new; the chosen hunk takes the old side.
    Reject,
}

/// Apply or revert hunk `index` of the `old` → `new` diff.
fn splice(old: &str, new: &str, index: usize, pick: Pick) -> io::Result<String> {
    let td = TextDiff::from_lines(old, new);
    let groups = td.grouped_ops(3);
    let chosen: Vec<DiffOp> = groups
        .get(index)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such change"))?
        .iter()
        .filter(|op| op.tag() != DiffTag::Equal)
        .cloned()
        .collect();
    let mut out = String::new();
    for op in td.ops() {
        let selected = chosen
            .iter()
            .any(|c| c.old_range() == op.old_range() && c.new_range() == op.new_range());
        let take_new = match (op.tag(), pick) {
            (DiffTag::Equal, _) => false,
            (_, Pick::Accept) => selected,
            (_, Pick::Reject) => !selected,
        };
        if take_new {
            for i in op.new_range() {
                out.push_str(td.new_slice(i).unwrap_or_default());
            }
        } else {
            for i in op.old_range() {
                out.push_str(td.old_slice(i).unwrap_or_default());
            }
        }
    }
    Ok(out)
}

impl Overlay {
    pub fn open(dir: &Path, workspace: &Path) -> io::Result<Overlay> {
        for sub in ["upper", "work", "base"] {
            fs::create_dir_all(dir.join(sub))?;
        }
        Ok(Overlay {
            dir: Arc::new(dir.to_path_buf()),
            ws: Arc::new(workspace.to_path_buf()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn workspace(&self) -> &Path {
        &self.ws
    }

    pub fn upper(&self) -> PathBuf {
        self.dir.join("upper")
    }

    pub fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    fn base(&self, rel: &Path) -> PathBuf {
        self.dir.join("base").join(rel)
    }

    fn absent(&self, rel: &Path) -> PathBuf {
        with_suffix(&self.base(rel), ABSENT)
    }

    /// True when an upper ancestor of `rel` (or `rel` itself) hides the lower layer.
    fn lower_hidden(&self, rel: &Path) -> bool {
        let mut cur = self.upper();
        for c in components(rel) {
            cur.push(c);
            match fs::symlink_metadata(&cur) {
                Ok(m) if is_whiteout(&m) => return true,
                Ok(m) if m.is_dir() && is_opaque(&cur) => return true,
                Ok(m) if !m.is_dir() => return true,
                _ => {}
            }
        }
        false
    }

    /// Resolve `rel` the way overlayfs would.
    pub fn resolve(&self, rel: &Path) -> Entry {
        let comps = components(rel);
        if comps.is_empty() {
            return Entry::Dir(self.ws.to_path_buf());
        }
        let mut cur = self.upper();
        let mut lower_visible = true;
        for (i, c) in comps.iter().enumerate() {
            cur.push(c);
            let last = i + 1 == comps.len();
            match fs::symlink_metadata(&cur) {
                Ok(m) if is_whiteout(&m) => return Entry::Missing,
                Ok(m) if last => {
                    return if m.file_type().is_symlink() {
                        Entry::Link(cur)
                    } else if m.is_dir() {
                        Entry::Dir(cur)
                    } else {
                        Entry::File(cur)
                    };
                }
                Ok(m) if m.is_dir() => {
                    if is_opaque(&cur) {
                        lower_visible = false;
                    }
                }
                Ok(_) => return Entry::Missing,
                Err(_) => {}
            }
        }
        if !lower_visible {
            return Entry::Missing;
        }
        // Never go through a symlinked directory in the real workspace.
        let mut prefix = self.ws.to_path_buf();
        for c in &comps[..comps.len() - 1] {
            prefix.push(c);
            if fs::symlink_metadata(&prefix).is_ok_and(|m| m.file_type().is_symlink()) {
                return Entry::Missing;
            }
        }
        let low = self.ws.join(rel);
        match fs::symlink_metadata(&low) {
            Ok(m) if m.file_type().is_symlink() => Entry::Link(low),
            Ok(m) if m.is_dir() => Entry::Dir(low),
            Ok(_) => Entry::File(low),
            Err(_) => Entry::Missing,
        }
    }

    pub fn read(&self, rel: &Path) -> io::Result<Vec<u8>> {
        match self.resolve(rel) {
            Entry::File(p) => fs::read(p),
            Entry::Dir(_) => Err(io::Error::other(format!(
                "{} is a directory",
                rel.display()
            ))),
            Entry::Link(_) => Err(io::Error::other(format!("{} is a symlink", rel.display()))),
            Entry::Missing => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} doesn't exist", rel.display()),
            )),
        }
    }

    /// The merged listing of a directory, sorted by name.
    pub fn read_dir(&self, rel: &Path) -> io::Result<Vec<DirItem>> {
        if !matches!(self.resolve(rel), Entry::Dir(_)) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} isn't a directory", rel.display()),
            ));
        }
        let mut names: BTreeMap<String, (bool, bool)> = BTreeMap::new();
        let mut gone: HashSet<String> = HashSet::new();
        let up = self.upper().join(rel);
        if let Ok(rd) = fs::read_dir(&up) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Ok(m) = fs::symlink_metadata(e.path()) else {
                    continue;
                };
                if is_whiteout(&m) {
                    gone.insert(name);
                } else {
                    let kind = match self.resolve(&rel.join(&name)) {
                        Entry::Dir(_) => (true, false),
                        Entry::Link(_) => (false, true),
                        _ => (false, false),
                    };
                    names.insert(name, kind);
                }
            }
        }
        if !self.lower_hidden(rel)
            && let Ok(rd) = fs::read_dir(self.ws.join(rel))
        {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if gone.contains(&name) || names.contains_key(&name) {
                    continue;
                }
                let kind = match fs::symlink_metadata(e.path()) {
                    Ok(m) if m.file_type().is_symlink() => (false, true),
                    Ok(m) => (m.is_dir(), false),
                    Err(_) => (false, false),
                };
                names.insert(name, kind);
            }
        }
        Ok(names
            .into_iter()
            .map(|(name, (is_dir, is_link))| DirItem {
                name,
                is_dir,
                is_link,
            })
            .collect())
    }

    /// Write a file into the layer, recording its base first.
    pub fn write(&self, rel: &Path, bytes: &[u8]) -> io::Result<()> {
        self.record_base(rel)?;
        // Parents: a deleted directory (whiteout) comes back as an opaque one, as overlayfs does.
        let mut cur = self.upper();
        let comps = components(rel);
        for c in &comps[..comps.len().saturating_sub(1)] {
            cur.push(c);
            match fs::symlink_metadata(&cur) {
                Ok(m) if is_whiteout(&m) => {
                    fs::remove_file(&cur)?;
                    fs::create_dir(&cur)?;
                    xattr::set(&cur, OPAQUE, b"y")?;
                }
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    return Err(io::Error::other(format!("{} is a file", cur.display())));
                }
                Err(_) => fs::create_dir(&cur)?,
            }
        }
        fsutil::atomic_write(&self.upper().join(rel), bytes)
    }

    fn manifest_path(&self, rel: &Path) -> PathBuf {
        with_suffix(&self.base(rel), ".void-dir")
    }

    fn has_base(&self, rel: &Path) -> bool {
        fs::symlink_metadata(self.base(rel)).is_ok()
            || fs::symlink_metadata(self.absent(rel)).is_ok()
            || fs::symlink_metadata(self.manifest_path(rel)).is_ok()
    }

    fn record_base(&self, rel: &Path) -> io::Result<()> {
        if self.has_base(rel) {
            return Ok(());
        }
        let low = self.ws.join(rel);
        match fs::metadata(&low) {
            Ok(m) if m.is_dir() => fsutil::atomic_write(
                &self.manifest_path(rel),
                &serde_json::to_vec(&dir_manifest(&low)).map_err(io::Error::other)?,
            ),
            Ok(_) => {
                let b = self.base(rel);
                if let Some(p) = b.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::copy(&low, &b).map(|_| ())
            }
            Err(_) => fsutil::atomic_write(&self.absent(rel), b""),
        }
    }

    fn clear_base(&self, rel: &Path) {
        let _ = fs::remove_file(self.base(rel));
        let _ = fs::remove_file(self.absent(rel));
        let _ = fs::remove_file(self.manifest_path(rel));
        let b = self.base(rel);
        if b.is_dir() {
            let _ = fs::remove_dir_all(&b);
        }
    }

    /// The workspace's .gitignore rules, read through the merged view.
    pub fn ignore_matcher(&self) -> ignore::gitignore::Gitignore {
        let mut b = ignore::gitignore::GitignoreBuilder::new(self.ws.as_path());
        if let Ok(bytes) = self.read(Path::new(".gitignore")) {
            for line in String::from_utf8_lossy(&bytes).lines() {
                let _ = b.add_line(None, line);
            }
        }
        b.build()
            .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
    }

    pub fn ignored(&self, gi: &ignore::gitignore::Gitignore, rel: &Path, is_dir: bool) -> bool {
        gi.matched_path_or_any_parents(self.ws.join(rel), is_dir)
            .is_ignore()
    }

    /// Record bases for everything in upper that has none yet (after a bash command).
    pub fn snapshot_bases(&self) -> io::Result<()> {
        let gi = self.ignore_matcher();
        let mut stack = vec![PathBuf::new()];
        while let Some(rel_dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(self.upper().join(&rel_dir)) else {
                continue;
            };
            for e in rd.flatten() {
                let rel = rel_dir.join(e.file_name());
                let Ok(m) = fs::symlink_metadata(e.path()) else {
                    continue;
                };
                if self.ignored(&gi, &rel, m.is_dir()) || rel.starts_with(".git") {
                    continue;
                }
                if m.is_dir() {
                    if is_opaque(&e.path()) {
                        self.record_base(&rel)?;
                    }
                    stack.push(rel);
                } else {
                    self.record_base(&rel)?;
                }
            }
        }
        Ok(())
    }

    /// The content changes are compared against: the base when recorded, else the real file.
    fn reference(&self, rel: &Path) -> Option<Vec<u8>> {
        if fs::symlink_metadata(self.absent(rel)).is_ok() {
            return None;
        }
        fs::read(self.base(rel))
            .ok()
            .or_else(|| fs::read(self.ws.join(rel)).ok())
    }

    fn conflicted(&self, rel: &Path) -> bool {
        let low = self.ws.join(rel);
        if let Ok(saved) = fs::read(self.manifest_path(rel)) {
            let saved: Vec<ManifestEntry> = serde_json::from_slice(&saved).unwrap_or_default();
            return saved != dir_manifest(&low);
        }
        if fs::symlink_metadata(self.absent(rel)).is_ok() {
            return fs::symlink_metadata(&low).is_ok();
        }
        match fs::read(self.base(rel)) {
            Ok(base) => fs::read(&low).ok().as_deref() != Some(base.as_slice()),
            Err(_) => false,
        }
    }

    /// Pending changes, sorted by path. `.git` and ignored build output are left out.
    pub fn changes(&self) -> Vec<Change> {
        let gi = self.ignore_matcher();
        let mut out = Vec::new();
        self.walk_changes(&gi, Path::new(""), &mut out);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    fn walk_changes(
        &self,
        gi: &ignore::gitignore::Gitignore,
        rel_dir: &Path,
        out: &mut Vec<Change>,
    ) {
        let Ok(rd) = fs::read_dir(self.upper().join(rel_dir)) else {
            return;
        };
        for e in rd.flatten() {
            let rel = rel_dir.join(e.file_name());
            if rel.starts_with(".git") {
                continue;
            }
            let Ok(m) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            let ignored = self.ignored(gi, &rel, m.is_dir());
            let has_base = self.has_base(&rel);
            if is_whiteout(&m) {
                if ignored && !has_base {
                    continue;
                }
                let conflict = self.conflicted(&rel);
                out.push(Change {
                    path: rel,
                    kind: ChangeKind::Deleted,
                    ignored,
                    conflict,
                    link: None,
                });
            } else if m.is_dir() {
                if ignored && !has_base {
                    // Build output: hidden, but files written on purpose inside still show.
                    self.walk_changes(gi, &rel, out);
                    continue;
                }
                if is_opaque(&e.path()) {
                    let conflict = self.conflicted(&rel);
                    out.push(Change {
                        path: rel.clone(),
                        kind: ChangeKind::ReplacedDir,
                        ignored,
                        conflict,
                        link: None,
                    });
                }
                self.walk_changes(gi, &rel, out);
            } else {
                if ignored && !has_base {
                    continue;
                }
                if m.file_type().is_symlink() {
                    let target = fs::read_link(e.path()).ok();
                    let lower = fs::read_link(self.ws.join(&rel)).ok();
                    if target.is_some() && target == lower {
                        continue;
                    }
                    let kind = if fs::symlink_metadata(self.ws.join(&rel)).is_ok() {
                        ChangeKind::Modified
                    } else {
                        ChangeKind::Added
                    };
                    out.push(Change {
                        path: rel,
                        kind,
                        ignored,
                        conflict: false,
                        link: target,
                    });
                    continue;
                }
                let reference = self.reference(&rel);
                let kind = if reference.is_none() {
                    ChangeKind::Added
                } else {
                    ChangeKind::Modified
                };
                let conflict = self.conflicted(&rel);
                if kind == ChangeKind::Modified && !conflict && fs::read(e.path()).ok() == reference
                {
                    continue;
                }
                out.push(Change {
                    path: rel,
                    kind,
                    ignored,
                    conflict,
                    link: None,
                });
            }
        }
    }

    fn upper_text(&self, rel: &Path) -> io::Result<String> {
        let up = self.upper().join(rel);
        let m = fs::symlink_metadata(&up)?;
        if is_whiteout(&m) {
            Ok(String::new())
        } else {
            Ok(text(&fs::read(&up)?))
        }
    }

    /// The change to one file as hunks (3 lines of context).
    pub fn diff(&self, rel: &Path) -> io::Result<Vec<Hunk>> {
        let up = self.upper().join(rel);
        if fs::symlink_metadata(&up)?.file_type().is_symlink() {
            let new = format!("symlink → {}\n", fs::read_link(&up)?.display());
            let old = match fs::read_link(self.ws.join(rel)) {
                Ok(t) => format!("symlink → {}\n", t.display()),
                Err(_) => String::new(),
            };
            return Ok(hunks(&old, &new));
        }
        let old = self.reference(rel).map(|b| text(&b)).unwrap_or_default();
        let new = self.upper_text(rel)?;
        Ok(hunks(&old, &new))
    }

    /// What changed on disk since void first changed this file (base → the real file now).
    pub fn disk_diff(&self, rel: &Path) -> io::Result<Vec<Hunk>> {
        let base = self.reference(rel).map(|b| text(&b)).unwrap_or_default();
        let now = fs::read(self.ws.join(rel))
            .map(|b| text(&b))
            .unwrap_or_default();
        Ok(hunks(&base, &now))
    }
}

fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    {
        let td = TextDiff::from_lines(old, new);
        let mut out = Vec::new();
        for (index, group) in td.grouped_ops(3).iter().enumerate() {
            let (Some(first), Some(last)) = (group.first(), group.last()) else {
                continue;
            };
            let mut lines = Vec::new();
            for op in group {
                for ch in td.iter_changes(op) {
                    let c = match ch.tag() {
                        similar::ChangeTag::Equal => ' ',
                        similar::ChangeTag::Delete => '-',
                        similar::ChangeTag::Insert => '+',
                    };
                    lines.push((c, ch.value().trim_end_matches(['\n', '\r']).to_string()));
                }
            }
            out.push(Hunk {
                index,
                old_start: first.old_range().start + 1,
                old_len: last.old_range().end - first.old_range().start,
                new_start: first.new_range().start + 1,
                new_len: last.new_range().end - first.new_range().start,
                lines,
            });
        }
        out
    }
}

impl Overlay {
    /// Refuse a path whose real location isn't inside the workspace.
    pub(crate) fn check_inside(&self, path: &Path) -> Result<(), OverlayError> {
        let ws = self.ws.canonicalize()?;
        let real = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => match path.parent().map(Path::canonicalize) {
                Some(Ok(p)) => p,
                _ => return Err(OverlayError::Outside(path.to_path_buf())),
            },
        };
        if real.starts_with(&ws) {
            Ok(())
        } else {
            Err(OverlayError::Outside(path.to_path_buf()))
        }
    }

    fn check_parent_inside(&self, path: &Path) -> Result<(), OverlayError> {
        match path.parent() {
            Some(p) => self.check_inside(p),
            None => Err(OverlayError::Outside(path.to_path_buf())),
        }
    }

    fn delete_lower(&self, rel: &Path) -> Result<(), OverlayError> {
        let low = self.ws.join(rel);
        let Ok(m) = fs::symlink_metadata(&low) else {
            return Ok(());
        };
        self.check_parent_inside(&low)?;
        if m.is_dir() {
            self.check_inside(&low)?;
            fs::remove_dir_all(&low)?;
        } else {
            fs::remove_file(&low)?;
        }
        Ok(())
    }

    /// Before creating directories: the deepest existing ancestor must be inside the workspace.
    fn check_existing_ancestor(&self, path: &Path) -> Result<(), OverlayError> {
        let mut p = path;
        loop {
            if fs::symlink_metadata(p).is_ok() {
                return self.check_inside(p);
            }
            match p.parent() {
                Some(parent) => p = parent,
                None => return Err(OverlayError::Outside(path.to_path_buf())),
            }
        }
    }

    fn write_lower(&self, rel: &Path, bytes: &[u8], mode_from: &Path) -> Result<(), OverlayError> {
        let low = self.ws.join(rel);
        if let Some(parent) = low.parent() {
            self.check_existing_ancestor(parent)?;
            fs::create_dir_all(parent)?;
        }
        self.check_parent_inside(&low)?;
        let perms = fs::metadata(&low)
            .or_else(|_| fs::metadata(mode_from))
            .map(|m| m.permissions());
        fsutil::atomic_write(&low, bytes)?;
        if let Ok(p) = perms {
            fs::set_permissions(&low, p)?;
        }
        Ok(())
    }

    /// Make the real workspace match the layer for one path (a file, deletion or directory).
    pub fn accept_file(&self, rel: &Path, force: bool) -> Result<(), OverlayError> {
        let up = self.upper().join(rel);
        let m = fs::symlink_metadata(&up)?;
        if is_whiteout(&m) {
            if self.conflicted(rel) && !force {
                return Err(OverlayError::Conflict(rel.to_path_buf()));
            }
            self.delete_lower(rel)?;
            fs::remove_file(&up)?;
            self.clear_base(rel);
        } else if m.is_dir() {
            if is_opaque(&up) && self.conflicted(rel) && !force {
                return Err(OverlayError::Conflict(rel.to_path_buf()));
            }
            self.check_existing_ancestor(&self.ws.join(rel))?;
            if is_opaque(&up)
                && let Ok(rd) = fs::read_dir(self.ws.join(rel))
            {
                for e in rd.flatten() {
                    let child = rel.join(e.file_name());
                    if fs::symlink_metadata(self.upper().join(&child)).is_err() {
                        self.delete_lower(&child)?;
                    }
                }
            }
            fs::create_dir_all(self.ws.join(rel))?;
            self.check_inside(&self.ws.join(rel))?;
            for e in fs::read_dir(&up)?.flatten() {
                self.accept_file(&rel.join(e.file_name()), force)?;
            }
            let _ = fs::remove_dir(&up);
        } else {
            if self.conflicted(rel) && !force {
                return Err(OverlayError::Conflict(rel.to_path_buf()));
            }
            if m.file_type().is_symlink() {
                let target = fs::read_link(&up)?;
                let low = self.ws.join(rel);
                self.check_parent_inside(&low)?;
                // A link may only point inside the workspace.
                let abs = if target.is_absolute() {
                    target.clone()
                } else {
                    low.parent().unwrap_or(&self.ws).join(&target)
                };
                let inside = lexical(&abs).is_some_and(|a| {
                    a.starts_with(self.ws.as_path())
                        || self.ws.canonicalize().is_ok_and(|w| a.starts_with(w))
                });
                if !inside {
                    return Err(OverlayError::Outside(abs));
                }
                let _ = fs::remove_file(&low);
                std::os::unix::fs::symlink(target, &low)?;
            } else {
                let bytes = fs::read(&up)?;
                self.write_lower(rel, &bytes, &up)?;
            }
            fs::remove_file(&up)?;
            self.clear_base(rel);
        }
        Ok(())
    }

    /// Drop one path's changes from the layer.
    pub fn reject_file(&self, rel: &Path) -> io::Result<()> {
        let up = self.upper().join(rel);
        let m = fs::symlink_metadata(&up)?;
        if m.is_dir() {
            fs::remove_dir_all(&up)?;
        } else {
            fs::remove_file(&up)?;
        }
        self.clear_base(rel);
        self.note(format!(
            "The user rejected your changes to {}",
            rel.display()
        ))
    }

    /// Apply one hunk to the real file.
    pub fn accept_hunk(&self, rel: &Path, index: usize, force: bool) -> Result<(), OverlayError> {
        let up = self.upper().join(rel);
        if is_whiteout(&fs::symlink_metadata(&up)?) {
            return self.accept_file(rel, force);
        }
        if self.conflicted(rel) && !force {
            return Err(OverlayError::Conflict(rel.to_path_buf()));
        }
        let old = self.reference(rel).map(|b| text(&b)).unwrap_or_default();
        let new = self.upper_text(rel)?;
        let merged = splice(&old, &new, index, Pick::Accept)?;
        self.write_lower(rel, merged.as_bytes(), &up)?;
        if merged == new {
            fs::remove_file(&up)?;
            self.clear_base(rel);
        } else {
            let _ = fs::remove_file(self.absent(rel));
            fsutil::atomic_write(&self.base(rel), merged.as_bytes())?;
        }
        Ok(())
    }

    /// Undo one hunk in the layer.
    pub fn reject_hunk(&self, rel: &Path, index: usize) -> io::Result<()> {
        let old_bytes = self.reference(rel);
        let old = old_bytes.as_deref().map(text).unwrap_or_default();
        let new = self.upper_text(rel)?;
        let hunk = self.diff(rel)?.into_iter().nth(index);
        let reverted = splice(&old, &new, index, Pick::Reject)?;
        // Back to the original (or, for a new file, back to nothing): drop it from the layer.
        let back_to_original = match old_bytes {
            Some(_) => reverted == old,
            None => reverted.is_empty(),
        };
        if back_to_original {
            fs::remove_file(self.upper().join(rel))?;
            self.clear_base(rel);
        } else {
            fsutil::atomic_write(&self.upper().join(rel), reverted.as_bytes())?;
        }
        let what = hunk
            .map(|h| {
                format!(
                    " (lines {}–{})",
                    h.new_start,
                    h.new_start + h.new_len.saturating_sub(1)
                )
            })
            .unwrap_or_default();
        self.note(format!(
            "The user rejected one of your changes to {}{what}",
            rel.display()
        ))
    }

    /// Accept every change; conflicted paths are skipped and returned. `keep_ignored` also
    /// copies hidden ignored files (build output) into the workspace.
    pub fn accept_all(&self, keep_ignored: bool) -> Result<Vec<PathBuf>, OverlayError> {
        let mut skipped = Vec::new();
        for c in self.changes() {
            if fs::symlink_metadata(self.upper().join(&c.path)).is_err() {
                continue;
            }
            if c.conflict {
                skipped.push(c.path);
                continue;
            }
            self.accept_file(&c.path, false)?;
        }
        if keep_ignored {
            let mut stack = vec![PathBuf::new()];
            while let Some(rel_dir) = stack.pop() {
                let Ok(rd) = fs::read_dir(self.upper().join(&rel_dir)) else {
                    continue;
                };
                for e in rd.flatten() {
                    let rel = rel_dir.join(e.file_name());
                    let Ok(m) = fs::symlink_metadata(e.path()) else {
                        continue;
                    };
                    if m.is_dir() {
                        stack.push(rel);
                    } else if !rel.starts_with(".git") && !self.has_base(&rel) {
                        // Only hidden build output (never based, so never in conflict).
                        self.accept_file(&rel, false)?;
                    }
                }
            }
        }
        Ok(skipped)
    }

    /// Reject every visible change.
    pub fn reject_all(&self) -> io::Result<()> {
        for c in self.changes() {
            if fs::symlink_metadata(self.upper().join(&c.path)).is_ok() {
                self.reject_file(&c.path)?;
            }
        }
        Ok(())
    }

    /// Delete the whole layer.
    pub fn discard(self) -> io::Result<()> {
        fsutil::remove_tree(self.dir.as_path())
    }

    fn meta(&self) -> Meta {
        fs::read(self.dir.join("meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn note(&self, text: String) -> io::Result<()> {
        let mut m = self.meta();
        m.rejected.push(text);
        fsutil::atomic_write(
            &self.dir.join("meta.json"),
            &serde_json::to_vec(&m).map_err(io::Error::other)?,
        )
    }

    /// Tell the agent at its next turn that the user edited `rel` in these pending changes.
    /// Repeated saves before that turn make one note.
    pub fn note_user_edit(&self, rel: &Path) -> io::Result<()> {
        let text = format!("The user edited {} in your pending changes", rel.display());
        if self.meta().rejected.contains(&text) {
            return Ok(());
        }
        self.note(text)
    }

    /// Rejection notes not yet passed to the agent (and forget them).
    pub fn take_rejections(&self) -> Vec<String> {
        let m = self.meta();
        if !m.rejected.is_empty() {
            let _ = fsutil::atomic_write(&self.dir.join("meta.json"), b"{\"rejected\":[]}");
        }
        m.rejected
    }

    /// Save upper + base so `restore_checkpoint` can undo a turn.
    pub fn checkpoint(&self) -> io::Result<()> {
        let cp = self.dir.join("checkpoint");
        if cp.exists() {
            fs::remove_dir_all(&cp)?;
        }
        let mut whiteouts = Vec::new();
        copy_tree(
            &self.upper(),
            &cp.join("upper"),
            Path::new(""),
            &mut whiteouts,
        )?;
        copy_tree(
            &self.dir.join("base"),
            &cp.join("base"),
            Path::new(""),
            &mut Vec::new(),
        )?;
        let list: Vec<String> = whiteouts
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        fs::write(
            cp.join("whiteouts.json"),
            serde_json::to_vec(&list).map_err(io::Error::other)?,
        )
    }

    /// Put upper + base back as they were at the last checkpoint. False when there is none.
    pub fn restore_checkpoint(&self) -> io::Result<bool> {
        let cp = self.dir.join("checkpoint");
        if !cp.join("upper").exists() {
            return Ok(false);
        }
        let list: Vec<String> = fs::read(cp.join("whiteouts.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let fresh = self.dir.join("upper.restore");
        if fresh.exists() {
            fs::remove_dir_all(&fresh)?;
        }
        copy_tree(&cp.join("upper"), &fresh, Path::new(""), &mut Vec::new())?;
        // Whiteouts can't be created without privileges: move the existing ones across,
        // recreate any that have gone through a throwaway overlay mount.
        let mut missing = Vec::new();
        for w in &list {
            let rel = Path::new(w);
            let cur = self.upper().join(rel);
            match fs::symlink_metadata(&cur) {
                Ok(m) if is_whiteout(&m) => {
                    if let Some(p) = fresh.join(rel).parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::rename(&cur, fresh.join(rel))?;
                }
                _ => missing.push(rel.to_path_buf()),
            }
        }
        fs::remove_dir_all(self.upper())?;
        fs::rename(&fresh, self.upper())?;
        let base = self.dir.join("base");
        fs::remove_dir_all(&base)?;
        copy_tree(&cp.join("base"), &base, Path::new(""), &mut Vec::new())?;
        if !missing.is_empty() {
            self.make_whiteouts(&missing)?;
        }
        Ok(true)
    }

    fn make_whiteouts(&self, rels: &[PathBuf]) -> io::Result<()> {
        let ws = self.ws.as_path();
        let mut cmd = std::process::Command::new("bwrap");
        cmd.args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
        ])
        .arg("--overlay-src")
        .arg(ws)
        .arg("--overlay")
        .arg(self.upper())
        .arg(self.work())
        .arg(ws)
        .arg("--chdir")
        .arg(ws)
        .args(["rm", "-rf", "--"])
        .args(rels);
        let st = cmd.status().map_err(|e| {
            io::Error::other(format!(
                "can't recreate deleted-file markers without bubblewrap: {e}"
            ))
        })?;
        if st.success() {
            Ok(())
        } else {
            Err(io::Error::other("recreating deleted-file markers failed"))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ManifestEntry {
    path: String,
    len: u64,
    mtime_ns: i128,
    dir: bool,
}

/// What's in a real directory (not following links), to notice changes before deleting it.
fn dir_manifest(dir: &Path) -> Vec<ManifestEntry> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = fs::read_dir(dir.join(&rel)) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            let r = rel.join(e.file_name());
            out.push(ManifestEntry {
                path: r.to_string_lossy().into_owned(),
                len: m.len(),
                mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
                dir: m.is_dir(),
            });
            if m.is_dir() && out.len() < 20_000 {
                stack.push(r);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn lexical(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// Copy a layer directory: files, symlinks, directories (keeping the opaque mark); whiteouts
/// are listed in `whiteouts` instead of copied.
fn copy_tree(src: &Path, dst: &Path, rel: &Path, whiteouts: &mut Vec<PathBuf>) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    if is_opaque(src) {
        xattr::set(dst, OPAQUE, b"y")?;
    }
    let Ok(rd) = fs::read_dir(src) else {
        return Ok(());
    };
    for e in rd.flatten() {
        let m = fs::symlink_metadata(e.path())?;
        let name = e.file_name();
        let (s, d, r) = (e.path(), dst.join(&name), rel.join(&name));
        if is_whiteout(&m) {
            whiteouts.push(r);
        } else if m.is_dir() {
            copy_tree(&s, &d, &r, whiteouts)?;
        } else if m.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&s)?, &d)?;
        } else {
            fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct T {
        _tmp: tempfile::TempDir,
        ws: PathBuf,
        ov: Overlay,
    }

    fn setup() -> T {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let ov = Overlay::open(&tmp.path().join("ovl"), &ws).unwrap();
        T { _tmp: tmp, ws, ov }
    }

    fn have_bwrap() -> bool {
        let ok = Path::new("/usr/bin/bwrap").exists();
        if !ok {
            eprintln!("skipping: bwrap not installed");
        }
        ok
    }

    /// Run `script` in the workspace through a real overlay mount (makes real whiteouts).
    fn bwrap_exec(t: &T, script: &str) {
        let ws = t.ws.to_str().unwrap();
        let st = std::process::Command::new("/usr/bin/bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--tmpfs",
                "/tmp",
            ])
            .args(["--overlay-src", ws, "--overlay"])
            .arg(t.ov.upper())
            .arg(t.ov.work())
            .args([ws, "--chdir", ws, "bash", "-c", script])
            .status()
            .unwrap();
        assert!(st.success(), "bwrap script failed: {script}");
    }

    fn rd(t: &T, p: &str) -> String {
        String::from_utf8(t.ov.read(Path::new(p)).unwrap()).unwrap()
    }

    #[test]
    fn write_goes_to_upper_and_records_base() {
        let t = setup();
        fs::write(t.ws.join("a.txt"), "1").unwrap();
        t.ov.write(Path::new("a.txt"), b"2").unwrap();
        assert_eq!(fs::read_to_string(t.ws.join("a.txt")).unwrap(), "1");
        assert_eq!(rd(&t, "a.txt"), "2");
        assert_eq!(
            fs::read_to_string(t.ov.dir().join("base/a.txt")).unwrap(),
            "1"
        );
        // A second write keeps the original base.
        t.ov.write(Path::new("a.txt"), b"3").unwrap();
        assert_eq!(
            fs::read_to_string(t.ov.dir().join("base/a.txt")).unwrap(),
            "1"
        );
    }

    #[test]
    fn new_file_records_absent_base() {
        let t = setup();
        t.ov.write(Path::new("d/new.txt"), b"x").unwrap();
        assert!(t.ov.dir().join("base/d/new.txt.void-absent").exists());
        assert!(!t.ws.join("d").exists());
        assert!(matches!(t.ov.resolve(Path::new("d")), Entry::Dir(_)));
    }

    #[test]
    fn resolve_sees_whiteout_and_opaque_dirs() {
        if !have_bwrap() {
            return;
        }
        let t = setup();
        fs::write(t.ws.join("a"), "a").unwrap();
        fs::create_dir_all(t.ws.join("d")).unwrap();
        fs::write(t.ws.join("d/x"), "x").unwrap();
        bwrap_exec(&t, "rm a; rm -rf d; mkdir d; echo y > d/y");
        assert!(matches!(t.ov.resolve(Path::new("a")), Entry::Missing));
        assert!(matches!(t.ov.resolve(Path::new("d/x")), Entry::Missing));
        let names: Vec<_> =
            t.ov.read_dir(Path::new("d"))
                .unwrap()
                .into_iter()
                .map(|i| i.name)
                .collect();
        assert_eq!(names, ["y"]);
        let root: Vec<_> =
            t.ov.read_dir(Path::new(""))
                .unwrap()
                .into_iter()
                .map(|i| i.name)
                .collect();
        assert_eq!(root, ["d"]);
    }

    #[test]
    fn changes_lists_kinds() {
        if !have_bwrap() {
            return;
        }
        let t = setup();
        fs::write(t.ws.join(".gitignore"), "target/\n.env.local\n").unwrap();
        fs::write(t.ws.join("m.txt"), "old\n").unwrap();
        fs::write(t.ws.join("gone.txt"), "bye\n").unwrap();
        fs::create_dir_all(t.ws.join("r")).unwrap();
        fs::write(t.ws.join("r/old"), "o").unwrap();
        bwrap_exec(
            &t,
            "rm gone.txt; rm -rf r; mkdir r; echo n > r/new; mkdir -p target; echo b > target/bin",
        );
        t.ov.snapshot_bases().unwrap();
        t.ov.write(Path::new("m.txt"), b"new\n").unwrap();
        t.ov.write(Path::new("added.txt"), b"hi\n").unwrap();
        t.ov.write(Path::new(".env.local"), b"K=1\n").unwrap();
        let ch = t.ov.changes();
        let find = |p: &str| ch.iter().find(|c| c.path == Path::new(p)).cloned();
        assert_eq!(find("m.txt").unwrap().kind, ChangeKind::Modified);
        assert_eq!(find("added.txt").unwrap().kind, ChangeKind::Added);
        assert_eq!(find("gone.txt").unwrap().kind, ChangeKind::Deleted);
        assert_eq!(find("r").unwrap().kind, ChangeKind::ReplacedDir);
        assert_eq!(find("r/new").unwrap().kind, ChangeKind::Added);
        assert!(
            find("target/bin").is_none(),
            "ignored build output is hidden"
        );
        let env = find(".env.local").unwrap();
        assert!(
            env.ignored,
            "ignored file written on purpose is shown, marked ignored"
        );
        assert!(ch.iter().all(|c| !c.conflict));
    }

    #[test]
    fn unchanged_copy_up_is_not_a_change() {
        let t = setup();
        fs::write(t.ws.join("same.txt"), "s").unwrap();
        t.ov.write(Path::new("same.txt"), b"s").unwrap();
        assert!(t.ov.changes().is_empty());
    }

    #[test]
    fn diff_has_hunks() {
        let t = setup();
        fs::write(t.ws.join("f"), "a\nb\nc\n").unwrap();
        t.ov.write(Path::new("f"), b"a\nB\nc\n").unwrap();
        let h = t.ov.diff(Path::new("f")).unwrap();
        assert_eq!(h.len(), 1);
        let lines: Vec<_> = h[0].lines.iter().map(|(c, l)| format!("{c}{l}")).collect();
        assert_eq!(lines, [" a", "-b", "+B", " c"]);
        assert_eq!(
            (h[0].old_start, h[0].old_len, h[0].new_start, h[0].new_len),
            (1, 3, 1, 3)
        );
    }

    #[test]
    fn accept_file_writes_lower_and_clears() {
        let t = setup();
        fs::write(t.ws.join("a"), "1").unwrap();
        t.ov.write(Path::new("a"), b"2").unwrap();
        t.ov.write(Path::new("sub/new"), b"n").unwrap();
        t.ov.accept_file(Path::new("a"), false).unwrap();
        t.ov.accept_file(Path::new("sub/new"), false).unwrap();
        assert_eq!(fs::read_to_string(t.ws.join("a")).unwrap(), "2");
        assert_eq!(fs::read_to_string(t.ws.join("sub/new")).unwrap(), "n");
        assert!(t.ov.changes().is_empty());
        assert!(!t.ov.upper().join("a").exists());
    }

    #[test]
    fn accept_keeps_lower_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let t = setup();
        fs::write(t.ws.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(t.ws.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        t.ov.write(Path::new("run.sh"), b"#!/bin/sh\necho hi\n")
            .unwrap();
        t.ov.accept_file(Path::new("run.sh"), false).unwrap();
        let mode = fs::metadata(t.ws.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn reject_file_restores_view() {
        let t = setup();
        fs::write(t.ws.join("a"), "1").unwrap();
        t.ov.write(Path::new("a"), b"2").unwrap();
        t.ov.reject_file(Path::new("a")).unwrap();
        assert_eq!(rd(&t, "a"), "1");
        assert!(t.ov.changes().is_empty());
        assert_eq!(t.ov.take_rejections().len(), 1);
    }

    #[test]
    fn accept_hunk_and_reject_hunk() {
        let t = setup();
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 2\n", "LINE 2\n")
            .replace("line 18\n", "LINE 18\n");
        fs::write(t.ws.join("f"), &old).unwrap();
        t.ov.write(Path::new("f"), new.as_bytes()).unwrap();
        assert_eq!(t.ov.diff(Path::new("f")).unwrap().len(), 2);
        t.ov.accept_hunk(Path::new("f"), 0, false).unwrap();
        assert_eq!(
            fs::read_to_string(t.ws.join("f")).unwrap(),
            old.replace("line 2\n", "LINE 2\n")
        );
        assert_eq!(t.ov.diff(Path::new("f")).unwrap().len(), 1);
        t.ov.reject_hunk(Path::new("f"), 0).unwrap();
        assert!(
            t.ov.changes().is_empty(),
            "upper equals base again → dropped"
        );
        assert_eq!(rd(&t, "f"), old.replace("line 2\n", "LINE 2\n"));
    }

    #[test]
    fn accept_detects_conflict() {
        let t = setup();
        fs::write(t.ws.join("a"), "1").unwrap();
        t.ov.write(Path::new("a"), b"2").unwrap();
        fs::write(t.ws.join("a"), "edited elsewhere").unwrap();
        assert!(t.ov.changes()[0].conflict);
        assert!(matches!(
            t.ov.accept_file(Path::new("a"), false),
            Err(OverlayError::Conflict(_))
        ));
        assert_eq!(
            fs::read_to_string(t.ws.join("a")).unwrap(),
            "edited elsewhere"
        );
        t.ov.accept_file(Path::new("a"), true).unwrap();
        assert_eq!(fs::read_to_string(t.ws.join("a")).unwrap(), "2");
    }

    #[test]
    fn accept_deleted_removes_lower() {
        if !have_bwrap() {
            return;
        }
        let t = setup();
        fs::write(t.ws.join("g"), "g").unwrap();
        fs::create_dir_all(t.ws.join("dd/x")).unwrap();
        fs::write(t.ws.join("dd/x/f"), "f").unwrap();
        bwrap_exec(&t, "rm g; rm -rf dd");
        t.ov.snapshot_bases().unwrap();
        t.ov.accept_file(Path::new("g"), false).unwrap();
        t.ov.accept_file(Path::new("dd"), false).unwrap();
        assert!(!t.ws.join("g").exists());
        assert!(!t.ws.join("dd").exists());
        assert!(t.ov.changes().is_empty());
    }

    #[test]
    fn accept_deleted_refuses_outside() {
        if !have_bwrap() {
            return;
        }
        let t = setup();
        let outside = t._tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious"), "keep").unwrap();
        std::os::unix::fs::symlink(&outside, t.ws.join("link")).unwrap();
        // The agent deletes through the symlinked directory inside the overlay.
        bwrap_exec(&t, "rm -f link/precious || true; rm link");
        t.ov.snapshot_bases().unwrap();
        // Accepting the deletion of `link` removes the link, never what it points at.
        t.ov.accept_file(Path::new("link"), false).unwrap();
        assert!(outside.join("precious").exists());
        // A fabricated whiteout path that resolves outside is refused.
        let r = t.ov.check_inside(&t.ws.join("link/../outside/precious"));
        assert!(matches!(r, Err(OverlayError::Outside(_))));
    }

    #[test]
    fn checkpoint_restore_round_trip() {
        let t = setup();
        fs::write(t.ws.join("a"), "1").unwrap();
        t.ov.write(Path::new("a"), b"2").unwrap();
        t.ov.checkpoint().unwrap();
        t.ov.write(Path::new("a"), b"3").unwrap();
        t.ov.write(Path::new("b"), b"new").unwrap();
        assert!(t.ov.restore_checkpoint().unwrap());
        assert_eq!(rd(&t, "a"), "2");
        assert!(matches!(t.ov.resolve(Path::new("b")), Entry::Missing));
        assert_eq!(t.ov.changes().len(), 1);
    }

    #[test]
    fn checkpoint_restore_keeps_whiteouts() {
        if !have_bwrap() {
            return;
        }
        let t = setup();
        fs::write(t.ws.join("w"), "w").unwrap();
        bwrap_exec(&t, "rm w");
        t.ov.checkpoint().unwrap();
        t.ov.reject_file(Path::new("w")).unwrap();
        assert!(matches!(t.ov.resolve(Path::new("w")), Entry::File(_)));
        assert!(t.ov.restore_checkpoint().unwrap());
        assert!(matches!(t.ov.resolve(Path::new("w")), Entry::Missing));
    }

    #[test]
    fn user_edit_notes_are_reported_once_each() {
        let t = setup();
        t.ov.write(Path::new("src/x.rs"), b"1").unwrap();
        t.ov.note_user_edit(Path::new("src/x.rs")).unwrap();
        t.ov.note_user_edit(Path::new("src/x.rs")).unwrap();
        assert_eq!(
            t.ov.take_rejections(),
            ["The user edited src/x.rs in your pending changes"]
        );
        assert!(t.ov.take_rejections().is_empty());
    }

    #[test]
    fn rejections_are_reported_once() {
        let t = setup();
        t.ov.write(Path::new("x"), b"1").unwrap();
        t.ov.reject_file(Path::new("x")).unwrap();
        let r = t.ov.take_rejections();
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("x"));
        assert!(t.ov.take_rejections().is_empty());
    }

    #[test]
    fn accept_all_skips_conflicts() {
        let t = setup();
        fs::write(t.ws.join("a"), "1").unwrap();
        t.ov.write(Path::new("a"), b"2").unwrap();
        t.ov.write(Path::new("b"), b"b").unwrap();
        fs::write(t.ws.join("a"), "changed").unwrap();
        let skipped = t.ov.accept_all(false).unwrap();
        assert_eq!(skipped, vec![PathBuf::from("a")]);
        assert_eq!(fs::read_to_string(t.ws.join("b")).unwrap(), "b");
    }

    #[test]
    fn merged_view_never_follows_symlinks() {
        let t = setup();
        let outside = t._tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("id_key"), "SECRET").unwrap();
        std::os::unix::fs::symlink(&outside, t.ws.join("keys")).unwrap();
        std::os::unix::fs::symlink(outside.join("id_key"), t.ws.join("k1")).unwrap();
        assert!(
            t.ov.read(Path::new("k1")).is_err(),
            "a lower symlink isn't read through"
        );
        assert!(
            t.ov.read(Path::new("keys/id_key")).is_err(),
            "a symlinked lower directory isn't entered"
        );
        assert!(t.ov.read_dir(Path::new("keys")).is_err());
        let items = t.ov.read_dir(Path::new("")).unwrap();
        assert!(items.iter().all(|i| !i.is_dir || i.name != "keys"));
        assert!(items.iter().find(|i| i.name == "keys").unwrap().is_link);
        if crate::testutil::have_bwrap() {
            crate::testutil::bwrap_exec(
                &t.ov,
                &format!("ln -s {} up", outside.join("id_key").display()),
            );
            assert!(
                t.ov.read(Path::new("up")).is_err(),
                "an upper symlink isn't read through"
            );
        }
    }

    #[test]
    fn accept_all_keep_ignored_never_overwrites_conflicts() {
        let t = setup();
        fs::write(t.ws.join(".gitignore"), "target/\n").unwrap();
        fs::write(t.ws.join("a.txt"), "1").unwrap();
        t.ov.write(Path::new("a.txt"), b"agent").unwrap();
        fs::write(t.ws.join("a.txt"), "shaun's edit").unwrap();
        let skipped = t.ov.accept_all(true).unwrap();
        assert_eq!(skipped, vec![PathBuf::from("a.txt")]);
        assert_eq!(
            fs::read_to_string(t.ws.join("a.txt")).unwrap(),
            "shaun's edit"
        );
    }

    #[test]
    fn symlink_changes_are_flagged_and_outside_targets_refused() {
        if !crate::testutil::have_bwrap() {
            return;
        }
        let t = setup();
        crate::testutil::bwrap_exec(&t.ov, "ln -s /etc/passwd pw; ln -s inside.txt ok");
        t.ov.snapshot_bases().unwrap();
        let ch = t.ov.changes();
        let pw = ch.iter().find(|c| c.path == Path::new("pw")).unwrap();
        assert_eq!(pw.link.as_deref(), Some(Path::new("/etc/passwd")));
        let d = t.ov.diff(Path::new("pw")).unwrap();
        assert!(
            d.iter()
                .flat_map(|h| h.lines.iter())
                .any(|(_, l)| l.contains("symlink → /etc/passwd"))
        );
        assert!(
            !d.iter()
                .flat_map(|h| h.lines.iter())
                .any(|(_, l)| l.contains("root:")),
            "target contents never shown"
        );
        assert!(matches!(
            t.ov.accept_file(Path::new("pw"), true),
            Err(OverlayError::Outside(_))
        ));
        t.ov.accept_file(Path::new("ok"), false).unwrap();
        assert!(
            fs::symlink_metadata(t.ws.join("ok"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn deleted_dir_conflicts_when_contents_changed() {
        if !crate::testutil::have_bwrap() {
            return;
        }
        let t = setup();
        fs::create_dir_all(t.ws.join("d")).unwrap();
        fs::write(t.ws.join("d/a"), "a").unwrap();
        crate::testutil::bwrap_exec(&t.ov, "rm -rf d");
        t.ov.snapshot_bases().unwrap();
        assert!(!t.ov.changes()[0].conflict);
        fs::write(t.ws.join("d/new-by-shaun"), "keep me").unwrap();
        let c = t.ov.changes();
        assert!(c[0].conflict, "{c:?}");
        assert!(matches!(
            t.ov.accept_file(Path::new("d"), false),
            Err(OverlayError::Conflict(_))
        ));
        assert!(t.ws.join("d/new-by-shaun").exists());
    }

    #[test]
    fn disk_diff_shows_edits_made_on_disk() {
        let t = setup();
        fs::write(t.ws.join("a"), "one\n").unwrap();
        t.ov.write(Path::new("a"), b"agent\n").unwrap();
        fs::write(t.ws.join("a"), "shaun\n").unwrap();
        let d = t.ov.disk_diff(Path::new("a")).unwrap();
        let lines: Vec<String> = d
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|(c, l)| format!("{c}{l}"))
            .collect();
        assert_eq!(lines, ["-one", "+shaun"]);
    }

    #[test]
    fn accept_refuses_creating_dirs_through_outside_symlink() {
        let t = setup();
        let outside = t._tmp.path().join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, t.ws.join("ln")).unwrap();
        // A file staged under the link's name (as if the link had been a dir in the layer).
        fs::create_dir_all(t.ov.upper().join("ln/sub")).unwrap();
        fs::write(t.ov.upper().join("ln/sub/f"), "x").unwrap();
        assert!(t.ov.accept_file(Path::new("ln/sub/f"), true).is_err());
        assert!(
            !outside.join("sub").exists(),
            "no directories created outside"
        );
    }

    #[test]
    fn discard_after_a_real_mount_removes_everything() {
        if !crate::testutil::have_bwrap() {
            return;
        }
        let t = setup();
        crate::testutil::bwrap_exec(&t.ov, "echo x > f");
        let dir = t.ov.dir().to_path_buf();
        t.ov.clone().discard().unwrap();
        assert!(!dir.exists());
    }
}
