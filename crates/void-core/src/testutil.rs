//! Helpers shared by unit tests.

use std::path::Path;

use crate::overlay::Overlay;

pub(crate) fn have_bwrap() -> bool {
    let ok = Path::new("/usr/bin/bwrap").exists();
    if !ok {
        eprintln!("skipping: bwrap not installed");
    }
    ok
}

/// Run `script` in the overlay's workspace through a real overlay mount.
pub(crate) fn bwrap_exec(ov: &Overlay, script: &str) {
    let ws = ov.workspace().to_str().unwrap().to_string();
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
        .args(["--overlay-src", &ws, "--overlay"])
        .arg(ov.upper())
        .arg(ov.work())
        .args([&ws, "--chdir", &ws, "bash", "-c", script])
        .status()
        .unwrap();
    assert!(st.success(), "bwrap script failed: {script}");
}
