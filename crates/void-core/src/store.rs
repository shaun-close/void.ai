//! Per-workspace storage under `~/.local/share/void.ai/`:
//!
//! ```text
//! index.json                                   known workspaces, newest first
//! workspaces/<id>/conversations/<conv>.json    one file per conversation
//! workspaces/<id>/layout.ron                   the UI's dock layout (opaque to the core)
//! ```

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fsutil;
use crate::model::{Conversation, ConversationId, ConversationMeta, Workspace, WorkspaceId};

const RECENT_MAX: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub workspace: Workspace,
    pub last_opened: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// `(config file, data dir)`: `~/.config/void.ai/config.toml` and `~/.local/share/void.ai`.
    pub fn default_paths() -> (PathBuf, PathBuf) {
        match directories::ProjectDirs::from("", "", "void.ai") {
            Some(d) => (
                d.config_dir().join("config.toml"),
                d.data_dir().to_path_buf(),
            ),
            None => (
                PathBuf::from("void.ai/config.toml"),
                PathBuf::from("void.ai"),
            ),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn workspace_dir(&self, ws: &WorkspaceId) -> PathBuf {
        self.data_dir.join("workspaces").join(&ws.0)
    }

    pub fn conversations_dir(&self, ws: &WorkspaceId) -> PathBuf {
        self.workspace_dir(ws).join("conversations")
    }

    fn conversation_path(&self, ws: &WorkspaceId, id: &ConversationId) -> PathBuf {
        self.conversations_dir(ws).join(format!("{id}.json"))
    }

    fn index_path(&self) -> PathBuf {
        self.data_dir.join("index.json")
    }

    /// The workspace for a project folder. Its id is the first 16 hex characters of the
    /// SHA-256 of the canonical path, so the same folder always maps to the same data.
    pub fn workspace_for(&self, root: &Path) -> io::Result<Workspace> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} isn't a folder", root.display()),
            ));
        }
        let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
        let id = hex::encode(digest)[..16].to_string();
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        Ok(Workspace {
            id: WorkspaceId(id),
            name,
            root: Some(root),
        })
    }

    /// Known workspaces, newest first. An unreadable index counts as empty.
    pub fn recent(&self) -> Vec<WorkspaceEntry> {
        std::fs::read(self.index_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Record that a workspace was opened now.
    pub fn touch(&self, ws: &Workspace) -> io::Result<()> {
        let mut list = self.recent();
        list.retain(|e| e.workspace.id != ws.id);
        list.insert(
            0,
            WorkspaceEntry {
                workspace: ws.clone(),
                last_opened: Utc::now(),
            },
        );
        list.truncate(RECENT_MAX);
        let json = serde_json::to_vec_pretty(&list).map_err(io::Error::other)?;
        fsutil::atomic_write(&self.index_path(), &json)
    }

    /// The most recently opened workspace whose folder still exists, else Scratch.
    pub fn last_workspace(&self) -> Workspace {
        self.recent()
            .into_iter()
            .map(|e| e.workspace)
            .find(|w| w.root.as_ref().is_none_or(|r| r.is_dir()))
            .unwrap_or_else(Workspace::scratch)
    }

    /// The workspace's conversations, newest first, plus a warning per file that couldn't be
    /// read (each such file is set aside).
    pub fn list(&self, ws: &WorkspaceId) -> (Vec<ConversationMeta>, Vec<String>) {
        let mut metas = Vec::new();
        let mut warnings = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.conversations_dir(ws)) else {
            return (metas, warnings);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            match read_conversation(&path) {
                Ok(c) => metas.push(c.meta()),
                Err(e) => {
                    let kept = match fsutil::set_aside(&path) {
                        Ok(bad) => format!("moved to {}", bad.display()),
                        Err(e2) => format!("couldn't be moved aside: {e2}"),
                    };
                    warnings.push(format!(
                        "Couldn't read conversation {} ({e}); {kept}.",
                        path.display()
                    ));
                }
            }
        }
        metas.sort_by_key(|m| std::cmp::Reverse(m.updated));
        (metas, warnings)
    }

    pub fn load(&self, ws: &WorkspaceId, id: &ConversationId) -> io::Result<Conversation> {
        read_conversation(&self.conversation_path(ws, id))
    }

    pub fn save(&self, c: &Conversation) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(c).map_err(io::Error::other)?;
        fsutil::atomic_write(&self.conversation_path(&c.workspace_id, &c.id), &json)
    }

    pub fn delete(&self, ws: &WorkspaceId, id: &ConversationId) -> io::Result<()> {
        match std::fs::remove_file(self.conversation_path(ws, id)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    pub fn load_layout(&self, ws: &WorkspaceId) -> Option<String> {
        std::fs::read_to_string(self.workspace_dir(ws).join("layout.ron")).ok()
    }

    pub fn save_layout(&self, ws: &WorkspaceId, json: &str) -> io::Result<()> {
        fsutil::atomic_write(&self.workspace_dir(ws).join("layout.ron"), json.as_bytes())
    }
}

fn read_conversation(path: &Path) -> io::Result<Conversation> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Conversation, Message, Workspace, WorkspaceId};
    use chrono::{Duration, Utc};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path().join("data"));
        (dir, s)
    }

    #[test]
    fn workspace_id_is_stable() {
        let (dir, s) = store();
        let a = dir.path().join("proj-a");
        let b = dir.path().join("proj-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let wa = s.workspace_for(&a).unwrap();
        let wa2 = s
            .workspace_for(&dir.path().join("proj-b/../proj-a/."))
            .unwrap();
        let wb = s.workspace_for(&b).unwrap();
        assert_eq!(wa.id, wa2.id);
        assert_ne!(wa.id, wb.id);
        assert_eq!(wa.id.0.len(), 16);
        assert_eq!(wa.name, "proj-a");
        assert_eq!(
            wa.root.as_deref(),
            Some(a.canonicalize().unwrap().as_path())
        );
    }

    #[test]
    fn missing_folder_is_an_error() {
        let (dir, s) = store();
        assert!(s.workspace_for(&dir.path().join("nope")).is_err());
    }

    #[test]
    fn save_load_roundtrip() {
        let (_d, s) = store();
        let mut c = Conversation::new(Workspace::scratch().id);
        c.messages.push(Message::user("hello"));
        s.save(&c).unwrap();
        assert_eq!(s.load(&c.workspace_id, &c.id).unwrap(), c);
    }

    #[test]
    fn conversations_are_scoped_to_workspace() {
        let (_d, s) = store();
        let c = Conversation::new(WorkspaceId("aaaa".into()));
        s.save(&c).unwrap();
        assert_eq!(s.list(&WorkspaceId("aaaa".into())).0.len(), 1);
        assert!(s.list(&WorkspaceId("bbbb".into())).0.is_empty());
    }

    #[test]
    fn list_sorts_newest_first() {
        let (_d, s) = store();
        let ws = Workspace::scratch().id;
        let mut old = Conversation::new(ws.clone());
        old.title = "old".into();
        old.updated = Utc::now() - Duration::hours(1);
        let mut new = Conversation::new(ws.clone());
        new.title = "new".into();
        s.save(&old).unwrap();
        s.save(&new).unwrap();
        let titles: Vec<_> = s.list(&ws).0.into_iter().map(|m| m.title).collect();
        assert_eq!(titles, ["new", "old"]);
    }

    #[test]
    fn delete_removes_conversation() {
        let (_d, s) = store();
        let c = Conversation::new(Workspace::scratch().id);
        s.save(&c).unwrap();
        s.delete(&c.workspace_id, &c.id).unwrap();
        assert!(s.list(&c.workspace_id).0.is_empty());
    }

    #[test]
    fn corrupt_conversation_is_set_aside() {
        let (_d, s) = store();
        let ws = Workspace::scratch().id;
        let good = Conversation::new(ws.clone());
        s.save(&good).unwrap();
        let dir = s.conversations_dir(&ws);
        std::fs::write(
            dir.join(format!("{}.json", crate::model::ConversationId::new())),
            "{oops",
        )
        .unwrap();
        let (metas, warnings) = s.list(&ws);
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, good.id);
        assert_eq!(warnings.len(), 1);
        let bad = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".json.bad-")
            })
            .count();
        assert_eq!(bad, 1);
        // Listing again doesn't warn twice: the bad file is out of the way.
        assert!(s.list(&ws).1.is_empty());
    }

    #[test]
    fn recent_is_newest_first_and_capped() {
        let (_d, s) = store();
        for i in 0..21 {
            let ws = Workspace {
                id: WorkspaceId(format!("{i:016}")),
                name: format!("w{i}"),
                root: Some(format!("/tmp/w{i}").into()),
            };
            s.touch(&ws).unwrap();
        }
        let recent = s.recent();
        assert_eq!(recent.len(), 20);
        assert_eq!(recent[0].workspace.name, "w20");
        assert_eq!(recent[19].workspace.name, "w1");
        // Touching an existing one moves it to the front without duplicating it.
        s.touch(&recent[5].workspace.clone()).unwrap();
        let again = s.recent();
        assert_eq!(again.len(), 20);
        assert_eq!(again[0].workspace.name, "w15");
    }

    #[test]
    fn last_workspace_defaults_to_scratch() {
        let (dir, s) = store();
        assert_eq!(s.last_workspace(), Workspace::scratch());
        let p = dir.path().join("proj");
        std::fs::create_dir_all(&p).unwrap();
        let ws = s.workspace_for(&p).unwrap();
        s.touch(&ws).unwrap();
        assert_eq!(s.last_workspace(), ws);
        // A folder that has since gone is skipped.
        std::fs::remove_dir(&p).unwrap();
        assert_eq!(s.last_workspace(), Workspace::scratch());
    }

    #[test]
    fn layout_roundtrip_per_workspace() {
        let (_d, s) = store();
        let a = WorkspaceId("aaaa".into());
        let b = WorkspaceId("bbbb".into());
        assert_eq!(s.load_layout(&a), None);
        s.save_layout(&a, "(x: 1)").unwrap();
        assert_eq!(s.load_layout(&a).as_deref(), Some("(x: 1)"));
        assert_eq!(s.load_layout(&b), None);
    }
}
