//! Small filesystem helpers: atomic writes and setting aside unreadable files.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Write `bytes` to `path` via a temporary file and a rename, creating parent directories.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = with_suffix(path, ".tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
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
}
