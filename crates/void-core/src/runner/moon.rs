//! moon task discovery via `moon query tasks` / `moon query projects`, and the PATH that
//! lets runs find proto's tools.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One moon task, for display and for building run steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoonTask {
    pub project: String,
    pub id: String,
    /// `project:id`.
    pub target: String,
    /// The task's command and args joined, for display.
    pub command: String,
    pub description: Option<String>,
    /// The project's folder relative to the workspace root (`.` for a root-level project),
    /// from `moon query projects`; None when moon didn't say.
    pub source: Option<String>,
    /// The task runs in the workspace root instead of its project's folder.
    pub run_from_workspace_root: bool,
}

#[derive(Deserialize)]
struct Query {
    #[serde(default)]
    tasks: std::collections::BTreeMap<String, std::collections::BTreeMap<String, RawTask>>,
}

#[derive(Deserialize)]
struct RawTask {
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    options: RawOptions,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOptions {
    #[serde(default)]
    run_from_workspace_root: bool,
}

pub fn parse_moon_tasks(json: &str) -> Result<Vec<MoonTask>, String> {
    let q: Query =
        serde_json::from_str(json).map_err(|e| format!("unreadable moon output: {e}"))?;
    let mut out = Vec::new();
    for (project, tasks) in q.tasks {
        for (id, t) in tasks {
            let command = std::iter::once(t.command.as_str())
                .chain(t.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            out.push(MoonTask {
                target: format!("{project}:{id}"),
                project: project.clone(),
                id,
                command,
                description: t.description.filter(|d| !d.trim().is_empty()),
                source: None,
                run_from_workspace_root: t.options.run_from_workspace_root,
            });
        }
    }
    Ok(out)
}

const NOT_FOUND: &str = "moon not found — install it with proto, or put it on PATH";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const STDERR_CAP: usize = 500;

/// The user's home (absolute), if known.
pub(crate) fn home() -> Option<PathBuf> {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .filter(|h| h.is_absolute())
}

/// PATH, then `~/.proto/shims/moon`, then `~/.proto/bin/moon`.
pub(crate) fn resolve_moon() -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let home = home().ok_or_else(|| NOT_FOUND.to_string())?;
    if let Ok(p) = crate::claude_code::turn::resolve_command("moon", &home) {
        return Ok(p);
    }
    PROTO_DIRS
        .iter()
        .map(|d| home.join(".proto").join(d).join("moon"))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| NOT_FOUND.to_string())
}

const PROTO_DIRS: [&str; 2] = ["shims", "bin"];

/// Run `moon query tasks` and `moon query projects` in `root`: the tasks, each with its
/// project's folder (left unknown if only the projects query fails).
pub async fn discover(root: &Path) -> Result<Vec<MoonTask>, String> {
    let moon = resolve_moon()?;
    let path = home().and_then(|h| proto_path(std::env::var_os("PATH").as_deref(), &h));
    let path = path.as_deref();
    let mut tasks = parse_moon_tasks(&query(&moon, root, "tasks", path).await?)?;
    if let Ok(projects) = query(&moon, root, "projects", path)
        .await
        .and_then(|json| parse_moon_projects(&json))
    {
        attach_sources(&mut tasks, &projects);
    }
    Ok(tasks)
}

/// `moon query <what>`'s stdout, killed (with its process group) after 30 s. `path`: the
/// PATH to give moon (proto's dirs added, as for run steps), None to leave it.
async fn query(
    moon: &Path,
    root: &Path,
    what: &str,
    path: Option<&OsStr>,
) -> Result<String, String> {
    let child = tokio::process::Command::new(moon)
        .args(["query", what])
        .envs(path.map(|p| ("PATH", p)))
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("couldn't run moon: {e}"))?;
    let pid = child.id();
    let out = match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
        Ok(r) => r.map_err(|e| format!("couldn't run moon: {e}"))?,
        Err(_) => {
            if let Some(pid) = pid {
                super::kill_group(pid);
            }
            return Err(format!("moon query {what} timed out after 30 s"));
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err: String = err.trim().chars().take(STDERR_CAP).collect();
        return Err(format!("moon query {what} failed ({}): {err}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Deserialize)]
struct Projects {
    #[serde(default)]
    projects: Vec<RawProject>,
}

#[derive(Deserialize)]
struct RawProject {
    id: String,
    source: String,
}

/// `moon query projects` output: project id → its folder relative to the workspace root.
pub fn parse_moon_projects(json: &str) -> Result<BTreeMap<String, String>, String> {
    let q: Projects =
        serde_json::from_str(json).map_err(|e| format!("unreadable moon output: {e}"))?;
    Ok(q.projects.into_iter().map(|p| (p.id, p.source)).collect())
}

/// Fill in each task's project folder.
pub fn attach_sources(tasks: &mut [MoonTask], projects: &BTreeMap<String, String>) {
    for t in tasks {
        t.source = projects.get(&t.project).cloned();
    }
}

/// The PATH for a run's steps: `current` with proto's `~/.proto/shims` and `~/.proto/bin`
/// put in front when they exist and aren't on it already (a desktop launch often lacks
/// them, and moon finds its toolchains through them). The existing entries keep their
/// order. None: nothing to add, leave PATH as it is.
pub(crate) fn proto_path(current: Option<&OsStr>, home: &Path) -> Option<OsString> {
    let dirs: Vec<PathBuf> = current
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    let add: Vec<PathBuf> = PROTO_DIRS
        .iter()
        .map(|d| home.join(".proto").join(d))
        .filter(|d| d.is_dir() && !dirs.contains(d))
        .collect();
    if add.is_empty() {
        return None;
    }
    std::env::join_paths(add.into_iter().chain(dirs)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_give_tasks_their_folders() {
        let tasks_json = include_str!("../../tests/fixtures/moon-query-tasks.json");
        let projects = parse_moon_projects(include_str!(
            "../../tests/fixtures/moon-query-projects.json"
        ))
        .unwrap();
        assert_eq!(
            projects.get("void-gui").map(String::as_str),
            Some("apps/void-gui")
        );
        assert_eq!(projects.get("workspace").map(String::as_str), Some("."));
        let mut tasks = parse_moon_tasks(tasks_json).unwrap();
        assert!(tasks.iter().all(|t| !t.run_from_workspace_root));
        attach_sources(&mut tasks, &projects);
        let run = tasks.iter().find(|t| t.target == "void-gui:run").unwrap();
        assert_eq!(run.source.as_deref(), Some("apps/void-gui"));
        let fmt = tasks
            .iter()
            .find(|t| t.target == "workspace:format")
            .unwrap();
        assert_eq!(fmt.source.as_deref(), Some("."));
        assert!(parse_moon_projects("nope").is_err());
        // runFromWorkspaceRoot is read from the task's options.
        let json = r#"{"tasks":{"web":{"lint":{"command":"eslint","options":{"runFromWorkspaceRoot":true}}}}}"#;
        assert!(parse_moon_tasks(json).unwrap()[0].run_from_workspace_root);
    }

    #[test]
    fn moon_queries_get_the_given_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("moon");
        std::fs::write(&fake, "#!/bin/sh\necho \"$1 $2 $PATH\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let want = "/home/x/.proto/shims:/home/x/.proto/bin:/usr/bin:/bin";
        let out = rt
            .block_on(query(&fake, dir.path(), "tasks", Some(OsStr::new(want))))
            .unwrap();
        assert_eq!(out.trim(), format!("query tasks {want}"));
        // None leaves void's own PATH.
        let out = rt
            .block_on(query(&fake, dir.path(), "projects", None))
            .unwrap();
        let own = std::env::var("PATH").unwrap();
        assert_eq!(out.trim(), format!("query projects {own}"));
    }

    #[test]
    fn proto_dirs_go_in_front_of_path() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let shims = h.join(".proto/shims");
        let bin = h.join(".proto/bin");
        // Neither exists: PATH is left alone.
        assert_eq!(proto_path(Some(OsStr::new("/usr/bin:/bin")), h), None);
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let joined = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();
        assert_eq!(
            proto_path(Some(OsStr::new("/usr/bin:/bin")), h),
            Some(joined(&[
                &shims,
                &bin,
                Path::new("/usr/bin"),
                Path::new("/bin")
            ]))
        );
        // One already there keeps its place; only the other is added, the rest untouched.
        let current = joined(&[Path::new("/usr/bin"), &bin, Path::new("/bin")]);
        assert_eq!(
            proto_path(Some(&current), h),
            Some(joined(&[
                &shims,
                Path::new("/usr/bin"),
                &bin,
                Path::new("/bin")
            ]))
        );
        let both = joined(&[&bin, Path::new("/x"), &shims]);
        assert_eq!(proto_path(Some(&both), h), None);
        // No PATH at all: just proto's.
        assert_eq!(proto_path(None, h), Some(joined(&[&shims, &bin])));
    }

    #[test]
    fn parse_moon_tasks_from_real_output() {
        let json = include_str!("../../tests/fixtures/moon-query-tasks.json");
        let tasks = parse_moon_tasks(json).unwrap();
        assert_eq!(tasks.len(), 6);
        let lint = tasks.iter().find(|t| t.target == "workspace:lint").unwrap();
        assert_eq!(lint.project, "workspace");
        assert_eq!(lint.id, "lint");
        assert_eq!(
            lint.command,
            "cargo clippy --workspace --all-targets --locked -- -D warnings"
        );
        assert!(tasks.iter().any(|t| t.target == "void-gui:run"));
        assert!(parse_moon_tasks("not json").is_err());
    }
}
