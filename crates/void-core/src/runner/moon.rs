//! moon task discovery via `moon query tasks`.

use std::path::Path;

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
            });
        }
    }
    Ok(out)
}

const NOT_FOUND: &str = "moon not found — install it with proto, or put it on PATH";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const STDERR_CAP: usize = 500;

/// PATH, then `~/.proto/shims/moon`, then `~/.proto/bin/moon`.
fn resolve_moon() -> Result<std::path::PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_default();
    if let Ok(p) = crate::claude_code::turn::resolve_command("moon", &home) {
        return Ok(p);
    }
    ["shims", "bin"]
        .iter()
        .map(|d| home.join(".proto").join(d).join("moon"))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| NOT_FOUND.to_string())
}

/// Run `moon query tasks` in `root` and parse the result.
pub async fn discover(root: &Path) -> Result<Vec<MoonTask>, String> {
    let moon = resolve_moon()?;
    let mut cmd = tokio::process::Command::new(moon);
    cmd.args(["query", "tasks"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .map_err(|_| "moon query tasks timed out after 30 s".to_string())?
        .map_err(|e| format!("couldn't run moon: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err: String = err.trim().chars().take(STDERR_CAP).collect();
        return Err(format!("moon query tasks failed ({}): {err}", out.status));
    }
    parse_moon_tasks(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

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
