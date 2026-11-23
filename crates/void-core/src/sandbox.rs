//! The sandbox every agent shell command runs in: bubblewrap with the whole filesystem
//! read-only, the workspace mounted as the conversation's overlay, sensitive paths hidden,
//! a scrubbed environment, and (when the helper is present) Landlock as a second layer.

use std::ffi::OsString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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
                "~/.claude.json",
                "~/.config/Claude",
                "~/.config/strata",
                "~/.local/share/opencode",
                "~/.npmrc",
                "~/.pypirc",
                "~/.cargo/credentials",
                "~/.cargo/credentials.toml",
                "~/.git-credentials",
                "~/.config/git/credentials",
                "~/.docker",
                "~/.kube",
                "~/.password-store",
                "~/.config/gcloud",
                "~/.azure",
                "~/.terraform.d",
                "~/.bash_history",
                "~/.zsh_history",
                "~/.python_history",
                "~/.local/share/fish/fish_history",
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

/// The chat overlay mounts a process gets (as in [`RunPlan`]).
#[derive(Clone, Debug)]
pub struct OverlayMounts {
    pub workspace: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    /// Read-write CARGO_TARGET_DIR (per workspace, outside the overlay).
    pub target_dir: PathBuf,
}

/// A read-write bind for a spawned process. A missing target is created first so the bind
/// has something to mount; a present one is never touched (not even read).
#[derive(Clone, Debug)]
pub enum RwBind {
    /// Created (with parents) as an empty directory when absent.
    Dir(PathBuf),
    /// Created with `if_missing` as its contents when absent.
    File { path: PathBuf, if_missing: String },
}

impl RwBind {
    pub fn path(&self) -> &Path {
        match self {
            RwBind::Dir(p) | RwBind::File { path: p, .. } => p,
        }
    }
}

/// A long-lived process (a coding agent CLI for one chat turn). Besides what [`RunPlan`]
/// gives a command, `$HOME` gets a throwaway writable layer (built per entry, see
/// `home_layer_args`; gone when the process exits) so the program can keep lock and temp
/// files next to its state; `rw_binds` sit on top of that layer and are written through to
/// the host.
#[derive(Clone, Debug)]
pub struct SpawnPlan {
    /// `None` for Scratch: a read-only view, plus `rw_binds` and `cwd` bound writable.
    pub overlay: Option<OverlayMounts>,
    pub argv: Vec<OsString>,
    /// Set on top of the scrubbed environment.
    pub env: Vec<(OsString, OsString)>,
    pub rw_binds: Vec<RwBind>,
    /// Paths dropped from the effective hidden list for this process only.
    pub unhide: Vec<PathBuf>,
    /// Working directory: inside the workspace, or (Scratch) a void.ai-owned directory that is
    /// created if missing.
    pub cwd: PathBuf,
}

/// A running [`Sandbox::spawn`] process. Dropping it kills the sandbox (SIGKILL).
///
/// The cache lock (when there are caches) is held until the sandbox is really gone, which is
/// later than bwrap's exit: bwrap returns as soon as the program does, while the kernel is
/// still tearing down the PID namespace and its overlays, and mounting the same layers again
/// in that window fails with EBUSY. A watcher thread waits on the namespace's init (from
/// bwrap's `--info-fd`, via a pidfd) and releases the lock then.
pub struct Spawned {
    /// bwrap itself, leader of its own process group; stdin, stdout and stderr are piped.
    pub child: tokio::process::Child,
    /// Turns true once the sandbox is gone and the cache lock released.
    gone: tokio::sync::watch::Receiver<bool>,
}

const KILL_GRACE: Duration = Duration::from_secs(3);

impl Spawned {
    /// Wait for the program to exit and the sandbox to be torn down.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await;
        let _ = self.gone.wait_for(|g| *g).await;
        status
    }

    /// SIGTERM to the sandboxed process group, then SIGKILL to the whole sandbox after 3 s.
    pub async fn kill(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let Some(pid) = self.child.id().map(|p| p as i32) else {
            return self.child.wait().await;
        };
        // bwrap dies on SIGTERM without passing it on, so signal the sandboxed program's own
        // group (bwrap's --new-session made it a session leader). If it hasn't started yet,
        // SIGTERM to bwrap's group tears the sandbox down.
        let group = sandboxed_group(pid).unwrap_or(pid);
        // SAFETY: plain kill(2) on a process group we started.
        unsafe { libc::kill(-group, libc::SIGTERM) };
        if let Ok(status) = tokio::time::timeout(KILL_GRACE, self.wait()).await {
            return status;
        }
        // SAFETY: as above; the PID namespace dies with bwrap.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let _ = self.child.start_kill();
        self.wait().await
    }
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

/// A bwrap argument list under construction.
#[derive(Default)]
struct Args(Vec<OsString>);

impl Args {
    fn push(&mut self, xs: &[&std::ffi::OsStr]) {
        self.0.extend(xs.iter().map(|x| x.to_os_string()));
    }

    fn extend(&mut self, xs: &[OsString]) {
        self.0.extend_from_slice(xs);
    }
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

    /// Where the hidden paths (minus `unhide`) really are: what `args_hidden` mounts over.
    fn hidden_targets(&self, unhide: &[PathBuf]) -> Vec<PathBuf> {
        let unhide: Vec<PathBuf> = unhide
            .iter()
            .flat_map(|u| {
                [
                    u.clone(),
                    std::fs::canonicalize(u).unwrap_or_else(|_| u.clone()),
                ]
            })
            .collect();
        self.hidden_paths()
            .into_iter()
            .filter(|h| !unhide.contains(h))
            .filter_map(|h| std::fs::canonicalize(h).ok())
            .filter(|real| !unhide.contains(real))
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

    /// Namespaces, the read-only root, /dev, /proc and an empty /tmp.
    fn args_base(&self, a: &mut Args) {
        a.push(&[
            "--die-with-parent".as_ref(),
            "--new-session".as_ref(),
            "--unshare-pid".as_ref(),
            "--unshare-ipc".as_ref(),
            "--unshare-uts".as_ref(),
        ]);
        if !self.settings.network {
            a.push(&["--unshare-net".as_ref()]);
        }
        a.push(&[
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
    }

    fn args_caches(&self, a: &mut Args, caches: &[(PathBuf, PathBuf)]) {
        for (cache, layer) in caches {
            let (up, work) = (layer.join("upper"), layer.join("work"));
            a.push(&[
                "--overlay-src".as_ref(),
                cache.as_os_str(),
                "--overlay".as_ref(),
                up.as_os_str(),
                work.as_os_str(),
                cache.as_os_str(),
            ]);
        }
    }

    /// The chat's overlay on the workspace, with `.git` read-only on top.
    fn args_overlay(&self, a: &mut Args, workspace: &Path, upper: &Path, work: &Path) {
        let ws = workspace.as_os_str();
        a.push(&[
            "--overlay-src".as_ref(),
            ws,
            "--overlay".as_ref(),
            upper.as_os_str(),
            work.as_os_str(),
            ws,
        ]);
        let git = workspace.join(".git");
        if git.exists() {
            a.push(&["--ro-bind".as_ref(), git.as_os_str(), git.as_os_str()]);
        }
    }

    /// Hidden paths, minus `unhide`. Mount over the real location (bwrap can't mount through
    /// symlinks such as /var/run → /run): directories get an empty tmpfs, files and sockets
    /// /dev/null.
    fn args_hidden(&self, a: &mut Args, unhide: &[PathBuf]) {
        let unhide: Vec<PathBuf> = unhide
            .iter()
            .flat_map(|u| {
                [
                    u.clone(),
                    std::fs::canonicalize(u).unwrap_or_else(|_| u.clone()),
                ]
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        for h in self.hidden_paths() {
            let Ok(real) = std::fs::canonicalize(&h) else {
                continue;
            };
            if unhide.contains(&h) || unhide.contains(&real) || !seen.insert(real.clone()) {
                continue;
            }
            if real.is_dir() {
                a.push(&["--tmpfs".as_ref(), real.as_os_str()]);
            } else {
                a.push(&["--ro-bind".as_ref(), "/dev/null".as_ref(), real.as_os_str()]);
            }
        }
    }

    /// The scrubbed environment.
    fn args_env(&self, a: &mut Args, target: Option<&std::ffi::OsStr>) {
        a.push(&["--clearenv".as_ref()]);
        for k in ENV_PASS {
            if let Some(v) = std::env::var_os(k) {
                a.push(&["--setenv".as_ref(), k.as_ref(), v.as_os_str()]);
            }
        }
        a.push(&["--setenv".as_ref(), "HOME".as_ref(), self.home.as_os_str()]);
        a.push(&["--setenv".as_ref(), "TERM".as_ref(), "dumb".as_ref()]);
        if let Some(t) = target {
            a.push(&["--setenv".as_ref(), "CARGO_TARGET_DIR".as_ref(), t]);
        }
    }

    /// The Landlock helper (when present), allowed to write only beneath `writes`.
    fn args_helper(&self, a: &mut Args, writes: &[&std::ffi::OsStr]) {
        if let Some(helper) = &self.helper {
            a.push(&[helper.as_os_str()]);
            for w in writes {
                a.push(&["--write".as_ref(), w]);
            }
            a.push(&["--".as_ref()]);
        }
    }

    /// The full bwrap argument list for one command.
    pub fn bwrap_args(&self, plan: &RunPlan) -> Vec<OsString> {
        let mut a = Args::default();
        self.args_base(&mut a);
        let caches = self.caches();
        self.args_caches(&mut a, &caches);
        self.args_overlay(&mut a, &plan.workspace, &plan.upper, &plan.work);
        let target = plan.target_dir.as_os_str();
        a.push(&["--bind".as_ref(), target, target]);
        // Hidden paths last, so they also cover anything inside the workspace or caches.
        self.args_hidden(&mut a, &[]);
        self.args_env(&mut a, Some(target));
        let ws = plan.workspace.as_os_str();
        a.push(&["--chdir".as_ref(), ws, "--".as_ref()]);
        let mut writes: Vec<&std::ffi::OsStr> = vec![ws, "/tmp".as_ref(), "/dev".as_ref(), target];
        writes.extend(caches.iter().map(|(c, _)| c.as_os_str()));
        self.args_helper(&mut a, &writes);
        // No profile or rc files: the environment is exactly what's set above.
        a.push(&[
            "bash".as_ref(),
            "--noprofile".as_ref(),
            "--norc".as_ref(),
            "-c".as_ref(),
            plan.command.as_ref(),
        ]);
        a.0
    }

    /// The bwrap argument list for a long-lived process (see [`SpawnPlan`]). Mount order, from
    /// the bottom: the read-only root, the throwaway home layer, package caches, the chat's
    /// workspace overlay, `rw_binds`, the hidden paths (minus `unhide`), then void.ai's own
    /// writable directories (target dir, Scratch cwd), which live under its hidden data dir.
    ///
    /// Fails (instead of quietly dropping a mount) when the home layer or an `rw_binds` target
    /// can't be resolved, or when the workspace or Scratch cwd lies inside an `rw_binds` path.
    pub fn spawn_args(&self, plan: &SpawnPlan) -> Result<Vec<OsString>, String> {
        let home = self.check_spawn(plan)?;
        let binds = plan
            .rw_binds
            .iter()
            .map(|b| {
                std::fs::canonicalize(b.path())
                    .map_err(|e| format!("can't bind {}: {e}", b.path().display()))
            })
            .collect::<Result<Vec<PathBuf>, String>>()?;
        let mut a = Args::default();
        self.args_base(&mut a);
        // Throwaway writable home: writes land in an invisible tmpfs, gone with the process.
        let h = home.as_os_str();
        let mut opaque = binds.clone();
        opaque.extend(self.hidden_targets(&plan.unhide));
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo");
        a.extend(&home_layer(&home, mountinfo, &opaque)?);
        let caches = self.caches();
        self.args_caches(&mut a, &caches);
        if let Some(o) = &plan.overlay {
            self.args_overlay(&mut a, &o.workspace, &o.upper, &o.work);
        }
        for b in &binds {
            a.push(&["--bind".as_ref(), b.as_os_str(), b.as_os_str()]);
        }
        self.args_hidden(&mut a, &plan.unhide);
        let target = plan.overlay.as_ref().map(|o| o.target_dir.as_os_str());
        if let Some(t) = target {
            a.push(&["--bind".as_ref(), t, t]);
        }
        let cwd = plan.cwd.as_os_str();
        if plan.overlay.is_none() {
            a.push(&["--bind".as_ref(), cwd, cwd]);
        }
        self.args_env(&mut a, target);
        for (k, v) in &plan.env {
            a.push(&["--setenv".as_ref(), k.as_os_str(), v.as_os_str()]);
        }
        a.push(&["--chdir".as_ref(), cwd, "--".as_ref()]);
        let mut writes: Vec<&std::ffi::OsStr> = vec!["/tmp".as_ref(), "/dev".as_ref()];
        match &plan.overlay {
            Some(o) => writes.extend([o.workspace.as_os_str(), o.target_dir.as_os_str()]),
            None => writes.push(cwd),
        }
        writes.extend(caches.iter().map(|(c, _)| c.as_os_str()));
        writes.push(h);
        writes.extend(binds.iter().map(|b| b.as_os_str()));
        self.args_helper(&mut a, &writes);
        a.extend(&plan.argv);
        Ok(a.0)
    }

    /// The checks `spawn` makes before touching anything; returns the canonical home.
    ///
    /// A workspace (or Scratch cwd) at or beneath a read-write bind is refused: the bind of the
    /// host tree is mounted over the chat overlay and the read-only `.git`, so edits would reach
    /// the host directly. The reverse (binds inside the workspace, e.g. workspace == `$HOME`) is
    /// fine: the binds sit on top, so Claude Code's saves reach its real files, not Review.
    fn check_spawn(&self, plan: &SpawnPlan) -> Result<PathBuf, String> {
        let home = std::fs::canonicalize(&self.home).map_err(|e| {
            format!(
                "can't set up the throwaway home layer on {}: {e}",
                self.home.display()
            )
        })?;
        let dir = match &plan.overlay {
            Some(o) => &o.workspace,
            None => &plan.cwd,
        };
        let real = resolve(dir);
        for b in &plan.rw_binds {
            let bind = resolve(b.path());
            if real.starts_with(&bind) {
                return Err(format!(
                    "can't open {} here: it is inside Claude Code's state ({}), which is bound \
                     read-write, so edits would bypass the chat overlay and Review",
                    dir.display(),
                    bind.display()
                ));
            }
        }
        Ok(home)
    }

    /// Start a long-lived process in the sandbox (no timeout; stdio piped; own process
    /// group). Holds the cache lock for as long as the returned [`Spawned`] lives, so other
    /// commands wait; when the lock is taken, `on_wait` is called once before waiting for it
    /// (the caller's "waiting for sandbox" signal).
    ///
    /// **Call this from a tokio runtime worker thread (a task on the runtime), never from
    /// `spawn_blocking` or a short-lived thread.** bwrap runs with `--die-with-parent`, which
    /// is `PR_SET_PDEATHSIG`: it fires when the *thread* that forked bwrap exits, not the
    /// process. Blocking-pool threads are retired after idling, which would SIGKILL a turn that
    /// is still running.
    pub async fn spawn(
        &self,
        plan: SpawnPlan,
        on_wait: impl FnOnce() + Send,
    ) -> Result<Spawned, String> {
        let Some(bwrap) = &self.bwrap else {
            return Err("bubblewrap (bwrap) isn't installed".into());
        };
        if plan.argv.is_empty() {
            return Err("nothing to run".into());
        }
        self.check_spawn(&plan)?;
        let caches = self.caches();
        let lock = if caches.is_empty() {
            None
        } else {
            Some(match self.lock.clone().try_lock_owned() {
                Ok(g) => g,
                Err(_) => {
                    on_wait();
                    self.lock.clone().lock_owned().await
                }
            })
        };
        let mut dirs: Vec<&Path> = Vec::new();
        match &plan.overlay {
            Some(o) => dirs.extend([o.target_dir.as_path(), &o.upper, &o.work]),
            None => dirs.push(&plan.cwd),
        }
        for dir in dirs {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("can't prepare {}: {e}", dir.display()))?;
        }
        for (_, layer) in &caches {
            let _ = std::fs::create_dir_all(layer.join("upper"));
            let _ = std::fs::create_dir_all(layer.join("work"));
        }
        prepare_rw_binds(&plan.rw_binds)?;
        let (info_r, info_w) =
            cloexec_pipe().map_err(|e| format!("can't start bubblewrap: {e}"))?;
        let w = info_w.as_raw_fd();
        let mut cmd = tokio::process::Command::new(bwrap);
        cmd.arg("--info-fd")
            .arg(w.to_string())
            .args(self.spawn_args(&plan)?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        // SAFETY: only fcntl(2) between fork and exec, so bwrap inherits the info fd.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(w, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("can't start bubblewrap: {e}"))?;
        drop(info_w);
        let (tx, gone) = tokio::sync::watch::channel(false);
        watch_teardown(info_r, lock, tx);
        Ok(Spawned { child, gone })
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

/// Create missing bind targets (directories, or files with their initial contents). Present
/// targets are left alone; `create_new` makes sure an existing file is never overwritten.
pub fn prepare_rw_binds(binds: &[RwBind]) -> Result<(), String> {
    for b in binds {
        let res = match b {
            RwBind::Dir(p) => std::fs::create_dir_all(p),
            RwBind::File { path, if_missing } => {
                if path.symlink_metadata().is_ok() {
                    continue;
                }
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .and_then(|mut f| f.write_all(if_missing.as_bytes()))
                    .or_else(|e| match e.kind() {
                        std::io::ErrorKind::AlreadyExists => Ok(()),
                        _ => Err(e),
                    })
            }
        };
        res.map_err(|e| format!("can't prepare {}: {e}", b.path().display()))?;
    }
    Ok(())
}

fn cloexec_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2(2) fills both fds on success; each is owned exactly once below.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh fds from pipe2.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// `"child-pid": N` from bwrap's `--info-fd` JSON.
fn info_child_pid(info: &str) -> Option<i32> {
    let rest = &info[info.find("\"child-pid\"")? + "\"child-pid\"".len()..];
    let digits: String = rest
        .trim_start_matches([':', ' ', '\n', '\t'])
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// See [`Spawned`]: release `lock` and report `gone` once the sandbox's PID-namespace init has
/// exited (by then the kernel has dropped its mounts). If its pid can't be learned or opened,
/// it is already gone.
fn watch_teardown(
    info: OwnedFd,
    lock: Option<tokio::sync::OwnedMutexGuard<()>>,
    gone: tokio::sync::watch::Sender<bool>,
) {
    std::thread::spawn(move || {
        use std::io::Read;
        let mut file = std::fs::File::from(info);
        let (mut text, mut buf) = (Vec::new(), [0u8; 512]);
        while !text.contains(&b'}') {
            match file.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => text.extend_from_slice(&buf[..n]),
            }
        }
        drop(file);
        if let Some(pid) = info_child_pid(&String::from_utf8_lossy(&text)) {
            // SAFETY: pidfd_open(2); a negative result means the process is already gone.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            if fd >= 0 {
                // SAFETY: fresh fd from pidfd_open.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                let mut p = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // A pidfd turns readable when the process has exited.
                // SAFETY: poll(2) on one valid pollfd.
                while unsafe { libc::poll(&mut p, 1, -1) } == -1
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                }
            }
        }
        drop(lock);
        let _ = gone.send(true);
    });
}

/// `p` with symlinks resolved as far as it exists (the rest appended as written), so paths
/// that `spawn` hasn't created yet can still be compared.
/// What the home layer needs to know about one directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HomeEntry {
    Dir,
    File,
    Symlink(PathBuf),
    /// Sockets, fifos, devices, and anything that couldn't be read.
    Other,
}

/// Mount points listed in `/proc/self/mountinfo` (field 5, with `\ooo` escapes undone).
fn mount_points(mountinfo: &str) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|m| {
            let b = m.as_bytes();
            let mut out = Vec::with_capacity(b.len());
            let mut i = 0;
            while i < b.len() {
                let oct = b
                    .get(i + 1..i + 4)
                    .filter(|o| b[i] == b'\\' && o.iter().all(|c| (b'0'..=b'7').contains(c)));
                match oct {
                    Some(o) => {
                        out.push(o.iter().fold(0u8, |n, c| n.wrapping_mul(8) + (c - b'0')));
                        i += 4;
                    }
                    None => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            PathBuf::from(OsString::from_vec(out))
        })
        .collect()
}

/// A directory's entries, sorted, typed from the listing itself (`DirEntry::file_type`: the
/// entry's type as the parent directory records it, or an `lstat`, which never follows a
/// symlink or triggers an automount), so a mount point is never opened by path.
fn list_dir(dir: &Path) -> Vec<(OsString, HomeEntry)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(OsString, HomeEntry)> = rd
        .flatten()
        .map(|e| {
            let kind = match e.file_type() {
                Ok(t) if t.is_symlink() => std::fs::read_link(e.path())
                    .map(HomeEntry::Symlink)
                    .unwrap_or(HomeEntry::Other),
                Ok(t) if t.is_dir() => HomeEntry::Dir,
                Ok(t) if t.is_file() => HomeEntry::File,
                _ => HomeEntry::Other,
            };
            (e.file_name(), kind)
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The throwaway home layer's bwrap arguments, built per entry because unprivileged overlayfs
/// refuses a lower directory with any mount beneath it (EINVAL, "failed to clone lowerpath"):
/// a tmpfs on `home`; each directory without mounts at or below it gets its own throwaway
/// overlay; directories holding mounts are recreated and walked the same way; mount points
/// that are directories stay empty (never listed or opened), other mount points are left out;
/// files are read-only binds (`-try`: a temp file may be gone by the time bwrap runs);
/// symlinks are recreated; anything else is left out. `opaque` paths (what the read-write
/// binds and hidden paths mount over) are never listed or overlaid: a directory among them
/// becomes an empty directory, anything else is left out, and the mount on top supplies it.
/// `list` gives a directory's entries.
/// The home layer for `home`, from `/proc/self/mountinfo`'s contents. `opaque` paths (the
/// read-write binds and hidden paths that go on top) are never listed.
fn home_layer(
    home: &Path,
    mountinfo: std::io::Result<String>,
    opaque: &[PathBuf],
) -> Result<Vec<OsString>, String> {
    let mounts = mount_points(&mountinfo.map_err(|e| {
        format!("can't read /proc/self/mountinfo to set up the throwaway home layer: {e}")
    })?);
    Ok(home_layer_args(home, &mounts, opaque, &mut list_dir))
}

fn home_layer_args(
    home: &Path,
    mounts: &[PathBuf],
    opaque: &[PathBuf],
    list: &mut dyn FnMut(&Path) -> Vec<(OsString, HomeEntry)>,
) -> Vec<OsString> {
    fn walk(
        dir: &Path,
        mounts: &[PathBuf],
        opaque: &[PathBuf],
        list: &mut dyn FnMut(&Path) -> Vec<(OsString, HomeEntry)>,
        out: &mut Vec<OsString>,
    ) {
        for (name, kind) in list(dir) {
            let p = dir.join(name);
            let o = p.clone().into_os_string();
            if mounts.contains(&p) || opaque.contains(&p) {
                if kind == HomeEntry::Dir {
                    out.extend(["--dir".into(), o]);
                }
                continue;
            }
            match kind {
                HomeEntry::Dir if mounts.iter().any(|m| m.starts_with(&p)) => {
                    out.extend(["--dir".into(), o]);
                    walk(&p, mounts, opaque, list, out);
                }
                HomeEntry::Dir => {
                    out.extend(["--overlay-src".into(), o.clone(), "--tmp-overlay".into(), o])
                }
                HomeEntry::File => out.extend(["--ro-bind-try".into(), o.clone(), o]),
                HomeEntry::Symlink(t) => out.extend(["--symlink".into(), t.into_os_string(), o]),
                HomeEntry::Other => {}
            }
        }
    }
    let mut out = vec!["--tmpfs".into(), home.as_os_str().to_os_string()];
    walk(home, mounts, opaque, list, &mut out);
    out
}

fn resolve(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(real) = std::fs::canonicalize(cur) {
            return rest.iter().rev().fold(real, |acc, c| acc.join(c));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// `(comm, ppid, pgrp)` from /proc/<pid>/stat.
fn proc_stat(pid: i32) -> Option<(String, i32, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let comm = stat.get(open + 1..close)?.to_string();
    let mut rest = stat.get(close + 1..)?.split_whitespace().skip(1);
    let ppid = rest.next()?.parse().ok()?;
    let pgrp = rest.next()?.parse().ok()?;
    Some((comm, ppid, pgrp))
}

/// The process group of the program bwrap runs: the first descendant of `bwrap` that isn't
/// bwrap itself (its PID-namespace init is).
fn sandboxed_group(bwrap: i32) -> Option<i32> {
    let procs: Vec<(i32, String, i32, i32)> = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| proc_stat(pid).map(|(c, pp, pg)| (pid, c, pp, pg)))
        .collect();
    let mut frontier = vec![bwrap];
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for (pid, comm, ppid, pgrp) in &procs {
            if frontier.contains(ppid) {
                if comm != "bwrap" {
                    return Some(*pgrp);
                }
                next.push(*pid);
            }
        }
        frontier = next;
    }
    None
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

    /// A mount under HOME (here `~/shares/nas`) must not be under any overlay's lower dir:
    /// unprivileged overlayfs refuses a lower dir with mounts beneath it (EINVAL).
    #[test]
    fn home_layer_skips_mounts_and_recurses() {
        let home = Path::new("/home/u");
        let mounts: Vec<PathBuf> = ["/", "/home", "/home/u/shares/nas", "/home/u/mnt"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let mut seen = Vec::new();
        let mut list = |d: &Path| {
            seen.push(d.to_path_buf());
            let e = |n: &str, k: HomeEntry| (OsString::from(n), k);
            match d.to_str().unwrap() {
                "/home/u" => vec![
                    e(".bashrc", HomeEntry::File),
                    e(".config", HomeEntry::Dir),
                    e("link", HomeEntry::Symlink("/opt/x".into())),
                    e("mnt", HomeEntry::Dir),
                    e("shares", HomeEntry::Dir),
                    e("sock", HomeEntry::Other),
                ],
                "/home/u/shares" => vec![
                    e("nas", HomeEntry::Dir),
                    e("notes.txt", HomeEntry::File),
                    e("old", HomeEntry::Dir),
                ],
                other => panic!("listed {other}"),
            }
        };
        let a = strs(&home_layer_args(home, &mounts, &[], &mut list));
        let want: Vec<&str> = vec![
            "--tmpfs",
            "/home/u",
            "--ro-bind-try",
            "/home/u/.bashrc",
            "/home/u/.bashrc",
            "--overlay-src",
            "/home/u/.config",
            "--tmp-overlay",
            "/home/u/.config",
            "--symlink",
            "/opt/x",
            "/home/u/link",
            "--dir",
            "/home/u/mnt",
            "--dir",
            "/home/u/shares",
            "--dir",
            "/home/u/shares/nas",
            "--ro-bind-try",
            "/home/u/shares/notes.txt",
            "/home/u/shares/notes.txt",
            "--overlay-src",
            "/home/u/shares/old",
            "--tmp-overlay",
            "/home/u/shares/old",
        ];
        assert_eq!(a, want);
        assert_eq!(
            seen,
            [home.to_path_buf(), home.join("shares")],
            "mounts never listed"
        );
    }

    /// A file can be a mount point too (a single-file bind): it's left out, not made a
    /// directory, so a later file mount on it (a hidden path, `~/.claude.json`) still works.
    #[test]
    fn home_layer_leaves_out_file_mount_points() {
        let home = Path::new("/home/u");
        let mounts = vec![
            PathBuf::from("/home/u/.netrc"),
            PathBuf::from("/home/u/sh/f"),
        ];
        let mut list = |d: &Path| {
            let e = |n: &str, k: HomeEntry| (OsString::from(n), k);
            match d.to_str().unwrap() {
                "/home/u" => vec![e(".netrc", HomeEntry::File), e("sh", HomeEntry::Dir)],
                "/home/u/sh" => vec![e("f", HomeEntry::File)],
                other => panic!("listed {other}"),
            }
        };
        let a = strs(&home_layer_args(home, &mounts, &[], &mut list));
        assert_eq!(a, ["--tmpfs", "/home/u", "--dir", "/home/u/sh"]);
    }

    /// The read-write binds and hidden paths go on top of the layer: void never lists what's
    /// in them, even with a mount below, and emits only a directory to mount on.
    #[test]
    fn home_layer_never_lists_opaque_paths() {
        let home = Path::new("/home/u");
        let mounts = vec![
            PathBuf::from("/home/u/.claude/m"),
            PathBuf::from("/home/u/.ssh/agent"),
        ];
        let opaque = [
            PathBuf::from("/home/u/.claude"),
            PathBuf::from("/home/u/.claude.json"),
            PathBuf::from("/home/u/.ssh"),
            PathBuf::from("/home/u/.netrc"),
        ];
        let mut list = |d: &Path| {
            let e = |n: &str, k: HomeEntry| (OsString::from(n), k);
            match d.to_str().unwrap() {
                "/home/u" => vec![
                    e(".claude", HomeEntry::Dir),
                    e(".claude.json", HomeEntry::File),
                    e(".netrc", HomeEntry::File),
                    e(".ssh", HomeEntry::Dir),
                ],
                other => panic!("listed {other}"),
            }
        };
        let a = strs(&home_layer_args(home, &mounts, &opaque, &mut list));
        assert_eq!(
            a,
            [
                "--tmpfs",
                "/home/u",
                "--dir",
                "/home/u/.claude",
                "--dir",
                "/home/u/.ssh"
            ]
        );
    }

    /// Without the mount list, every mount under HOME would get an overlay opened by path
    /// (an automount trigger, then EINVAL): refuse the spawn instead.
    #[test]
    fn home_layer_needs_mountinfo() {
        let err = home_layer(
            Path::new("/home/u"),
            Err(std::io::Error::other("denied")),
            &[],
        )
        .unwrap_err();
        assert!(
            err.contains("/proc/self/mountinfo") && err.contains("denied"),
            "{err}"
        );
    }

    #[test]
    fn mountinfo_mount_points_unescaped() {
        let mi = "36 35 98:0 / / rw - ext4 /dev/root rw\n\
                  40 36 0:50 / /home/u/my\\040share rw - nfs4 nas:/x rw\n\
                  41 36 0:51 / /home/u/back\\134slash rw - tmpfs t rw\n";
        assert_eq!(
            mount_points(mi),
            [
                PathBuf::from("/"),
                PathBuf::from("/home/u/my share"),
                PathBuf::from("/home/u/back\\slash")
            ]
        );
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
        assert_eq!(
            &a[a.len() - 5..],
            ["bash", "--noprofile", "--norc", "-c", "echo hi"]
        );

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
        assert_eq!(
            &b[b.len() - 6..],
            ["--", "bash", "--noprofile", "--norc", "-c", "echo hi"]
        );
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

    #[test]
    fn defaults_hide_credentials_found_on_desktops() {
        let s = SandboxSettings::default();
        for h in [
            "~/.npmrc",
            "~/.local/share/fish/fish_history",
            "~/.bash_history",
            "~/.cargo/credentials.toml",
            "~/.git-credentials",
            "~/.docker",
            "~/.kube",
            "~/.password-store",
            "~/.claude.json",
            "~/.config/strata",
            "~/.local/share/opencode",
        ] {
            assert!(s.hidden.iter().any(|x| x == h), "{h}");
        }
    }

    #[test]
    fn hidden_mounts_come_after_the_overlays_and_shell_skips_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(ws.join("secrets")).unwrap();
        let p = |x: &Path| x.to_string_lossy().into_owned();
        let settings = SandboxSettings {
            hidden: vec![p(&ws.join("secrets"))],
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
            workspace: ws.clone(),
            upper: tmp.path().join("u"),
            work: tmp.path().join("w"),
            target_dir: tmp.path().join("t"),
            command: "true".into(),
            timeout_secs: None,
        };
        let a = strs(&sb.bwrap_args(&plan));
        let ovl = pos(&a, &["--overlay", &p(&plan.upper), &p(&plan.work), &p(&ws)]).unwrap();
        let hid = pos(
            &a,
            &["--tmpfs", &p(&ws.join("secrets").canonicalize().unwrap())],
        )
        .unwrap();
        assert!(hid > ovl, "hidden mounts must cover the workspace overlay");
        assert_eq!(
            &a[a.len() - 5..],
            ["bash", "--noprofile", "--norc", "-c", "true"]
        );
    }

    /// A home with Claude Code state, an SSH dir and a Claude desktop dir, all hidden by
    /// default; the plan unhides and binds only `~/.claude` and `~/.claude.json`.
    fn claude_rig(tmp: &Path) -> (Sandbox, PathBuf, SpawnPlan) {
        let home = tmp.join("home");
        let ws = home.join("proj");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude.json"), "{}").unwrap();
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(home.join(".config/Claude")).unwrap();
        fs::create_dir_all(home.join(".cargo/registry")).unwrap();
        fs::create_dir_all(&ws).unwrap();
        let settings = SandboxSettings {
            hidden: vec![
                "~/.ssh".into(),
                "~/.claude".into(),
                "~/.claude.json".into(),
                "~/.config/Claude".into(),
            ],
            caches: vec!["~/.cargo/registry".into()],
            ..SandboxSettings::default()
        };
        let sb = Sandbox::with_paths(
            Some("/usr/bin/bwrap".into()),
            None,
            tmp.join("data"),
            home.clone(),
            settings,
        );
        let plan = SpawnPlan {
            overlay: Some(OverlayMounts {
                workspace: ws.clone(),
                upper: tmp.join("ovl/upper"),
                work: tmp.join("ovl/work"),
                target_dir: tmp.join("target"),
            }),
            argv: vec!["claude".into(), "-p".into()],
            env: vec![("VOID_TEST_ENV".into(), "1".into())],
            rw_binds: vec![
                RwBind::Dir(home.join(".claude")),
                RwBind::File {
                    path: home.join(".claude.json"),
                    if_missing: "{}".into(),
                },
            ],
            unhide: vec![home.join(".claude"), home.join(".claude.json")],
            cwd: ws,
        };
        (sb, home, plan)
    }

    #[test]
    fn spawn_args_unhide_and_bind_claude_state() {
        let tmp = tempfile::tempdir().unwrap();
        let (sb, home, plan) = claude_rig(tmp.path());
        let a = strs(&sb.spawn_args(&plan).unwrap());
        let p = |x: &Path| x.canonicalize().unwrap().to_string_lossy().into_owned();
        let (h, claude, cj) = (
            p(&home),
            p(&home.join(".claude")),
            p(&home.join(".claude.json")),
        );
        let layer = pos(&a, &["--tmpfs", &h]).expect("home layer");
        let proj = p(&home.join("proj"));
        // Claude Code's state and the hidden paths are never listed or overlaid: the binds and
        // hiding mounts on top supply them.
        let state = pos(&a, &["--dir", &claude]).expect("a dir to bind ~/.claude on");
        assert!(pos(&a, &["--overlay-src", &claude]).is_none());
        assert!(pos(&a, &["--ro-bind-try", &cj, &cj]).is_none());
        assert!(pos(&a, &["--ro-bind", &cj, &cj]).is_none());
        let ssh_home = p(&home.join(".ssh"));
        assert!(pos(&a, &["--overlay-src", &ssh_home]).is_none());
        let ssh_dir = pos(&a, &["--dir", &ssh_home]).unwrap();
        let cache = pos(
            &a,
            &[
                "--overlay-src",
                &p(&home.join(".cargo/registry")),
                "--overlay",
            ],
        );
        let cache = cache.unwrap();
        let ovl = pos(&a, &["--overlay-src", &proj, "--overlay"]).unwrap();
        let proj_layer = pos(&a, &["--overlay-src", &proj, "--tmp-overlay", &proj]).unwrap();
        assert!(
            layer < state && state < ssh_dir && ssh_dir < proj_layer && proj_layer < cache,
            "the home layer, per entry, before everything else"
        );
        let b1 = pos(&a, &["--bind", &claude, &claude]).expect("~/.claude rw");
        let b2 = pos(&a, &["--bind", &cj, &cj]).expect("~/.claude.json rw");
        assert!(
            layer < cache && layer < ovl,
            "home layer goes under everything else"
        );
        assert!(
            layer < b1 && layer < b2,
            "claude state binds sit on the home layer"
        );
        assert!(pos(&a, &["--tmpfs", &claude]).is_none());
        assert!(pos(&a, &["--ro-bind", "/dev/null", &cj]).is_none());
        let ssh = pos(&a, &["--tmpfs", &p(&home.join(".ssh"))]).expect("~/.ssh hidden");
        let desk = pos(&a, &["--tmpfs", &p(&home.join(".config/Claude"))]).unwrap();
        assert!(ssh > b2 && desk > b2 && ssh > ovl, "hidden mounts on top");
        let t = tmp.path().join("target");
        let tb = pos(&a, &["--bind", &t.to_string_lossy(), &t.to_string_lossy()]).unwrap();
        assert!(
            tb > ssh,
            "void's own target dir is bound after the hidden mounts"
        );
        assert!(a.iter().any(|x| x == "--clearenv"));
        assert!(pos(&a, &["--setenv", "VOID_TEST_ENV", "1"]).is_some());
        assert!(pos(&a, &["--setenv", "HOME", &home.to_string_lossy()]).is_some());
        assert!(pos(&a, &["--chdir", &home.join("proj").to_string_lossy()]).is_some());
        assert_eq!(&a[a.len() - 3..], ["--", "claude", "-p"]);
        assert!(!a.iter().any(|x| x == "bash"));
    }

    #[test]
    fn spawn_keeps_void_agent_hiding() {
        let tmp = tempfile::tempdir().unwrap();
        let (sb, home, plan) = claude_rig(tmp.path());
        sb.spawn_args(&plan).unwrap();
        let ov = plan.overlay.unwrap();
        let run = RunPlan {
            workspace: ov.workspace,
            upper: ov.upper,
            work: ov.work,
            target_dir: ov.target_dir,
            command: "true".into(),
            timeout_secs: None,
        };
        let a = strs(&sb.bwrap_args(&run));
        let p = |x: &Path| x.canonicalize().unwrap().to_string_lossy().into_owned();
        assert!(pos(&a, &["--tmpfs", &p(&home.join(".claude"))]).is_some());
        assert!(
            pos(
                &a,
                &["--ro-bind", "/dev/null", &p(&home.join(".claude.json"))]
            )
            .is_some()
        );
        assert!(!a.iter().any(|x| x == "--tmp-overlay"));
        assert!(!a.iter().any(|x| x == "VOID_TEST_ENV"));
    }

    #[test]
    fn spawn_scratch_without_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let (sb, _home, mut plan) = claude_rig(tmp.path());
        let scratch = tmp.path().join("data/scratch/chat1");
        plan.overlay = None;
        plan.cwd = scratch.clone();
        let a = strs(&sb.spawn_args(&plan).unwrap());
        let s = scratch.to_string_lossy().into_owned();
        let ws = tmp.path().join("home/proj").canonicalize().unwrap();
        assert!(
            pos(&a, &["--overlay-src", &ws.to_string_lossy(), "--overlay"]).is_none(),
            "no workspace overlay"
        );
        assert!(
            pos(
                &a,
                &["--overlay-src", &ws.to_string_lossy(), "--tmp-overlay"]
            )
            .is_some(),
            "just its throwaway home layer"
        );
        assert!(!a.iter().any(|x| x == "CARGO_TARGET_DIR"));
        let ssh_dir = tmp.path().join("home/.ssh").canonicalize().unwrap();
        let ssh = pos(&a, &["--tmpfs", &ssh_dir.to_string_lossy()]).unwrap();
        let cwd = pos(&a, &["--bind", &s, &s]).expect("cwd bound writable");
        assert!(cwd > ssh);
        assert!(pos(&a, &["--chdir", &s]).is_some());
        // With the Landlock helper, the cwd and the claude state are writable too.
        let ll = Sandbox::with_paths(
            Some("/usr/bin/bwrap".into()),
            Some("/x/void-sandbox-exec".into()),
            tmp.path().join("data"),
            tmp.path().join("home"),
            sb.settings().clone(),
        );
        let b = strs(&ll.spawn_args(&plan).unwrap());
        let h = pos(&b, &["/x/void-sandbox-exec"]).unwrap();
        assert!(pos(&b[h..], &["--write", &s]).is_some());
        assert!(pos(&b[h..], &["--write", "/tmp"]).is_some());
        let home = tmp.path().join("home").canonicalize().unwrap();
        assert!(pos(&b[h..], &["--write", &home.to_string_lossy()]).is_some());
        assert_eq!(&b[b.len() - 3..], ["--", "claude", "-p"]);
    }

    /// The rig's sandbox with a bwrap that doesn't exist: `spawn` can only fail, and a refusal
    /// has to come from the checks, never from trying to start anything.
    fn no_bwrap(sb: &Sandbox, home: &Path, data: &Path) -> Sandbox {
        Sandbox::with_paths(
            Some("/nonexistent/bwrap".into()),
            None,
            data.to_path_buf(),
            home.to_path_buf(),
            sb.settings().clone(),
        )
    }

    #[tokio::test]
    async fn spawn_workspace_inside_claude_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (sb, home, mut plan) = claude_rig(tmp.path());
        let sb = no_bwrap(&sb, &home, &tmp.path().join("data"));
        plan.argv = vec!["true".into()];
        fs::create_dir_all(home.join(".claude/ws/.git")).unwrap();
        for ws in [home.join(".claude/ws"), home.join(".claude")] {
            let mut p = plan.clone();
            let o = p.overlay.as_mut().unwrap();
            o.workspace = ws.clone();
            p.cwd = ws.clone();
            let err = sb.spawn(p, || {}).await.err().unwrap();
            assert!(
                err.contains("inside Claude Code's state"),
                "{}: {err}",
                ws.display()
            );
        }
        let mut p = plan.clone();
        p.overlay = None;
        p.cwd = home.join(".claude/scratch");
        let err = sb.spawn(p, || {}).await.err().unwrap();
        assert!(err.contains("inside Claude Code's state"), "scratch: {err}");
        // The reverse is fine: the state binds sit on top of a workspace that contains them.
        let mut p = plan.clone();
        p.overlay.as_mut().unwrap().workspace = home.clone();
        p.cwd = home.clone();
        let err = sb.spawn(p, || {}).await.err().unwrap();
        assert!(
            err.contains("can't start bubblewrap"),
            "home as workspace: {err}"
        );
    }

    #[tokio::test]
    async fn spawn_refuses_unresolvable_rw_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let (sb, home, mut plan) = claude_rig(tmp.path());
        let sb = no_bwrap(&sb, &home, &tmp.path().join("data"));
        plan.argv = vec!["true".into()];
        fs::remove_file(home.join(".claude.json")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("gone"), home.join(".claude.json")).unwrap();
        let err = sb.spawn(plan, || {}).await.err().unwrap();
        assert!(err.contains(".claude.json"), "{err}");
        assert!(!err.contains("can't start bubblewrap"), "{err}");
    }

    #[tokio::test]
    async fn spawn_refuses_without_a_home_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let (sb, _home, mut plan) = claude_rig(tmp.path());
        let missing = tmp.path().join("no-such-home");
        let sb = no_bwrap(&sb, &missing, &tmp.path().join("data"));
        plan.argv = vec!["true".into()];
        plan.rw_binds.clear();
        plan.unhide.clear();
        let err = sb.spawn(plan, || {}).await.err().unwrap();
        assert!(err.contains("home"), "{err}");
        assert!(!err.contains("can't start bubblewrap"), "{err}");
    }

    #[test]
    fn spawn_prepares_missing_claude_state_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (_sb, home, plan) = claude_rig(tmp.path());
        fs::remove_dir_all(home.join(".claude")).unwrap();
        fs::remove_file(home.join(".claude.json")).unwrap();
        prepare_rw_binds(&plan.rw_binds).unwrap();
        assert!(home.join(".claude").is_dir());
        assert_eq!(fs::read_to_string(home.join(".claude.json")).unwrap(), "{}");
        // Present targets are left exactly as they are (dummy content in a temp home).
        fs::write(home.join(".claude.json"), "dummy").unwrap();
        prepare_rw_binds(&plan.rw_binds).unwrap();
        assert_eq!(
            fs::read_to_string(home.join(".claude.json")).unwrap(),
            "dummy"
        );
    }
}
