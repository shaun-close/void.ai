//! Tools the agent can call. Every tool works on the conversation's overlay and is confined to
//! the workspace; nothing here writes the real workspace.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::backend::ToolSpec;
use crate::overlay::Overlay;

pub mod bash;
pub mod fs;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolKind {
    Read,
    Edit,
    Exec,
}

/// What a tool hands back to the model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Everything a tool run may use.
#[derive(Clone)]
pub struct ToolCtx {
    pub workspace: PathBuf,
    pub overlay: Overlay,
    /// Paths no tool may touch (keys, browser profiles, void.ai's own data…).
    pub hidden: Arc<Vec<PathBuf>>,
    pub cancel: CancellationToken,
    /// Live output (bash) for the UI.
    pub stream: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Where bash runs; None means bash is unavailable.
    pub sandbox: Option<Arc<crate::sandbox::Sandbox>>,
    /// The workspace's read-write CARGO_TARGET_DIR for sandboxed builds.
    pub target_dir: PathBuf,
    /// Called when bash has to wait for the sandbox (another sandbox holds the cache lock).
    pub on_wait: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema of the arguments (an object).
    fn parameters(&self) -> Value;
    fn kind(&self) -> ToolKind;
    /// One line for tool cards and approval prompts.
    fn summary(&self, args: &Value) -> String;
    fn run(&self, args: Value, ctx: ToolCtx) -> BoxFuture<'static, ToolOutput>;
}

/// The tools void.ai knows; agent profiles pick from them by name.
#[derive(Clone)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn builtin() -> Registry {
        let mut tools = fs::all();
        tools.push(Arc::new(bash::Bash));
        Registry { tools }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.retain(|t| t.name() != tool.name());
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    /// Specs for the named tools, in the order given; unknown names are skipped.
    pub fn specs(&self, names: &[String]) -> Vec<ToolSpec> {
        names
            .iter()
            .filter_map(|n| self.get(n))
            .map(|t| ToolSpec {
                name: t.name().into(),
                description: t.description().into(),
                parameters: t.parameters(),
            })
            .collect()
    }
}

/// Lexically normalise; `None` when `..` climbs above the start of a relative path.
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
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

fn outside(raw: &str) -> String {
    format!("{raw} is outside the workspace; tools only work inside it")
}

/// A symlink at `rel` in the merged view: its target.
fn link_target(ov: &Overlay, rel: &Path) -> Option<PathBuf> {
    let up = ov.upper().join(rel);
    match std::fs::symlink_metadata(&up) {
        Ok(m) if m.file_type().is_symlink() => std::fs::read_link(&up).ok(),
        Ok(_) => None,
        Err(_) => {
            let low = ov.workspace().join(rel);
            match std::fs::symlink_metadata(&low) {
                Ok(m) if m.file_type().is_symlink() => std::fs::read_link(&low).ok(),
                _ => None,
            }
        }
    }
}

/// Turn a path argument into a workspace-relative path, following symlinks through the merged
/// view. Refuses anything that ends up outside the workspace or under a hidden path.
pub fn confine(ov: &Overlay, hidden: &[PathBuf], raw: &str) -> Result<PathBuf, String> {
    let ws = ov.workspace();
    let ws_real = ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf());
    let to_rel = |abs: &Path| -> Option<PathBuf> {
        abs.strip_prefix(ws)
            .or_else(|_| abs.strip_prefix(&ws_real))
            .ok()
            .map(Path::to_path_buf)
    };
    let raw_path = Path::new(raw.trim());
    let start = if raw_path.is_absolute() {
        to_rel(&normalize(raw_path).ok_or_else(|| outside(raw))?).ok_or_else(|| outside(raw))?
    } else {
        normalize(raw_path).ok_or_else(|| outside(raw))?
    };
    let mut remaining: VecDeque<OsString> = start.iter().map(|c| c.to_os_string()).collect();
    let mut rel = PathBuf::new();
    let mut hops = 0;
    while let Some(c) = remaining.pop_front() {
        let cand = rel.join(&c);
        match link_target(ov, &cand) {
            None => rel = cand,
            Some(target) => {
                hops += 1;
                if hops > 40 {
                    return Err(format!("{raw}: too many symlinks"));
                }
                let abs = if target.is_absolute() {
                    target
                } else {
                    ws.join(&rel).join(target)
                };
                let abs = normalize(&abs).ok_or_else(|| outside(raw))?;
                let r = to_rel(&abs).ok_or_else(|| outside(raw))?;
                let mut next: VecDeque<OsString> = r.iter().map(|c| c.to_os_string()).collect();
                next.extend(remaining);
                remaining = next;
                rel = PathBuf::new();
            }
        }
    }
    let abs = ws.join(&rel);
    let abs_real = ws_real.join(&rel);
    for h in hidden {
        let h_real = h.canonicalize().unwrap_or_else(|_| h.clone());
        if abs.starts_with(h) || abs_real.starts_with(&h_real) {
            return Err(format!("{raw} is in a protected location"));
        }
    }
    Ok(rel)
}

pub(crate) fn arg_str(args: &Value, key: &str) -> Result<String, ToolOutput> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolOutput::err(format!("missing string argument `{key}`")))
}
