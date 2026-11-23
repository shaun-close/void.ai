//! Running a chain of steps (UI-free): each step in its own process group, output streamed,
//! Stop = SIGTERM then SIGKILL. Plus the compiler-location parser and the moon-config path test.
//!
//! Leftover processes: anything a step leaves running in its process group is killed when
//! the step ends; `setsid` daemons are unaffected (they left the group). Otherwise a
//! background job would keep the output pipe open and the chain couldn't move on.

use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use regex::Regex;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio_util::sync::CancellationToken;

use super::config::{OnFailure, Step, StepConfig};
use super::env::Envs;
use super::moon::MoonTask;

/// One run (a config or an ad-hoc task), unique for the engine's lifetime.
/// Serializable so a GUI layout can name a run's output tab (ids restart with the engine).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct RunId(pub u64);

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run {}", self.0)
    }
}

/// A file location found in a line of output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// Absolute (the base dir it was found in, joined with what was printed).
    pub path: PathBuf,
    /// 1-based, as printed.
    pub line: usize,
    /// 1-based, as printed.
    pub col: Option<usize>,
    /// Byte offsets in the **original** line (ANSI escapes included) of the location text.
    pub span: Range<usize>,
}

/// The file locations in one line of output: rustc's `--> src/x.rs:3:5`, tsc's
/// `src/x.ts(3,5)`, and generic `path:line:col` / `path:line`.
///
/// `bases` are the folders relative paths are tried against, in order (a run step's
/// `base_dirs`: its moon project's folder, then the workspace root); the first that holds the
/// file wins. Only existing files inside one of `bases` count (`..` is fine as long as it
/// stays inside), whether printed relative or absolute.
///
/// ANSI escapes are skipped when matching; `span` is in byte offsets of the **original**
/// `line` (so it can include escapes that sat inside the location).
pub fn parse_locations<P: AsRef<Path>>(line: &str, bases: &[P]) -> Vec<Location> {
    let bases: Vec<&Path> = bases.iter().map(AsRef::as_ref).collect();
    if bases.is_empty() {
        return Vec::new();
    }
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?P<path>[^\s:()\[\]{}<>"'`,;|=]+)(?:\((?P<tl>\d+),(?P<tc>\d+)\)|:(?P<l>\d+)(?::(?P<c>\d+))?)"#,
        )
        .expect("location regex")
    });
    let Some((plain, map)) = strip_ansi(line) else {
        return Vec::new();
    };
    RE.captures_iter(&plain)
        .filter_map(|m| {
            let whole = m.get(0)?;
            let num = |name: &str| m.name(name).and_then(|n| n.as_str().parse::<usize>().ok());
            let (line_no, col) = match num("tl") {
                Some(l) => (l, num("tc")),
                None => (num("l")?, num("c")),
            };
            if line_no == 0 {
                return None;
            }
            let path = find_file(Path::new(&m["path"]), &bases)?;
            let span = map[whole.start()]..map[whole.end() - 1] + 1;
            Some(Location {
                path,
                line: line_no,
                col,
                span,
            })
        })
        .collect()
}

/// The existing file `printed` names: absolute, or relative to the first base that has it;
/// always inside one of `bases`.
fn find_file(printed: &Path, bases: &[&Path]) -> Option<PathBuf> {
    let bases: Vec<PathBuf> = bases.iter().filter_map(|b| normalize(b)).collect();
    let inside = |p: &PathBuf| bases.iter().any(|b| p.starts_with(b)) && p.is_file();
    if printed.is_absolute() {
        return normalize(printed).filter(inside);
    }
    bases
        .iter()
        .find_map(|b| normalize(&b.join(printed)).filter(inside))
}

/// `p` with `.` and `..` resolved lexically (None if `..` climbs past the top).
fn normalize(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// `line` without ANSI escapes (CSI, OSC such as hyperlinks, two-byte escapes), and for each
/// byte of the result its offset in `line`. None if that wouldn't be valid UTF-8 (it always
/// is: only ASCII bytes and whole OSC strings are removed).
fn strip_ansi(line: &str) -> Option<(String, Vec<usize>)> {
    let b = line.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut map = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x1b {
            out.push(b[i]);
            map.push(i);
            i += 1;
            continue;
        }
        i = match b.get(i + 1) {
            // CSI: parameter and intermediate bytes, then one final byte.
            Some(b'[') => {
                let mut j = i + 2;
                while j < b.len() && (0x20..=0x3f).contains(&b[j]) {
                    j += 1;
                }
                if j < b.len() && (0x40..=0x7e).contains(&b[j]) {
                    j + 1
                } else {
                    j
                }
            }
            // OSC: up to BEL or ESC \.
            Some(b']') => {
                let mut j = i + 2;
                loop {
                    match b.get(j) {
                        None => break j,
                        Some(0x07) => break j + 1,
                        Some(0x1b) if b.get(j + 1) == Some(&b'\\') => break j + 2,
                        Some(_) => j += 1,
                    }
                }
            }
            Some(c) if c.is_ascii() => i + 2,
            _ => i + 1,
        };
    }
    Some((String::from_utf8(out).ok()?, map))
}

/// Whether a change to `rel` (relative to the workspace root) can change moon's tasks:
/// `moon.yml` / `moon.yaml` anywhere, or anything under `.moon/` except moon's own cache.
pub fn is_moon_config_path(rel: &Path) -> bool {
    let names: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n),
            _ => None,
        })
        .collect();
    if let Some(i) = names.iter().position(|n| *n == ".moon") {
        return names.get(i + 1).is_none_or(|n| *n != "cache");
    }
    names
        .last()
        .is_some_and(|n| *n == "moon.yml" || *n == "moon.yaml")
}

/// Where a step's relative paths are looked up, in order: a moon task's project folder (moon
/// runs it there, so tsc and the like print paths relative to it), then the workspace root.
/// Shell steps (and moon tasks that run from the workspace root, or whose project folder
/// isn't known) get the root alone.
pub fn step_base_dirs(step: &Step, root: &Path, tasks: &[MoonTask]) -> Vec<PathBuf> {
    if let Step::Moon { target, .. } = step
        && let Some(task) = tasks.iter().find(|t| t.target == *target)
        && !task.run_from_workspace_root
        && let Some(source) = task.source.as_deref()
        && let Some(dir) = normalize(&root.join(source))
        && dir != root
        && dir.starts_with(root)
    {
        return vec![dir, root.to_path_buf()];
    }
    vec![root.to_path_buf()]
}

/// What a running chain reports, in order: for each step `Step` then its `Output`, then one
/// `Finished`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunUpdate {
    Step {
        index: usize,
        label: String,
        /// For [`parse_locations`] on this step's output.
        base_dirs: Vec<PathBuf>,
    },
    Output(String),
    Finished {
        ok: bool,
        failed_step: Option<usize>,
        exit_code: Option<i32>,
        stopped: bool,
        error: Option<String>,
    },
}

pub type Sink = Arc<dyn Fn(RunUpdate) + Send + Sync>;

/// What to run. `env`: the profiles and the name of the one to apply (resolved when the run
/// starts, on a blocking thread).
pub struct ChainSpec {
    pub steps: Vec<StepConfig>,
    pub root: PathBuf,
    pub env: Option<(Envs, String)>,
    /// moon's tasks (for the project folders of moon steps).
    pub tasks: Vec<MoonTask>,
}

/// Output is sent at most this often (or when this much has piled up).
const FLUSH: Duration = Duration::from_millis(40);
const FLUSH_BYTES: usize = 64 * 1024;
/// After SIGTERM, how long a step gets before its group is SIGKILLed.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// After a step ends, how long its output may take to drain (something outside its process
/// group can hold the pipe open).
const DRAIN: Duration = Duration::from_secs(1);

/// A step's command line, for display.
pub fn step_label(step: &Step) -> String {
    match step {
        Step::Moon { target, args } if args.is_empty() => format!("moon run {target}"),
        Step::Moon { target, args } => format!("moon run {target} -- {}", args.join(" ")),
        Step::Shell { command } => command.clone(),
    }
}

/// Run the chain to the end (or Stop via `cancel`), reporting through `sink`. Not sandboxed:
/// each step runs as the user, in `root`, with void's environment plus the profile's (and
/// proto's shims/bin on PATH). Anything a step leaves running in its process group is
/// killed when the step ends; `setsid` daemons are unaffected.
pub async fn run_chain(spec: ChainSpec, cancel: CancellationToken, sink: Sink) {
    let finish = |ok, failed_step, exit_code, stopped, error| {
        sink(RunUpdate::Finished {
            ok,
            failed_step,
            exit_code,
            stopped,
            error,
        })
    };
    let env = match spec.env {
        None => Vec::new(),
        Some((envs, name)) => {
            let job = tokio::task::spawn_blocking(move || envs.resolve(&name));
            tokio::select! {
                r = job => match r {
                    Ok(Ok(vars)) => vars,
                    Ok(Err(e)) => return finish(false, None, None, false, Some(e)),
                    Err(_) => {
                        let e = "couldn't resolve the environment profile".to_string();
                        return finish(false, None, None, false, Some(e));
                    }
                },
                _ = cancel.cancelled() => return finish(false, None, None, true, None),
            }
        }
    };
    // The first failure: (step, exit code).
    let mut failed: Option<(usize, Option<i32>)> = None;
    for (index, sc) in spec.steps.iter().enumerate() {
        if cancel.is_cancelled() {
            return finish(
                false,
                failed.map(|f| f.0),
                failed.and_then(|f| f.1),
                true,
                None,
            );
        }
        sink(RunUpdate::Step {
            index,
            label: step_label(&sc.step),
            base_dirs: step_base_dirs(&sc.step, &spec.root, &spec.tasks),
        });
        match run_step(&sc.step, &spec.root, &env, &cancel, &sink).await {
            StepEnd::Ok => {}
            StepEnd::Stopped => {
                let (step, code) = failed.unwrap_or((index, None));
                return finish(false, Some(step), code, true, None);
            }
            StepEnd::Failed(code) => {
                failed.get_or_insert((index, code));
                if sc.on_failure == OnFailure::Stop {
                    break;
                }
            }
        }
    }
    finish(
        failed.is_none(),
        failed.map(|f| f.0),
        failed.and_then(|f| f.1),
        false,
        None,
    );
}

enum StepEnd {
    Ok,
    /// Exited non-zero (the code), was killed by a signal, or couldn't start (None).
    Failed(Option<i32>),
    Stopped,
}

/// SIGKILLs the step's whole process group when dropped: when the step ends (so nothing it
/// left behind keeps running or holds the output pipe), and if the run's task is dropped.
struct GroupGuard(u32);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        super::kill_group(self.0);
    }
}

async fn run_step(
    step: &Step,
    root: &Path,
    env: &[(String, String)],
    cancel: &CancellationToken,
    sink: &Sink,
) -> StepEnd {
    let fail = |why: String| {
        sink(RunUpdate::Output(format!("void: {why}\n")));
        StepEnd::Failed(None)
    };
    let mut cmd = match step {
        Step::Moon { target, args } => match super::moon::resolve_moon() {
            Ok(moon) => {
                let mut c = tokio::process::Command::new(moon);
                c.arg("run").arg(target);
                if !args.is_empty() {
                    c.arg("--").args(args);
                }
                c
            }
            Err(e) => return fail(e),
        },
        Step::Shell { command } => {
            let mut c = tokio::process::Command::new("bash");
            c.args(["--noprofile", "--norc", "-c", command]);
            c
        }
    };
    // One pipe for stdout and stderr keeps their order as written.
    let (reader, writer) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => return fail(format!("couldn't make a pipe: {e}")),
    };
    let reader = match writer.try_clone().and_then(|w2| {
        cmd.stdout(w2).stderr(writer);
        pipe::Receiver::from_owned_fd(reader.into())
    }) {
        Ok(r) => r,
        Err(e) => return fail(format!("couldn't make a pipe: {e}")),
    };
    // proto's shims and bin go on PATH (after the profile's own PATH, if it sets one).
    let path = env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| std::ffi::OsString::from(v))
        .or_else(|| std::env::var_os("PATH"));
    let path = super::moon::home().and_then(|h| super::moon::proto_path(path.as_deref(), &h));
    cmd.current_dir(root)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .envs(path.map(|p| ("PATH", p)))
        .stdin(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let spawned = cmd.spawn();
    // Our copies of the pipe's write end go with the Command, so EOF comes when the step's
    // processes are gone.
    drop(cmd);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return fail(format!("couldn't start the step: {e}")),
    };
    let Some(pid) = child.id() else {
        return fail("the step vanished before it started".into());
    };
    let guard = GroupGuard(pid);
    let mut pump = tokio::spawn(pump(reader, sink.clone()));
    let end = tokio::select! {
        status = child.wait() => match status {
            Ok(s) if s.success() => StepEnd::Ok,
            Ok(s) => StepEnd::Failed(s.code()),
            Err(_) => StepEnd::Failed(None),
        },
        _ = cancel.cancelled() => {
            // SAFETY: plain syscall on the step's own process group.
            unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
            if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_err() {
                super::kill_group(pid);
                let _ = tokio::time::timeout(DRAIN, child.wait()).await;
            }
            StepEnd::Stopped
        }
    };
    drop(guard);
    if tokio::time::timeout(DRAIN, &mut pump).await.is_err() {
        pump.abort();
    }
    end
}

/// Forward the pipe's output to `sink` in chunks (every `FLUSH` at most), as UTF-8 (invalid
/// bytes replaced, a character split across reads kept whole).
async fn pump(mut r: pipe::Receiver, sink: Sink) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut utf8 = Utf8Stream::default();
    let mut pending = String::new();
    let mut since: Option<tokio::time::Instant> = None;
    let flush = |pending: &mut String| {
        if !pending.is_empty() {
            sink(RunUpdate::Output(std::mem::take(pending)));
        }
    };
    loop {
        let read = match since {
            None => r.read(&mut buf).await,
            Some(t) => match tokio::time::timeout_at(t + FLUSH, r.read(&mut buf)).await {
                Ok(read) => read,
                Err(_) => {
                    flush(&mut pending);
                    since = None;
                    continue;
                }
            },
        };
        match read {
            Ok(0) => break,
            Ok(n) => {
                pending.push_str(&utf8.push(&buf[..n]));
                if pending.len() >= FLUSH_BYTES {
                    flush(&mut pending);
                    since = None;
                } else if since.is_none() && !pending.is_empty() {
                    since = Some(tokio::time::Instant::now());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    pending.push_str(&utf8.finish());
    flush(&mut pending);
}

/// Decodes a byte stream as UTF-8 across chunk boundaries.
#[derive(Default)]
struct Utf8Stream {
    carry: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest: &[u8] = &self.carry;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(good).unwrap_or_default());
                    match e.error_len() {
                        Some(n) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &bad[n..];
                        }
                        // An incomplete character at the end: wait for the rest.
                        None => {
                            rest = bad;
                            break;
                        }
                    }
                }
            }
        }
        self.carry = rest.to_vec();
        out
    }

    fn finish(&mut self) -> String {
        let s = String::from_utf8_lossy(&self.carry).into_owned();
        self.carry.clear();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_with(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let p = dir.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        dir
    }

    #[test]
    fn parse_locations_from_rustc_tsc_generic() {
        let dir = root_with(&["src/x.rs", "web/src/x.ts", "lib/a.c", "README.md"]);
        let root = dir.path();
        let at = |p: &str| root.join(p);

        // rustc
        let line = "  --> src/x.rs:3:5";
        let got = parse_locations(line, &[root]);
        assert_eq!(
            got,
            vec![Location {
                path: at("src/x.rs"),
                line: 3,
                col: Some(5),
                span: 6..18,
            }]
        );
        assert_eq!(&line[got[0].span.clone()], "src/x.rs:3:5");

        // tsc
        let line = "web/src/x.ts(12,7): error TS2322: Type 'string' is not assignable";
        let got = parse_locations(line, &[root]);
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].path.clone(), got[0].line, got[0].col),
            (at("web/src/x.ts"), 12, Some(7))
        );
        assert_eq!(&line[got[0].span.clone()], "web/src/x.ts(12,7)");

        // generic path:line:col and path:line, several per line
        let line = "lib/a.c:10:2: warning: see README.md:4 and src/x.rs:9";
        let got = parse_locations(line, &[root]);
        let short: Vec<_> = got
            .iter()
            .map(|l| (l.path.clone(), l.line, l.col, &line[l.span.clone()]))
            .collect();
        assert_eq!(
            short,
            vec![
                (at("lib/a.c"), 10, Some(2), "lib/a.c:10:2"),
                (at("README.md"), 4, None, "README.md:4"),
                (at("src/x.rs"), 9, None, "src/x.rs:9"),
            ]
        );

        // absolute paths under the root count; outside it, or missing, they don't
        let abs = format!("panicked at {}:7:9:", at("src/x.rs").display());
        let got = parse_locations(&abs, &[root]);
        assert_eq!((got[0].path.clone(), got[0].line), (at("src/x.rs"), 7));
        assert!(parse_locations("/etc/passwd:1:1", &[root]).is_empty());
        assert!(parse_locations("src/missing.rs:1:1", &[root]).is_empty());
        assert!(parse_locations("../x/src/x.rs:1:1", &[root]).is_empty());
        assert!(parse_locations("at 12:30:45 http://localhost:8080/x", &[root]).is_empty());
        assert!(parse_locations("src/x.rs:0:1", &[root]).is_empty());

        // ANSI colours are skipped, the span is in the original line's bytes
        let line = "\x1b[1m\x1b[94m  --> \x1b[0msrc/x.rs\x1b[0m:3:5";
        let got = parse_locations(line, &[root]);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].line, got[0].col), (3, Some(5)));
        assert_eq!(&line[got[0].span.clone()], "src/x.rs\x1b[0m:3:5");
    }

    #[test]
    fn locations_try_the_project_folder_then_the_root() {
        let dir = root_with(&[
            "web/src/app.ts",
            "web/src/both.ts",
            "src/both.ts",
            "README.md",
            "lib/util.ts",
        ]);
        let root = dir.path();
        let bases = [root.join("web"), root.to_path_buf()];
        let found = |line: &str| -> Vec<(PathBuf, usize)> {
            parse_locations(line, &bases)
                .into_iter()
                .map(|l| (l.path, l.line))
                .collect()
        };
        // Relative to the project (tsc run by moon in web/).
        assert_eq!(
            found("src/app.ts(3,5): error TS2322"),
            vec![(root.join("web/src/app.ts"), 3)]
        );
        // Relative to the root still works.
        assert_eq!(found("see README.md:7"), vec![(root.join("README.md"), 7)]);
        // In both: the project's wins.
        assert_eq!(
            found("src/both.ts:2:1"),
            vec![(root.join("web/src/both.ts"), 2)]
        );
        // Up out of the project but still inside the root.
        assert_eq!(
            found("../lib/util.ts:4"),
            vec![(root.join("lib/util.ts"), 4)]
        );
        // Never outside the root.
        assert!(found("../../etc/passwd:1").is_empty());
        let nothing: [PathBuf; 0] = [];
        assert!(parse_locations("README.md:1", &nothing).is_empty());
    }

    #[test]
    fn moon_steps_look_in_their_project_folder() {
        let root = Path::new("/ws");
        let task = |target: &str, source: Option<&str>, from_root: bool| MoonTask {
            project: target.split(':').next().unwrap().into(),
            id: target.split(':').nth(1).unwrap().into(),
            target: target.into(),
            command: "x".into(),
            description: None,
            source: source.map(str::to_string),
            run_from_workspace_root: from_root,
        };
        let tasks = [
            task("web:check", Some("apps/web"), false),
            task("web:lint", Some("apps/web"), true),
            task("workspace:format", Some("."), false),
            task("odd:x", None, false),
        ];
        let moon = |target: &str| Step::Moon {
            target: target.into(),
            args: vec![],
        };
        let dirs = |step: Step| step_base_dirs(&step, root, &tasks);
        assert_eq!(
            dirs(moon("web:check")),
            vec![PathBuf::from("/ws/apps/web"), PathBuf::from("/ws")]
        );
        for only_root in [
            moon("web:lint"),
            moon("workspace:format"),
            moon("odd:x"),
            moon("unknown:task"),
            Step::Shell {
                command: "tsc".into(),
            },
        ] {
            assert_eq!(dirs(only_root), vec![PathBuf::from("/ws")]);
        }
    }

    #[test]
    fn moon_config_paths() {
        for p in [
            "moon.yml",
            "crates/a/moon.yaml",
            ".moon/workspace.yml",
            ".moon",
        ] {
            assert!(is_moon_config_path(Path::new(p)), "{p}");
        }
        for p in [
            "src/moon.rs",
            "moon.yml.bak",
            ".moon/cache/states/x.json",
            ".moon/cache",
            "a/.moonx/y",
        ] {
            assert!(!is_moon_config_path(Path::new(p)), "{p}");
        }
    }
}
