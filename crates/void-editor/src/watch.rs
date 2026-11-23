//! Noticing files that change on disk behind the editor's back (another editor, `git checkout`,
//! accepting an agent's changes), and telling those apart from the editor's own saves.
//!
//! [`Watcher`] wraps `notify`: raw events are coalesced for [`DEBOUNCE`] and handed out as
//! paths on a channel the UI drains once a frame. [`Fingerprint`] remembers what a buffer last
//! loaded or saved, so [`check`] can say whether the file still holds it.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

/// Events are held until none has arrived for this long…
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// …but never longer than this, so a steady stream (a build writing `target/`) still flows.
const MAX_DELAY: Duration = Duration::from_millis(1000);

/// What a buffer last read from or wrote to its file: enough to tell "someone else changed
/// it" from "that was our own save" without keeping a copy of the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub hash: u64,
}

impl Fingerprint {
    /// `meta` is the file's metadata from just before `bytes` were read (or just after they
    /// were written).
    pub fn new(meta: &fs::Metadata, bytes: &[u8]) -> Fingerprint {
        Fingerprint {
            len: bytes.len() as u64,
            modified: meta.modified().ok(),
            hash: xxhash_rust::xxh3::xxh3_64(bytes),
        }
    }

    fn same_stat(&self, meta: &fs::Metadata) -> bool {
        self.len == meta.len() && self.modified.is_some() && self.modified == meta.modified().ok()
    }
}

/// The file compared with a [`Fingerprint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDisk {
    /// It still holds what the buffer has; the fingerprint is refreshed (a `touch` or a rewrite
    /// with the same bytes changes the timestamp only).
    Unchanged(Fingerprint),
    /// Someone else wrote something different.
    Changed(Fingerprint),
    /// It's gone.
    Missing,
}

/// Compare `path` with what the buffer last loaded or saved. A size and timestamp match is
/// trusted; otherwise the bytes are hashed, so only real content changes count.
pub fn check(path: &Path, last: Option<&Fingerprint>) -> io::Result<OnDisk> {
    let meta = match fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(OnDisk::Missing),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(OnDisk::Missing),
        Err(e) => return Err(e),
    };
    if let Some(last) = last
        && last.same_stat(&meta)
    {
        return Ok(OnDisk::Unchanged(*last));
    }
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(OnDisk::Missing),
        Err(e) => return Err(e),
    };
    let now = Fingerprint::new(&meta, &bytes);
    Ok(match last {
        Some(last) if last.len == now.len && last.hash == now.hash => OnDisk::Unchanged(now),
        _ => OnDisk::Changed(now),
    })
}

/// Watches folders (recursively) and single directories (not recursively), and hands out the
/// paths that changed, coalesced.
pub struct Watcher {
    inner: RecommendedWatcher,
    /// What's watched, for "something was lost, look again at everything" (queue overflow).
    roots: Arc<Mutex<Vec<PathBuf>>>,
    rx: Receiver<PathBuf>,
}

impl Watcher {
    /// Watch `paths` recursively. `wake` runs whenever paths are ready (request a repaint).
    /// A recursive watch registers every directory below the path, so call this off the UI
    /// thread for a big folder.
    pub fn new(paths: &[PathBuf], wake: impl Fn() + Send + 'static) -> notify::Result<Watcher> {
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<PathBuf>>();
        let (tx, rx) = mpsc::channel();
        let roots = Arc::new(Mutex::new(Vec::new()));
        let all = roots.clone();
        // Symlinks aren't followed: a link to a big tree elsewhere would register all of it
        // (and run into the inotify watch limit).
        let config = notify::Config::default()
            .with_event_kinds(notify::EventKindMask::CORE)
            .with_follow_symlinks(false);
        let mut inner = RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                let paths = match res {
                    Ok(ev) if ev.need_rescan() => all.lock().map(|r| r.clone()).unwrap_or_default(),
                    Ok(ev) if matches!(ev.kind, EventKind::Access(_)) => return,
                    Ok(ev) => ev.paths,
                    Err(e) if !e.paths.is_empty() => e.paths,
                    Err(_) => all.lock().map(|r| r.clone()).unwrap_or_default(),
                };
                if !paths.is_empty() {
                    let _ = raw_tx.send(paths);
                }
            },
            config,
        )?;
        std::thread::Builder::new()
            .name("void-watch".into())
            .spawn(move || debounce(raw_rx, tx, wake))?;
        for p in paths {
            inner.watch(p, RecursiveMode::Recursive)?;
            roots.lock().unwrap().push(p.clone());
        }
        Ok(Watcher { inner, roots, rx })
    }

    /// Also watch `dir` and its direct children (a file open from outside the folder).
    pub fn watch_dir(&mut self, dir: &Path) -> notify::Result<()> {
        self.inner.watch(dir, RecursiveMode::NonRecursive)?;
        self.roots.lock().unwrap().push(dir.to_path_buf());
        Ok(())
    }

    pub fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.roots.lock().unwrap().retain(|p| p != path);
        self.inner.unwatch(path)
    }

    /// Every path reported since the last call, sorted and without repeats.
    pub fn drain(&self) -> Vec<PathBuf> {
        let set: BTreeSet<PathBuf> = self.rx.try_iter().collect();
        set.into_iter().collect()
    }

    pub fn receiver(&self) -> &Receiver<PathBuf> {
        &self.rx
    }
}

/// Collect raw event paths until things go quiet, then pass each on once. Ends when the
/// watcher (and with it the raw sender) is dropped.
fn debounce(raw: Receiver<Vec<PathBuf>>, out: Sender<PathBuf>, wake: impl Fn()) {
    while let Ok(first) = raw.recv() {
        let started = Instant::now();
        let mut batch: BTreeSet<PathBuf> = first.into_iter().collect();
        let mut closed = false;
        loop {
            let left = MAX_DELAY.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            match raw.recv_timeout(DEBOUNCE.min(left)) {
                Ok(more) => batch.extend(more),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        for p in batch {
            if out.send(p).is_err() {
                return;
            }
        }
        wake();
        if closed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Drain until `want` shows up (watchers take a moment; never a fixed sleep).
    fn wait_for(w: &Watcher, want: &Path) -> Vec<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            if let Ok(p) = w.receiver().recv_timeout(Duration::from_millis(50)) {
                seen.push(p);
                seen.extend(w.drain());
                if seen.iter().any(|p| p == want) {
                    return seen;
                }
            }
        }
        panic!("no event for {} (saw {seen:?})", want.display());
    }

    #[test]
    fn watcher_reports_changes_coalesced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let w2 = wakes.clone();
        let w = Watcher::new(std::slice::from_ref(&root), move || {
            w2.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let f = root.join("sub/a.txt");
        for i in 0..20 {
            std::fs::write(&f, format!("v{i}")).unwrap();
        }
        let seen = wait_for(&w, &f);
        assert_eq!(
            seen.iter().filter(|p| **p == f).count(),
            1,
            "twenty writes, one path: {seen:?}"
        );
        assert!(wakes.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn watch_dir_sees_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut w = Watcher::new(&[], || {}).unwrap();
        w.watch_dir(&root).unwrap();
        let f = root.join("b.txt");
        std::fs::write(&f, "x").unwrap();
        wait_for(&w, &f);
    }

    fn bump_mtime(p: &Path) {
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(10))
            .unwrap();
    }

    /// A symlink to a folder elsewhere isn't followed (that folder isn't registered).
    #[test]
    fn watcher_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (root, outside) = (base.join("root"), base.join("outside"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let w = Watcher::new(std::slice::from_ref(&root), || {}).unwrap();
        std::fs::write(outside.join("x.txt"), "x").unwrap();
        // Events arrive in order: once the later write shows up, the earlier one would have.
        let sentinel = root.join("sentinel");
        std::fs::write(&sentinel, "s").unwrap();
        let seen = wait_for(&w, &sentinel);
        assert!(
            !seen.iter().any(|p| p.ends_with("x.txt")),
            "followed the link: {seen:?}"
        );
    }

    #[test]
    fn check_tells_own_bytes_from_new_ones() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "one").unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        let fp = Fingerprint::new(&meta, b"one");
        assert_eq!(check(&p, Some(&fp)).unwrap(), OnDisk::Unchanged(fp));
        // Same bytes rewritten with a new timestamp (set explicitly: filesystem timestamps
        // can be coarse): still ours, found by hashing.
        std::fs::write(&p, "one").unwrap();
        bump_mtime(&p);
        assert!(matches!(
            check(&p, Some(&fp)).unwrap(),
            OnDisk::Unchanged(_)
        ));
        // Different bytes (of a different length, so even an unchanged timestamp shows it).
        std::fs::write(&p, "three").unwrap();
        bump_mtime(&p);
        assert!(matches!(check(&p, Some(&fp)).unwrap(), OnDisk::Changed(_)));
        assert!(matches!(check(&p, None).unwrap(), OnDisk::Changed(_)));
        std::fs::remove_file(&p).unwrap();
        assert_eq!(check(&p, Some(&fp)).unwrap(), OnDisk::Missing);
    }
}
