use crate::buffer::{Buffer, LineEnding};
use std::fs;
use std::io;
use std::path::Path;
use void_core::fsutil::atomic_write;

impl Buffer {
    /// Load `path`. Invalid UTF-8 or a NUL in the first 8 KiB opens lossily, read-only, with a notice, so saving cannot
    /// corrupt the file.
    pub fn from_file(path: &Path) -> io::Result<Buffer> {
        let bytes = fs::read(path)?;
        let has_nul = bytes.iter().take(8192).any(|&b| b == 0);
        let (text, valid) = match String::from_utf8(bytes) {
            Ok(text) => (text, true),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), false),
        };
        let mut b = Buffer::new(&text);
        if has_nul || !valid {
            b.read_only = true;
            b.notice = Some(if valid {
                "Binary file (contains NUL): opened read-only".to_string()
            } else {
                "Not valid UTF-8: opened read-only".to_string()
            });
        }
        Ok(b)
    }

    /// Write atomically, restoring the file's line ending and final newline, then mark saved.
    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "buffer is read-only",
            ));
        }
        let eol = match self.line_ending {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        };
        let mut out = String::with_capacity(self.rope().len_bytes() + 16);
        for chunk in self.rope().chunks() {
            out.push_str(chunk);
        }
        if self.final_newline {
            out.push('\n');
        }
        let out = if eol == "\n" {
            out
        } else {
            out.replace('\n', eol)
        };
        atomic_write(path, out.as_bytes())?;
        self.mark_saved();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    #[test]
    fn crlf_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        let bytes = b"one\r\ntwo\r\nthree\r\n";
        fs::write(&p, bytes).unwrap();
        let mut b = Buffer::from_file(&p).unwrap();
        assert_eq!(b.line_ending, LineEnding::CrLf);
        assert!(!b.rope().to_string().contains('\r'));
        assert_eq!(b.line_count(), 3);
        b.save(&p).unwrap();
        assert_eq!(fs::read(&p).unwrap(), bytes);
        b.replace(0..0, "x");
        b.save(&p).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"xone\r\ntwo\r\nthree\r\n");
    }

    #[test]
    fn final_newline_preserved() {
        let dir = tempfile::tempdir().unwrap();
        for bytes in [&b"a\nb\n"[..], b"a\nb", b"", b"\n"] {
            let p = dir.path().join("f.txt");
            fs::write(&p, bytes).unwrap();
            let mut b = Buffer::from_file(&p).unwrap();
            assert_eq!(b.final_newline, bytes.ends_with(b"\n"));
            b.save(&p).unwrap();
            assert_eq!(fs::read(&p).unwrap(), bytes);
        }
    }

    #[test]
    fn loads_large_file_quickly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.txt");
        let mut s = String::new();
        for i in 0..100_000 {
            s.push_str(&format!("line {i:06} with some padding text ok\n"));
        }
        fs::write(&p, &s).unwrap();
        let t = Instant::now();
        let b = Buffer::from_file(&p).unwrap();
        let el = t.elapsed();
        println!("from_file of 100000 lines took {el:?}");
        assert_eq!(b.line_count(), 100_000);
        assert!(el.as_millis() < 1000, "took {el:?}");
    }

    #[test]
    fn non_utf8_opens_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bin");
        fs::write(&p, b"ok \xff\xfe end").unwrap();
        let mut b = Buffer::from_file(&p).unwrap();
        assert!(b.read_only);
        assert!(b.notice.is_some());
        assert!(b.rope().to_string().starts_with("ok "));
        assert!(b.save(&p).is_err());
        assert_eq!(fs::read(&p).unwrap(), b"ok \xff\xfe end");
    }

    #[test]
    fn binary_with_nul_opens_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("elf");
        fs::write(&p, b"ELF\0\x01abc").unwrap();
        let mut b = Buffer::from_file(&p).unwrap();
        assert!(b.read_only);
        assert!(b.notice.is_some());
        assert!(b.save(&p).is_err());
    }

    #[test]
    fn save_keeps_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("run.sh");
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        let mut b = Buffer::from_file(&p).unwrap();
        b.replace(0..0, "# hi\n");
        b.save(&p).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!b.is_dirty());
    }
}
