//! The sandbox every agent shell command runs in: bubblewrap with the whole filesystem
//! read-only, the workspace mounted as the conversation's overlay, sensitive paths hidden,
//! a scrubbed environment, and (when the helper is present) Landlock as a second layer.

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// What the model gets back from a command: the last 30 000 bytes.
pub const OUTPUT_CAP: usize = 30_000;
const HELPER: &str = "void-sandbox-exec";
const ENV_PASS: [&str; 6] = [
    "PATH",
    "USER",
    "LANG",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "PROTO_HOME",
];

/// `[sandbox]` in config.toml. Paths may start with `~/` and contain `$UID`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxSettings {
    /// Hidden behind an empty tmpfs (keys, browser profiles, sockets, void.ai's own data).
    pub hidden: Vec<String>,
    /// Exceptions to `hidden`.
    pub expose: Vec<String>,
    /// Package caches mounted with a shared void.ai-owned overlay so builds can download.
    pub caches: Vec<String>,
    pub network: bool,
    pub timeout_secs: u64,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect();
        Self {
            hidden: s(&[
                "~/.ssh",
                "~/.gnupg",
                "~/.aws",
                "~/.netrc",
                "~/.config/gh",
                "~/.local/share/keyrings",
                "~/.mozilla",
                "~/.config/chromium",
                "~/.config/google-chrome",
                "~/.config/opencode",
                "~/.config/void.ai",
                "~/.local/share/void.ai",
                "~/.claude",
                "~/.config/Claude",
                "/run/user/$UID",
                "/run/dbus",
                "/var/run/docker.sock",
                "/run/docker.sock",
                "/run/podman",
            ]),
            expose: Vec::new(),
            caches: s(&[
                "~/.cargo/registry",
                "~/.cargo/git",
                "~/.npm",
                "~/.cache/pip",
                "~/.cache/uv",
                "~/.proto/tools",
                "~/.rustup/toolchains",
            ]),
            network: true,
            timeout_secs: 120,
        }
    }
}

/// Expand `~/` and `$UID`.
pub fn expand(p: &str, home: &Path, uid: u32) -> PathBuf {
    let p = p.replace("$UID", &uid.to_string());
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if p == "~" => home.to_path_buf(),
        None => PathBuf::from(p),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    Ready { landlock: bool },
    Unavailable(String),
}

/// One command's mounts.
#[derive(Clone, Debug)]
pub struct RunPlan {
    pub workspace: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    /// Read-write CARGO_TARGET_DIR (per workspace, outside the overlay).
    pub target_dir: PathBuf,
    pub command: String,
    /// Overrides the settings' timeout (clamped to 1–600 s).
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecResult {
    pub exit: Option<i32>,
    /// Merged stdout+stderr, capped to the last OUTPUT_CAP bytes.
    pub output: String,
    pub timed_out: bool,
    pub cancelled: bool,
    pub cut_bytes: usize,
}

#[derive(Clone)]
pub struct Sandbox {
    bwrap: Option<PathBuf>,
    helper: Option<PathBuf>,
    data_dir: PathBuf,
    home: PathBuf,
    uid: u32,
    settings: SandboxSettings,
    /// Cache overlays can't be mounted twice at once: commands take turns.
    lock: Arc<tokio::sync::Mutex<()>>,
    avail: Arc<tokio::sync::OnceCell<Availability>>,
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

fn find_helper() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VOID_AI_SANDBOX_EXEC") {
        return Some(PathBuf::from(p)).filter(|p| p.is_file());
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join(HELPER), dir.parent()?.join(HELPER)]
        .into_iter()
        .find(|p| p.is_file())
}

fn current_uid() -> u32 {
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}

/// Keep the tail of the output; returns (text, bytes cut from the front).
pub fn cap_output(bytes: &[u8]) -> (String, usize) {
    if bytes.len() <= OUTPUT_CAP {
        return (String::from_utf8_lossy(bytes).into_owned(), 0);
    }
    let mut start = bytes.len() - OUTPUT_CAP;
    while start < bytes.len() && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    (String::from_utf8_lossy(&bytes[start..]).into_owned(), start)
}

impl Sandbox {
    /// Find bwrap on PATH and the Landlock helper next to the running binary.
    pub fn new(data_dir: PathBuf, home: PathBuf, settings: SandboxSettings) -> Sandbox {
        Self::with_paths(
            find_on_path("bwrap"),
            find_helper(),
            data_dir,
            home,
            settings,
        )
    }

    pub fn with_paths(
        bwrap: Option<PathBuf>,
        helper: Option<PathBuf>,
        data_dir: PathBuf,
        home: PathBuf,
        settings: SandboxSettings,
    ) -> Sandbox {
        Sandbox {
            bwrap,
            helper,
            data_dir,
            home,
            uid: current_uid(),
            settings,
            lock: Arc::new(tokio::sync::Mutex::new(())),
            avail: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    pub fn settings(&self) -> &SandboxSettings {
        &self.settings
    }

    fn exp(&self, p: &str) -> PathBuf {
        expand(p, &self.home, self.uid)
    }

    /// Hidden paths (that exist and aren't exposed).
    pub fn hidden_paths(&self) -> Vec<PathBuf> {
        let expose: Vec<PathBuf> = self.settings.expose.iter().map(|p| self.exp(p)).collect();
        self.settings
            .hidden
            .iter()
            .map(|p| self.exp(p))
            .filter(|p| !expose.contains(p))
            .collect()
    }

    fn caches(&self) -> Vec<(PathBuf, PathBuf)> {
        self.settings
            .caches
            .iter()
            .map(|c| self.exp(c))
            .filter(|c| c.is_dir())
            .map(|c| {
                let name =
                    hex::encode(Sha256::digest(c.as_os_str().as_encoded_bytes()))[..12].to_string();
                (c, self.data_dir.join("cache-overlay").join(name))
            })
            .collect()
    }

    /// Where cache layers live (for "Clear sandbox caches").
    pub fn cache_layer_dir(&self) -> PathBuf {
        self.data_dir.join("cache-overlay")
    }

    /// Whether bwrap works here and Landlock can be applied (checked once, then remembered).
    pub async fn availability(&self) -> Availability {
        self.avail.get_or_init(|| self.probe()).await.clone()
    }

    async fn probe(&self) -> Availability {
        let Some(bwrap) = &self.bwrap else {
            return Availability::Unavailable("bubblewrap (bwrap) isn't installed".into());
        };
        let probe = tokio::process::Command::new(bwrap)
            .args(["--ro-bind", "/", "/", "--unshare-pid", "--", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        match tokio::time::timeout(Duration::from_secs(5), probe).await {
            Ok(Ok(o)) if o.status.success() => {}
            Ok(Ok(o)) => {
                return Availability::Unavailable(format!(
                    "bubblewrap can't create a sandbox here: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
            }
            Ok(Err(e)) => return Availability::Unavailable(format!("can't run bubblewrap: {e}")),
            Err(_) => return Availability::Unavailable("bubblewrap didn't answer".into()),
        }
        let landlock = match &self.helper {
            Some(h) => tokio::process::Command::new(h)
                .arg("--probe")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|s| s.success()),
            None => false,
        };
        Availability::Ready { landlock }
    }

    /// The full bwrap argument list for one command.
    pub fn bwrap_args(&self, plan: &RunPlan) -> Vec<OsString> {
        let mut a: Vec<OsString> = Vec::new();
        let mut push = |xs: &[&std::ffi::OsStr]| a.extend(xs.iter().map(|x| x.to_os_string()));
        push(&[
            "--die-with-parent".as_ref(),
            "--new-session".as_ref(),
            "--unshare-pid".as_ref(),
            "--unshare-ipc".as_ref(),
            "--unshare-uts".as_ref(),
        ]);
        if !self.settings.network {
            push(&["--unshare-net".as_ref()]);
        }
        push(&[
            "--ro-bind".as_ref(),
            "/".as_ref(),
            "/".as_ref(),
            "--dev".as_ref(),
            "/dev".as_ref(),
            "--proc".as_ref(),
            "/proc".as_ref(),
            "--tmpfs".as_ref(),
            "/tmp".as_ref(),
        ]);
        // Mount over the real location (bwrap can't mount through symlinks such as
        // /var/run → /run): directories get an empty tmpfs, files and sockets /dev/null.
        let mut seen = std::collections::HashSet::new();
        for h in self.hidden_paths() {
            let Ok(real) = std::fs::canonicalize(&h) else {
                continue;
            };
            if !seen.insert(real.clone()) {
                continue;
            }
            if real.is_dir() {
                push(&["--tmpfs".as_ref(), real.as_os_str()]);
            } else {
                push(&["--ro-bind".as_ref(), "/dev/null".as_ref(), real.as_os_str()]);
            }
        }
        let caches = self.caches();
        for (cache, layer) in &caches {
            let (up, work) = (layer.join("upper"), layer.join("work"));
            push(&[
                "--overlay-src".as_ref(),
                cache.as_os_str(),
                "--overlay".as_ref(),
                up.as_os_str(),
                work.as_os_str(),
                cache.as_os_str(),
            ]);
        }
        let ws = plan.workspace.as_os_str();
        push(&[
            "--overlay-src".as_ref(),
            ws,
            "--overlay".as_ref(),
            plan.upper.as_os_str(),
            plan.work.as_os_str(),
            ws,
        ]);
        let git = plan.workspace.join(".git");
        if git.exists() {
            push(&["--ro-bind".as_ref(), git.as_os_str(), git.as_os_str()]);
        }
        let target = plan.target_dir.as_os_str();
        push(&["--bind".as_ref(), target, target]);
        push(&["--clearenv".as_ref()]);
        for k in ENV_PASS {
            if let Some(v) = std::env::var_os(k) {
                push(&["--setenv".as_ref(), k.as_ref(), v.as_os_str()]);
            }
        }
        push(&["--setenv".as_ref(), "HOME".as_ref(), self.home.as_os_str()]);
        push(&["--setenv".as_ref(), "TERM".as_ref(), "dumb".as_ref()]);
        push(&["--setenv".as_ref(), "CARGO_TARGET_DIR".as_ref(), target]);
        push(&["--chdir".as_ref(), ws, "--".as_ref()]);
        if let Some(helper) = &self.helper {
            push(&[helper.as_os_str()]);
            let mut writes: Vec<&std::ffi::OsStr> =
                vec![ws, "/tmp".as_ref(), "/dev".as_ref(), target];
            for (cache, _) in &caches {
                writes.push(cache.as_os_str());
            }
            for w in writes {
                push(&["--write".as_ref(), w]);
            }
            push(&["--".as_ref()]);
        }
        push(&["bash".as_ref(), "-lc".as_ref(), plan.command.as_ref()]);
        a
    }

    /// Run one command; output streams to `stream` as it arrives.
    pub async fn run(
        &self,
        plan: RunPlan,
        cancel: CancellationToken,
        stream: Option<UnboundedSender<String>>,
    ) -> ExecResult {
        let fail = |msg: String| ExecResult {
            exit: None,
            output: msg,
            timed_out: false,
            cancelled: false,
            cut_bytes: 0,
        };
        let Some(bwrap) = &self.bwrap else {
            return fail("bubblewrap (bwrap) isn't installed".into());
        };
        let caches = self.caches();
        let _turn = if caches.is_empty() {
            None
        } else {
            Some(self.lock.lock().await)
        };
        for dir in [&plan.target_dir, &plan.upper, &plan.work] {
            if let Err(e) = std::fs::create_dir_all(dir) {
                return fail(format!("can't prepare {}: {e}", dir.display()));
            }
        }
        for (_, layer) in &caches {
            let _ = std::fs::create_dir_all(layer.join("upper"));
            let _ = std::fs::create_dir_all(layer.join("work"));
        }
        let mut child = match tokio::process::Command::new(bwrap)
            .args(self.bwrap_args(&plan))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return fail(format!("can't start bubblewrap: {e}")),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        if let Some(out) = child.stdout.take() {
            pump(out, tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            pump(err, tx.clone());
        }
        drop(tx);
        let timeout = Duration::from_secs(
            plan.timeout_secs
                .unwrap_or(self.settings.timeout_secs)
                .clamp(1, 600),
        );
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let mut out: Vec<u8> = Vec::new();
        let mut dropped = 0usize;
        let (mut timed_out, mut cancelled) = (false, false);
        let collect = |chunk: Vec<u8>, out: &mut Vec<u8>, dropped: &mut usize| {
            if let Some(s) = &stream {
                let _ = s.send(String::from_utf8_lossy(&chunk).into_owned());
            }
            out.extend_from_slice(&chunk);
            // Bound memory: keep at most 4× the cap.
            if out.len() > OUTPUT_CAP * 4 {
                let cut = out.len() - OUTPUT_CAP * 2;
                out.drain(..cut);
                *dropped += cut;
            }
        };
        // Pipes close when the sandbox's PID namespace is gone, so read until then.
        loop {
            tokio::select! {
                chunk = rx.recv() => match chunk {
                    Some(c) => collect(c, &mut out, &mut dropped),
                    None => break,
                },
                _ = &mut deadline, if !timed_out && !cancelled => {
                    timed_out = true;
                    let _ = child.start_kill();
                }
                _ = cancel.cancelled(), if !timed_out && !cancelled => {
                    cancelled = true;
                    let _ = child.start_kill();
                }
            }
        }
        let status = child.wait().await.ok();
        while let Ok(c) = rx.try_recv() {
            collect(c, &mut out, &mut dropped);
        }
        let (output, cut) = cap_output(&out);
        ExecResult {
            exit: status.and_then(|s| s.code()),
            output,
            timed_out,
            cancelled,
            cut_bytes: cut + dropped,
        }
    }
}

fn pump<R: tokio::io::AsyncRead + Unpin + Send + 'static>(mut r: R, tx: UnboundedSender<Vec<u8>>) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn strs(v: &[std::ffi::OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    fn pos(args: &[String], seq: &[&str]) -> Option<usize> {
        args.windows(seq.len())
            .position(|w| w.iter().zip(seq).all(|(a, b)| a == b))
    }

    #[test]
    fn settings_expand_tilde_and_uid() {
        let home = Path::new("/home/x");
        assert_eq!(expand("~/.ssh", home, 1000), PathBuf::from("/home/x/.ssh"));
        assert_eq!(
            expand("/run/user/$UID", home, 1000),
            PathBuf::from("/run/user/1000")
        );
        assert_eq!(expand("/abs", home, 1000), PathBuf::from("/abs"));
    }

    #[test]
    fn defaults_cover_the_spec_lists() {
        let s = SandboxSettings::default();
        for h in [
            "~/.ssh",
            "~/.gnupg",
            "~/.config/void.ai",
            "~/.local/share/void.ai",
            "/run/user/$UID",
            "/run/dbus",
            "/var/run/docker.sock",
        ] {
            assert!(s.hidden.iter().any(|x| x == h), "{h}");
        }
        assert!(s.caches.iter().any(|x| x == "~/.cargo/registry"));
        assert!(s.network);
        assert_eq!(s.timeout_secs, 120);
    }

    #[test]
    fn bwrap_args_order_and_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(home.join(".cargo/registry")).unwrap();
        fs::create_dir_all(ws.join(".git")).unwrap();
        let settings = SandboxSettings {
            hidden: vec!["~/.ssh".into(), "~/.missing".into()],
            caches: vec!["~/.cargo/registry".into()],
            ..SandboxSettings::default()
        };
        // SAFETY: test-only variable with a unique value.
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-void-test-secret") };
        let sb = Sandbox::with_paths(
            Some("/usr/bin/bwrap".into()),
            None,
            tmp.path().join("data"),
            home.clone(),
            settings.clone(),
        );
        let plan = RunPlan {
            workspace: ws.clone(),
            upper: tmp.path().join("ovl/upper"),
            work: tmp.path().join("ovl/work"),
            target_dir: tmp.path().join("target"),
            command: "echo hi".into(),
            timeout_secs: None,
        };
        let a = strs(&sb.bwrap_args(&plan));
        let p = |x: &Path| x.to_string_lossy().into_owned();
        assert!(pos(&a, &["--ro-bind", "/", "/"]).is_some());
        assert!(pos(&a, &["--tmpfs", &p(&home.join(".ssh"))]).is_some());
        assert!(pos(&a, &["--tmpfs", &p(&home.join(".missing"))]).is_none());
        let src = pos(&a, &["--overlay-src", &p(&ws)]).unwrap();
        let ovl = pos(&a, &["--overlay", &p(&plan.upper), &p(&plan.work), &p(&ws)]).unwrap();
        assert!(src < ovl);
        let git = pos(
            &a,
            &["--ro-bind", &p(&ws.join(".git")), &p(&ws.join(".git"))],
        )
        .unwrap();
        assert!(git > ovl);
        assert!(pos(&a, &["--overlay-src", &p(&home.join(".cargo/registry"))]).is_some());
        assert!(pos(&a, &["--bind", &p(&plan.target_dir), &p(&plan.target_dir)]).is_some());
        assert!(!a.iter().any(|x| x == "--unshare-net"));
        assert!(a.iter().any(|x| x == "--clearenv"));
        assert!(!a.iter().any(|x| x.contains("sk-void-test-secret")));
        assert!(pos(&a, &["--setenv", "HOME", &p(&home)]).is_some());
        assert!(pos(&a, &["--setenv", "CARGO_TARGET_DIR", &p(&plan.target_dir)]).is_some());
        assert!(pos(&a, &["--chdir", &p(&ws)]).is_some());
        assert_eq!(&a[a.len() - 3..], ["bash", "-lc", "echo hi"]);

        let offline = Sandbox::with_paths(
            Some("/usr/bin/bwrap".into()),
            Some("/x/void-sandbox-exec".into()),
            tmp.path().join("data"),
            home.clone(),
            SandboxSettings {
                network: false,
                ..settings
            },
        );
        let b = strs(&offline.bwrap_args(&plan));
        assert!(b.iter().any(|x| x == "--unshare-net"));
        let h = pos(&b, &["/x/void-sandbox-exec"]).unwrap();
        assert!(pos(&b[h..], &["--write", &p(&ws)]).is_some());
        assert!(pos(&b[h..], &["--write", "/tmp"]).is_some());
        assert!(pos(&b[h..], &["--write", &p(&plan.target_dir)]).is_some());
        assert_eq!(&b[b.len() - 4..], ["--", "bash", "-lc", "echo hi"]);
    }

    #[test]
    fn exposed_paths_are_not_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        let settings = SandboxSettings {
            hidden: vec!["~/.ssh".into()],
            expose: vec!["~/.ssh".into()],
            caches: vec![],
            ..SandboxSettings::default()
        };
        let sb = Sandbox::with_paths(
            Some("/usr/bin/bwrap".into()),
            None,
            tmp.path().join("d"),
            home.clone(),
            settings,
        );
        let plan = RunPlan {
            workspace: tmp.path().join("ws"),
            upper: tmp.path().join("u"),
            work: tmp.path().join("w"),
            target_dir: tmp.path().join("t"),
            command: "true".into(),
            timeout_secs: None,
        };
        let a = strs(&sb.bwrap_args(&plan));
        assert!(pos(&a, &["--tmpfs", &home.join(".ssh").to_string_lossy()]).is_none());
    }

    #[test]
    fn hidden_paths_resolve_symlinks_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
        fs::write(tmp.path().join("sock"), "").unwrap();
        let p = |x: &Path| x.to_string_lossy().into_owned();
        let settings = SandboxSettings {
            hidden: vec![
                p(&tmp.path().join("link")),
                p(&tmp.path().join("real")),
                p(&tmp.path().join("sock")),
            ],
            caches: vec![],
            ..SandboxSettings::default()
        };
        let sb = Sandbox::with_paths(
            Some("/usr/bin/bwrap".into()),
            None,
            tmp.path().join("d"),
            tmp.path().join("h"),
            settings,
        );
        let plan = RunPlan {
            workspace: tmp.path().join("ws"),
            upper: tmp.path().join("u"),
            work: tmp.path().join("w"),
            target_dir: tmp.path().join("t"),
            command: "true".into(),
            timeout_secs: None,
        };
        let a = strs(&sb.bwrap_args(&plan));
        let real_c = p(&real.canonicalize().unwrap());
        assert_eq!(
            a.windows(2)
                .filter(|w| w[0] == "--tmpfs" && w[1] == real_c)
                .count(),
            1,
            "deduplicated"
        );
        assert!(
            pos(
                &a,
                &[
                    "--ro-bind",
                    "/dev/null",
                    &p(&tmp.path().join("sock").canonicalize().unwrap())
                ]
            )
            .is_some()
        );
    }

    #[test]
    fn output_cap_keeps_the_tail() {
        let s = "x".repeat(10) + &"y".repeat(OUTPUT_CAP);
        let (out, cut) = cap_output(s.as_bytes());
        assert_eq!(cut, 10);
        assert!(out.starts_with('y'));
        assert_eq!(out.len(), OUTPUT_CAP);
    }
}
