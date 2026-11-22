//! The sandbox really contains commands. Every test runs bwrap against canary files in temp
//! directories (never the real home) and skips with a note when the sandbox isn't available.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use void_core::sandbox::{Availability, ExecResult, RunPlan, Sandbox, SandboxSettings};

struct Rig {
    tmp: tempfile::TempDir,
    ws: PathBuf,
    sb: Sandbox,
    landlock: bool,
}

async fn rig_with(settings: impl FnOnce(&Path) -> SandboxSettings) -> Option<Rig> {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let home = tmp.path().join("home");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&home).unwrap();
    let s = settings(tmp.path());
    let sb = Sandbox::new(tmp.path().join("data"), home, s);
    match sb.availability().await {
        Availability::Ready { landlock } => Some(Rig {
            tmp,
            ws,
            sb,
            landlock,
        }),
        Availability::Unavailable(why) => {
            eprintln!("skipping: sandbox unavailable: {why}");
            None
        }
    }
}

async fn rig() -> Option<Rig> {
    rig_with(|_| SandboxSettings {
        caches: vec![],
        ..SandboxSettings::default()
    })
    .await
}

impl Rig {
    async fn run(&self, cmd: &str) -> ExecResult {
        let plan = RunPlan {
            workspace: self.ws.clone(),
            upper: self.tmp.path().join("ovl/upper"),
            work: self.tmp.path().join("ovl/work"),
            target_dir: self.tmp.path().join("target"),
            command: cmd.into(),
            timeout_secs: None,
        };
        fs::create_dir_all(&plan.upper).unwrap();
        fs::create_dir_all(&plan.work).unwrap();
        self.sb.run(plan, CancellationToken::new(), None).await
    }
}

#[tokio::test]
async fn sandbox_blocks_home_write() {
    let Some(r) = rig().await else { return };
    let outside = r.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let res = r
        .run(&format!("touch {}/new; echo rc=$?", outside.display()))
        .await;
    assert!(res.output.contains("rc=1"), "{}", res.output);
    assert!(!outside.join("new").exists());
}

#[tokio::test]
async fn sandbox_blocks_outside_delete() {
    let Some(r) = rig().await else { return };
    let canary = r.tmp.path().join("canary.txt");
    fs::write(&canary, "alive").unwrap();
    let res = r
        .run(&format!(
            "rm -rf {} ; rm -f {}; echo done",
            r.tmp.path().join("home").display(),
            canary.display()
        ))
        .await;
    assert!(res.output.contains("done"));
    assert_eq!(fs::read_to_string(&canary).unwrap(), "alive");
    assert!(r.tmp.path().join("home").exists());
}

#[tokio::test]
async fn sandbox_hides_run_user() {
    let Some(r) = rig().await else { return };
    let res = r.run("ls -A /run/user/$(id -u) 2>/dev/null | wc -l; systemd-run --user --scope true >/dev/null 2>&1; echo rc=$?").await;
    let mut lines = res.output.lines();
    assert_eq!(lines.next().map(str::trim), Some("0"), "{}", res.output);
    assert!(
        !res.output.contains("rc=0"),
        "systemd-run must fail: {}",
        res.output
    );
}

#[tokio::test]
async fn sandbox_hides_listed_dirs() {
    let Some(r) = rig_with(|tmp| SandboxSettings {
        hidden: vec![tmp.join("secrets").to_string_lossy().into_owned()],
        caches: vec![],
        ..SandboxSettings::default()
    })
    .await
    else {
        return;
    };
    let secrets = r.tmp.path().join("secrets");
    fs::create_dir_all(&secrets).unwrap();
    fs::write(secrets.join("key"), "TOPSECRET").unwrap();
    let res = r
        .run(&format!("cat {}/key; echo rc=$?", secrets.display()))
        .await;
    assert!(!res.output.contains("TOPSECRET"), "{}", res.output);
}

#[tokio::test]
async fn sandbox_git_is_read_only() {
    let Some(r) = rig().await else { return };
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&r.ws)
        .status();
    if !ok.is_ok_and(|s| s.success()) {
        eprintln!("skipping: git not available");
        return;
    }
    let res = r
        .run("touch .git/x; echo rc=$?; git status --porcelain >/dev/null 2>&1; echo st=$?")
        .await;
    assert!(res.output.contains("rc=1"), "{}", res.output);
    assert!(res.output.contains("st=0"), "{}", res.output);
    assert!(!r.ws.join(".git/x").exists());
    assert!(!r.tmp.path().join("ovl/upper/.git/x").exists());
}

#[tokio::test]
async fn sandbox_writes_land_in_upper() {
    let Some(r) = rig().await else { return };
    let res = r.run("echo hi > f && cat f").await;
    assert!(res.output.contains("hi"));
    assert_eq!(res.exit, Some(0));
    assert!(!r.ws.join("f").exists());
    assert!(r.tmp.path().join("ovl/upper/f").exists());
}

#[tokio::test]
async fn sandbox_timeout_kills() {
    let Some(r) = rig_with(|_| SandboxSettings {
        timeout_secs: 1,
        caches: vec![],
        ..SandboxSettings::default()
    })
    .await
    else {
        return;
    };
    let t = Instant::now();
    let res = r.run("sleep 30").await;
    assert!(res.timed_out);
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
}

#[tokio::test]
async fn sandbox_env_is_scrubbed() {
    let Some(r) = rig().await else { return };
    // SAFETY: test-only variable with a unique value.
    unsafe { std::env::set_var("OPENAI_API_KEY", "sk-void-sandbox-test") };
    let res = r.run("env").await;
    assert!(!res.output.contains("sk-void-sandbox-test"));
    assert!(res.output.contains("TERM=dumb"));
}

#[tokio::test]
async fn sandbox_landlock_blocks_abstract_socket() {
    let Some(r) = rig().await else { return };
    if !r.landlock {
        eprintln!("skipping: landlock helper not active");
        return;
    }
    if !Path::new("/usr/bin/python3").exists() {
        eprintln!("skipping: python3 not installed");
        return;
    }
    use std::os::linux::net::SocketAddrExt;
    let name = format!("void-sandbox-test-{}", std::process::id());
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
    listener.set_nonblocking(true).unwrap();
    let script = format!(
        "python3 -c \"import socket;s=socket.socket(socket.AF_UNIX);s.connect('\\0{name}');print('CONNECTED')\" 2>&1; echo rc=$?"
    );
    let res = r.run(&script).await;
    assert!(
        res.output.contains("rc="),
        "command must have run: {}",
        res.output
    );
    // Python echoes the failing source line, so look for the printed line itself.
    assert!(
        !res.output.lines().any(|l| l.trim() == "CONNECTED"),
        "{}",
        res.output
    );
    assert!(
        res.output.contains("Operation not permitted"),
        "Landlock should refuse: {}",
        res.output
    );
    drop(listener);
}
