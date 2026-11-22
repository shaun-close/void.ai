//! Agent profiles (build, plan) and the system prompt an agent turn starts from.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::model::Workspace;
use crate::overlay::Overlay;
use crate::sandbox::Availability;

const INSTRUCTIONS_CAP: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    pub prompt: String,
    /// Tools offered to the model.
    pub tools: Vec<String>,
    /// Tools always refused, whatever the workspace rules say.
    #[serde(default)]
    pub deny: Vec<String>,
}

impl AgentProfile {
    pub fn allows(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t == tool) && !self.deny.iter().any(|t| t == tool)
    }
}

pub fn default_agents() -> Vec<AgentProfile> {
    let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        AgentProfile {
            id: "build".into(),
            name: "Build".into(),
            prompt: "You are working as a coding agent in the user's workspace. Read the relevant code \
                before changing it. Make focused changes with `edit` (exact text replacement) or \
                `write`. Your file changes are staged for the user's review, not written to disk, and \
                you always see your own pending changes. Use `bash` to build and run tests; it runs \
                in a sandbox. When you're done, say briefly what you changed and what's left."
                .into(),
            tools: v(&["read", "list", "glob", "grep", "edit", "write", "bash"]),
            deny: Vec::new(),
        },
        AgentProfile {
            id: "plan".into(),
            name: "Plan".into(),
            prompt: "You are in plan mode: read and search the code to understand it, then propose a \
                concrete plan. You can't edit files or run commands in this mode."
                .into(),
            tools: v(&["read", "list", "glob", "grep"]),
            deny: v(&["edit", "write", "bash"]),
        },
    ]
}

fn instructions(root: &Path, overlay: Option<&Overlay>) -> Option<(String, String)> {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let bytes = match overlay {
            Some(ov) => ov.read(Path::new(name)).ok(),
            None => std::fs::read(root.join(name)).ok(),
        };
        if let Some(b) = bytes {
            let mut text = String::from_utf8_lossy(&b).into_owned();
            if text.len() > INSTRUCTIONS_CAP {
                let mut cut = INSTRUCTIONS_CAP;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push_str("\n…(truncated)");
            }
            return Some((name.to_string(), text));
        }
    }
    None
}

/// Base prompt, the agent's prompt, an environment block and the workspace's AGENTS.md (or
/// CLAUDE.md). Scratch (no folder, no tools) gets the base prompt only.
pub fn system_prompt(
    cfg: &Config,
    agent: &AgentProfile,
    workspace: &Workspace,
    overlay: Option<&Overlay>,
    sandbox: &Availability,
) -> String {
    let Some(root) = &workspace.root else {
        return cfg.base_system_prompt.clone();
    };
    let mut p = format!(
        "{}\n\n{}\n\n<environment>\nWorkspace: {}\nOS: {}\nDate: {}\n",
        cfg.base_system_prompt,
        agent.prompt,
        root.display(),
        std::env::consts::OS,
        chrono::Local::now().format("%Y-%m-%d"),
    );
    if agent.allows("bash") {
        match sandbox {
            Availability::Ready { landlock } => p.push_str(&format!(
                "Shell: available, sandboxed{} — the rest of the disk is read-only, your file changes go to review, network is on.\n",
                if *landlock { " (bubblewrap + Landlock)" } else { " (bubblewrap)" }
            )),
            Availability::Unavailable(why) => p.push_str(&format!("Shell: unavailable ({why}).\n")),
        }
    }
    if root.join(".git").exists() {
        p.push_str("Git: .git is read-only for you (status, diff and log work); the user commits after reviewing.\n");
    }
    p.push_str("</environment>");
    if let Some((name, text)) = instructions(root, overlay) {
        p.push_str(&format!("\n\nProject instructions ({name}):\n{text}"));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Workspace, WorkspaceId};
    use crate::overlay::Overlay;
    use crate::sandbox::Availability;

    fn ws(root: &std::path::Path) -> Workspace {
        Workspace {
            id: WorkspaceId("w".into()),
            name: "w".into(),
            root: Some(root.to_path_buf()),
        }
    }

    #[test]
    fn plan_agent_denies_edits() {
        let agents = default_agents();
        let build = agents.iter().find(|a| a.id == "build").unwrap();
        let plan = agents.iter().find(|a| a.id == "plan").unwrap();
        assert_eq!(build.tools.len(), 7);
        for t in ["edit", "write", "bash"] {
            assert!(plan.deny.iter().any(|d| d == t));
            assert!(!plan.tools.iter().any(|x| x == t));
        }
        assert!(plan.allows("read") && !plan.allows("write") && build.allows("bash"));
    }

    #[test]
    fn old_config_loads_with_agent_defaults() {
        let old = r#"
            active_profile = "strata"
            font_size = 15.0
        "#;
        let c: Config = toml::from_str(old).unwrap();
        assert_eq!(c.default_agent, "build");
        assert_eq!(c.agents.len(), 2);
        assert!(c.sandbox.network);
    }

    #[test]
    fn system_prompt_includes_agents_md_from_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("CLAUDE.md"), "use the claude file").unwrap();
        let ov = Overlay::open(&tmp.path().join("o"), &root).unwrap();
        ov.write(std::path::Path::new("AGENTS.md"), b"Always run moon :test.")
            .unwrap();
        let cfg = Config::default();
        let build = &cfg.agents[0];
        let p = system_prompt(
            &cfg,
            build,
            &ws(&root),
            Some(&ov),
            &Availability::Ready { landlock: true },
        );
        assert!(p.starts_with(&cfg.base_system_prompt));
        assert!(p.contains(&build.prompt));
        assert!(p.contains("Always run moon :test."));
        assert!(
            !p.contains("use the claude file"),
            "AGENTS.md wins over CLAUDE.md"
        );
        assert!(p.contains(&root.display().to_string()));
        assert!(p.contains(".git is read-only"));
        assert!(p.contains("sandboxed"));
    }

    #[test]
    fn system_prompt_caps_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("AGENTS.md"), "x".repeat(40_000)).unwrap();
        let cfg = Config::default();
        let p = system_prompt(
            &cfg,
            &cfg.agents[0],
            &ws(&root),
            None,
            &Availability::Unavailable("no bwrap".into()),
        );
        assert!(p.contains("…(truncated)"));
        assert!(p.len() < 36_000);
        assert!(p.contains("no bwrap"));
    }

    #[test]
    fn scratch_prompt_is_just_the_base() {
        let cfg = Config::default();
        let p = system_prompt(
            &cfg,
            &cfg.agents[0],
            &Workspace::scratch(),
            None,
            &Availability::Ready { landlock: true },
        );
        assert_eq!(p, cfg.base_system_prompt);
    }
}
