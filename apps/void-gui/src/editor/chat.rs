//! Chat views: a tab showing a chat's version of a file (its merged overlay). Saving writes
//! the chat's pending changes through the engine (`Command::SaveToOverlay`), never the real
//! file; the engine refuses while the chat's turn runs. When the chat's changes move (the
//! agent wrote again, or Review accepted or rejected), clean views reload and dirty ones ask.

use std::io;
use std::path::{Path, PathBuf};

use void_core::model::{ConversationId, WorkspaceId};
use void_core::overlay::Overlay;
use void_core::store::Store;
use void_editor::{Buffer, Fingerprint};

use super::state::{ChatSave, DiskState, Doc, Msg};
use super::{DocKey, EditorStore, file_name};
use crate::layout::EditorView;

/// Refusal while the chat's agent turn runs (the engine says the same).
pub const BUSY: &str = "void is working in this chat — save to Disk, or wait";

/// Where the current folder workspace keeps its chats' pending changes.
#[derive(Clone, Debug)]
pub struct ChatOverlays {
    pub store: Store,
    pub workspace: WorkspaceId,
    pub root: PathBuf,
}

impl ChatOverlays {
    /// `path` relative to the workspace (None outside it).
    pub fn relative(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(&self.root)
            .ok()
            .filter(|r| !r.as_os_str().is_empty())
            .map(Path::to_path_buf)
    }

    /// The chat's version of `path`: its pending change, else the file on disk. Never follows
    /// a symlink (as the merged view doesn't), and never creates the chat's layer.
    pub fn read(&self, conv: ConversationId, path: &Path) -> io::Result<Vec<u8>> {
        let rel = self.relative(path).ok_or_else(|| {
            io::Error::other(format!("{} is outside the workspace", path.display()))
        })?;
        let dir = self.store.overlay_dir(&self.workspace, &conv);
        if dir.exists() {
            return Overlay::open(&dir, &self.root)?.read(&rel);
        }
        let full = self.root.join(&rel);
        if std::fs::symlink_metadata(&full)?.file_type().is_symlink() {
            return Err(io::Error::other(format!("{} is a symlink", rel.display())));
        }
        std::fs::read(full)
    }
}

/// A chat-view save for the app to send to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySave {
    pub request: u64,
    pub conversation: ConversationId,
    pub path: PathBuf,
    pub content: String,
}

fn chat_of(key: &DocKey) -> Option<ConversationId> {
    match key.1 {
        EditorView::Chat(c) => Some(c),
        EditorView::Disk => None,
    }
}

impl EditorStore {
    fn read_chat(&self, key: &DocKey) -> io::Result<Vec<u8>> {
        let conv = chat_of(key).ok_or_else(|| io::Error::other("not a chat view"))?;
        self.overlays
            .as_ref()
            .ok_or_else(|| io::Error::other("chat views need a folder workspace"))?
            .read(conv, &key.0)
    }

    /// Load a chat view's buffer from the chat's version of the file (non-text opens
    /// read-only, as on disk).
    pub(super) fn open_chat(&mut self, key: &DocKey) -> Result<(), String> {
        match self.read_chat(key) {
            Ok(bytes) => {
                let fp = Fingerprint::of_bytes(&bytes);
                let mut buffer = Buffer::from_bytes(bytes);
                buffer.disk = Some(fp);
                self.failed.remove(key);
                self.docs.insert(key.clone(), Doc::new(&key.0, buffer));
                Ok(())
            }
            Err(e) => {
                let msg = if e.kind() == io::ErrorKind::NotFound {
                    format!("{} isn't in void's version of the files", file_name(&key.0))
                } else {
                    format!("Couldn't open void's version of {}: {e}", key.0.display())
                };
                self.failed.insert(key.clone(), msg.clone());
                Err(msg)
            }
        }
    }

    /// Save a chat view: queue `SaveToOverlay` (the doc stays dirty until `OverlaySaved`).
    /// Unless `force`, a chat version that changed since the buffer read it isn't overwritten.
    pub fn save_chat(&mut self, key: &DocKey, force: bool) -> Result<String, String> {
        let Some(conv) = chat_of(key) else {
            return Err("not a chat view".into());
        };
        let busy = self.busy_chats.contains(&conv);
        let now = self.read_chat(key).ok().map(|b| Fingerprint::of_bytes(&b));
        let doc = self
            .docs
            .get_mut(key)
            .ok_or_else(|| "That file isn't open".to_string())?;
        let name = file_name(&key.0);
        let r = if doc.core.buffer.read_only {
            Err(format!("\"{name}\" is read-only"))
        } else if busy {
            Err(BUSY.to_string())
        } else if !force && now.is_some() && moved(doc, now) {
            doc.core.disk = DiskState::Changed;
            Err(format!(
                "void changed its version of \"{name}\" since it was read: Reload, or overwrite with :w!"
            ))
        } else {
            // A file gone from void's version is written back, as a Disk save recreates one.
            let content = doc.core.buffer.file_text();
            let request = self.next_request;
            self.next_request += 1;
            doc.chat_saves.push(ChatSave {
                request,
                version: doc.core.buffer.version(),
                fp: Fingerprint::of_bytes(content.as_bytes()),
            });
            self.overlay_saves.push(OverlaySave {
                request,
                conversation: conv,
                path: key.0.clone(),
                content,
            });
            Ok(format!("\"{name}\" saving to void's pending changes…"))
        };
        doc.core.message = Some(match &r {
            Ok(m) => Msg::info(m.clone()),
            Err(e) => Msg::error(e.clone()),
        });
        r
    }

    /// The save in flight that holds the buffer's current text, if any (closing waits for it).
    pub fn saving(&self, key: &DocKey) -> Option<u64> {
        let doc = self.docs.get(key)?;
        doc.chat_saves
            .last()
            .filter(|s| s.version == doc.core.buffer.version())
            .map(|s| s.request)
    }

    /// The engine wrote chat-view save `request`: the text it held is now the chat's version
    /// (later edits, or a later save still in flight, keep the buffer dirty).
    pub fn overlay_saved(&mut self, request: u64, conv: ConversationId, path: &Path) {
        let key: DocKey = (path.to_path_buf(), EditorView::Chat(conv));
        let Some(doc) = self.docs.get_mut(&key) else {
            return;
        };
        let Some(i) = doc.chat_saves.iter().position(|s| s.request == request) else {
            return;
        };
        // The engine answers in order: anything older was answered already.
        let save = doc.chat_saves[i];
        doc.chat_saves.drain(..=i);
        doc.core.buffer.mark_saved_at(save.version);
        doc.core.buffer.disk = Some(save.fp);
        doc.core.disk = DiskState::InSync;
        doc.core.kept = None;
        doc.core.message = Some(Msg::info(format!(
            "\"{}\" written to void's pending changes",
            file_name(path)
        )));
    }

    /// The engine refused or failed chat-view save `request`: nothing was written, the
    /// buffer stays dirty and says why.
    pub fn overlay_save_failed(
        &mut self,
        request: u64,
        conv: ConversationId,
        path: &Path,
        error: &str,
    ) {
        let key: DocKey = (path.to_path_buf(), EditorView::Chat(conv));
        let Some(doc) = self.docs.get_mut(&key) else {
            return;
        };
        let before = doc.chat_saves.len();
        doc.chat_saves.retain(|s| s.request != request);
        if doc.chat_saves.len() != before {
            doc.core.message = Some(Msg::error(error.to_string()));
        }
    }

    /// These chats' pending changes moved: re-read their views (clean ones reload, dirty ones
    /// get the banner; our own saves are recognised and left alone).
    pub fn check_overlays(&mut self, convs: &[ConversationId]) {
        let keys: Vec<DocKey> = self
            .docs
            .keys()
            .filter(|k| chat_of(k).is_some_and(|c| convs.contains(&c)))
            .cloned()
            .collect();
        for key in keys {
            let read = self.read_chat(&key);
            let Some(doc) = self.docs.get_mut(&key) else {
                continue;
            };
            if !doc.chat_saves.is_empty() {
                continue;
            }
            match read {
                Ok(bytes) => {
                    let fp = Fingerprint::of_bytes(&bytes);
                    if !moved(doc, Some(fp)) {
                        doc.core.disk = DiskState::InSync;
                    } else if doc.core.kept == Some(fp) {
                        // "Keep mine" said not to ask about this version again.
                    } else if !doc.core.buffer.is_dirty() {
                        reload(doc, bytes);
                        doc.core.message = Some(Msg::info(format!(
                            "void changed its version of \"{}\": reloaded",
                            file_name(&key.0)
                        )));
                    } else {
                        doc.core.disk = DiskState::Changed;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    doc.core.disk = DiskState::Deleted;
                }
                Err(_) => {}
            }
        }
    }

    /// `:e!` / Reload on a chat view: take void's version.
    pub(super) fn reload_chat(&mut self, key: &DocKey) -> Result<(), String> {
        let bytes = self
            .read_chat(key)
            .map_err(|e| format!("Couldn't reload void's version: {e}"))?;
        let doc = self
            .docs
            .get_mut(key)
            .ok_or_else(|| "That file isn't open".to_string())?;
        reload(doc, bytes);
        Ok(())
    }

    /// Keep mine on a chat view: stop asking about void's current version.
    pub(super) fn keep_mine_chat(&mut self, key: &DocKey) {
        let now = self.read_chat(key).ok().map(|b| Fingerprint::of_bytes(&b));
        if let Some(doc) = self.docs.get_mut(key) {
            doc.core.kept = now;
            doc.core.disk = DiskState::InSync;
        }
    }
}

/// The chat's version differs from what the buffer last read or saved.
fn moved(doc: &Doc, now: Option<Fingerprint>) -> bool {
    let last = doc.core.buffer.disk;
    match (last, now) {
        (Some(a), Some(b)) => (a.len, a.hash) != (b.len, b.hash),
        (None, None) => false,
        _ => true,
    }
}

fn reload(doc: &mut Doc, bytes: Vec<u8>) {
    let fp = Fingerprint::of_bytes(&bytes);
    let mut buffer = Buffer::from_bytes(bytes);
    buffer.disk = Some(fp);
    doc.reload_from(buffer);
}
