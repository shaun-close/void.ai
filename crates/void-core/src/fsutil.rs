//! Small filesystem helpers: atomic writes and setting aside unreadable files.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Write `bytes` to `path` via a fresh, exclusively created temporary file (random name, never
/// a symlink) and a rename, creating parent directories. Keeps an existing file's permissions;
/// new files get 0644.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic(path, bytes, None)
}

/// [`atomic_write`] for secrets: the file is always mode 0600, even when it replaces a looser one.
pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic(path, bytes, Some(0o600))
}

fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent)?;
    let perms = match mode {
        Some(m) => fs::Permissions::from_mode(m),
        None => fs::metadata(path)
            .map(|m| m.permissions())
            .unwrap_or_else(|_| fs::Permissions::from_mode(0o644)),
    };
    let mut tmp = tempfile::Builder::new()
        .prefix(".void-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.as_file().set_permissions(perms)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Remove a directory tree even when parts of it are unreadable (overlayfs leaves its
/// `work/work` directory with mode 000). Missing is fine.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fn open_up(p: &Path) {
        let Ok(m) = fs::symlink_metadata(p) else {
            return;
        };
        if m.is_dir() {
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    open_up(&e.path());
                }
            }
        }
    }
    if fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    open_up(path);
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Rename an unreadable file to `<file>.bad-<unix-ts>` so it's kept, never overwritten.
pub fn set_aside(path: &Path) -> io::Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut bad = with_suffix(path, &format!(".bad-{ts}"));
    let mut n = 1;
    while bad.exists() {
        bad = with_suffix(path, &format!(".bad-{ts}-{n}"));
        n += 1;
    }
    fs::rename(path, &bad)?;
    Ok(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_parents_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/c.txt");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn set_aside_renames_with_bad_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        std::fs::write(&p, b"junk").unwrap();
        let bad = set_aside(&p).unwrap();
        assert!(!p.exists());
        assert!(
            bad.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("x.json.bad-")
        );
        assert_eq!(std::fs::read(&bad).unwrap(), b"junk");
    }

    #[test]
    fn atomic_write_ignores_a_planted_tmp_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "untouched").unwrap();
        let p = dir.path().join("data");
        std::os::unix::fs::symlink(&outside, dir.path().join("data.tmp")).unwrap();
        atomic_write(&p, b"new").unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
        assert_eq!(fs::read_to_string(&p).unwrap(), "new");
        assert!(
            fs::symlink_metadata(dir.path().join("data.tmp")).is_ok(),
            "an unrelated .tmp file is left alone"
        );
    }

    #[test]
    fn remove_tree_handles_unreadable_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("t");
        fs::create_dir_all(root.join("work/work")).unwrap();
        fs::write(root.join("f"), "x").unwrap();
        fs::set_permissions(root.join("work/work"), fs::Permissions::from_mode(0o000)).unwrap();
        remove_tree(&root).unwrap();
        assert!(!root.exists());
        remove_tree(&root).unwrap(); // already gone is fine
    }
}
