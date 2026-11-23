//! The bash tool: one sandboxed command, its changes landing in the conversation's overlay.

use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_str};
use crate::sandbox::{Availability, RunPlan};

pub struct Bash;

impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command (bash -c) in the workspace, inside a sandbox: the rest of the disk is read-only, .git is read-only, file changes land in the pending review (not on disk), and the network is available unless turned off. Returns the exit code and the last 30 KB of output. Default timeout 120 s, at most 600 s."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "command":{"type":"string","description":"The command to run"},
            "timeout_secs":{"type":"integer","description":"Timeout in seconds (max 600)"}},
            "required":["command"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Exec
    }
    fn summary(&self, args: &Value) -> String {
        args.get("command")
            .and_then(Value::as_str)
            .and_then(|c| c.lines().next())
            .unwrap_or("?")
            .chars()
            .take(80)
            .collect()
    }
    fn run(&self, args: Value, ctx: ToolCtx) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let command = match arg_str(&args, "command") {
                Ok(c) => c,
                Err(e) => return e,
            };
            let Some(sandbox) = ctx.sandbox.clone() else {
                return ToolOutput::err(
                    "bash is unavailable: there is no sandbox for this workspace",
                );
            };
            if let Availability::Unavailable(why) = sandbox.availability().await {
                return ToolOutput::err(format!("bash is unavailable: {why}"));
            }
            let plan = RunPlan {
                workspace: ctx.workspace.clone(),
                upper: ctx.overlay.upper(),
                work: ctx.overlay.work(),
                target_dir: ctx.target_dir.clone(),
                command,
                timeout_secs: args.get("timeout_secs").and_then(Value::as_u64),
            };
            let secs = plan
                .timeout_secs
                .unwrap_or(sandbox.settings().timeout_secs)
                .clamp(1, 600);
            let on_wait = ctx.on_wait.clone();
            let res = sandbox
                .run(plan, ctx.cancel.clone(), ctx.stream.clone(), move || {
                    if let Some(f) = on_wait {
                        f()
                    }
                })
                .await;
            let ov = ctx.overlay.clone();
            let _ = tokio::task::spawn_blocking(move || ov.snapshot_bases()).await;
            let mut body = String::new();
            if res.cut_bytes > 0 {
                body.push_str(&format!("…({} bytes cut)\n", res.cut_bytes));
            }
            body.push_str(&res.output);
            if res.cancelled {
                ToolOutput::err(format!("stopped\n{body}"))
            } else if res.timed_out {
                ToolOutput::err(format!("timed out after {secs}s\n{body}"))
            } else {
                match res.exit {
                    Some(0) => ToolOutput::ok(format!("exit code: 0\n{body}")),
                    Some(n) => ToolOutput::err(format!("exit code: {n}\n{body}")),
                    None => ToolOutput::err(format!("the command was killed\n{body}")),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::overlay::Overlay;
    use crate::sandbox::{Availability, Sandbox, SandboxSettings};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    struct T {
        _tmp: tempfile::TempDir,
        ws: PathBuf,
        ov: Overlay,
        ctx: ToolCtx,
    }

    async fn setup(with_sandbox: bool) -> Option<T> {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(tmp.path().join("home")).unwrap();
        let ov = Overlay::open(&tmp.path().join("ovl"), &ws).unwrap();
        let sandbox = if with_sandbox {
            let sb = Sandbox::new(
                tmp.path().join("data"),
                tmp.path().join("home"),
                SandboxSettings {
                    caches: vec![],
                    ..SandboxSettings::default()
                },
            );
            if let Availability::Unavailable(why) = sb.availability().await {
                eprintln!("skipping: {why}");
                return None;
            }
            Some(Arc::new(sb))
        } else {
            None
        };
        let ctx = ToolCtx {
            workspace: ws.clone(),
            overlay: ov.clone(),
            hidden: Arc::new(vec![]),
            cancel: tokio_util::sync::CancellationToken::new(),
            stream: None,
            sandbox,
            target_dir: tmp.path().join("target"),
            on_wait: None,
        };
        Some(T {
            _tmp: tmp,
            ws,
            ov,
            ctx,
        })
    }

    #[tokio::test]
    async fn bash_runs_in_overlay() {
        let Some(t) = setup(true).await else { return };
        let out = Registry::builtin()
            .get("bash")
            .unwrap()
            .run(
                json!({"command": "echo x > out.txt && cat out.txt"}),
                t.ctx.clone(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("exit code: 0"));
        assert!(out.content.contains('x'));
        assert!(!t.ws.join("out.txt").exists());
        assert_eq!(t.ov.read(std::path::Path::new("out.txt")).unwrap(), b"x\n");
        assert_eq!(t.ov.changes().len(), 1);
    }

    #[tokio::test]
    async fn bash_nonzero_exit_is_an_error_result() {
        let Some(t) = setup(true).await else { return };
        let out = Registry::builtin()
            .get("bash")
            .unwrap()
            .run(json!({"command": "echo oops; exit 3"}), t.ctx.clone())
            .await;
        assert!(out.is_error);
        assert!(out.content.starts_with("exit code: 3"));
        assert!(out.content.contains("oops"));
    }

    #[tokio::test]
    async fn bash_stop_kills_process_group() {
        let Some(t) = setup(true).await else { return };
        let ctx = t.ctx.clone();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel();
        });
        let start = Instant::now();
        let out = Registry::builtin()
            .get("bash")
            .unwrap()
            .run(json!({"command": "sleep 31.7 & sleep 31.7; wait"}), ctx)
            .await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert!(out.content.starts_with("stopped"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let left = std::process::Command::new("pgrep")
            .args(["-f", "sleep 31.7"])
            .output()
            .unwrap();
        assert!(
            left.stdout.is_empty(),
            "left running: {}",
            String::from_utf8_lossy(&left.stdout)
        );
    }

    #[tokio::test]
    async fn bash_reports_unavailable_without_sandbox() {
        let Some(t) = setup(false).await else { return };
        let out = Registry::builtin()
            .get("bash")
            .unwrap()
            .run(json!({"command": "ls"}), t.ctx.clone())
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("unavailable"));
    }
}
