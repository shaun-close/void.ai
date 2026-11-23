//! Environment profiles, stored as `env.toml`. Values exist only in memory once resolved:
//! never logged, never in errors, never in conversation files.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::config::{load_toml, save_toml};

/// Where a variable's value comes from. TOML: `value = "…"`, `env = "VAR"`, `cmd = "…"`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvValue {
    /// A literal.
    Value(String),
    /// Copied from void's own environment variable of that name.
    Env(String),
    /// Stdout (trimmed) of `bash --noprofile --norc -c <cmd>`, run outside any sandbox.
    Cmd(String),
}

impl fmt::Debug for EnvValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(_) => f.write_str("Value(<hidden>)"),
            Self::Env(v) => write!(f, "Env({v:?})"),
            Self::Cmd(_) => f.write_str("Cmd(<hidden>)"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvProfile {
    pub name: String,
    #[serde(default)]
    pub vars: BTreeMap<String, EnvValue>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default)]
    pub profiles: Vec<EnvProfile>,
}

const CMD_TIMEOUT: Duration = Duration::from_secs(10);
const STDERR_CAP: usize = 200;

impl Envs {
    /// Load `path`; missing gives none, an unreadable file is set aside with a warning.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        load_toml(path)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        save_toml(self, path)
    }

    /// The profile's variables as `(name, value)` pairs, in name order. Errors name the
    /// profile and variable, never a value.
    pub fn resolve(&self, name: &str) -> Result<Vec<(String, String)>, String> {
        let profile = self
            .profiles
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("environment profile '{name}' doesn't exist"))?;
        profile
            .vars
            .iter()
            .map(|(var, v)| {
                let value = match v {
                    EnvValue::Value(s) => s.clone(),
                    EnvValue::Env(src) => std::env::var(src).map_err(|_| {
                        format!(
                            "profile '{name}': {var} needs environment variable {src}, which isn't set"
                        )
                    })?,
                    EnvValue::Cmd(cmd) => run_cmd(cmd, CMD_TIMEOUT)
                        .map_err(|why| format!("profile '{name}': {var} failed: {why}"))?,
                };
                Ok((var.clone(), value))
            })
            .collect()
    }
}

/// Run `cmd` through bash (outside any sandbox). The error text is safe to show: stderr is
/// capped and lines containing the command's own stdout (the would-be value) are redacted.
fn run_cmd(cmd: &str, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't start bash: {e}"))?;
    fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            String::from_utf8_lossy(&b).into_owned()
        })
    }
    let out = drain(child.stdout.take().expect("piped"));
    let err = drain(child.stderr.take().expect("piped"));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("couldn't wait for the command: {e}")),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let Some(status) = status else {
        return Err(format!("command timed out after {} s", timeout.as_secs()));
    };
    let value = stdout.trim().to_string();
    if status.success() {
        return Ok(value);
    }
    let shown: Vec<&str> = stderr
        .lines()
        .map(|l| {
            if !value.is_empty() && l.contains(&value) {
                "[redacted]"
            } else {
                l
            }
        })
        .collect();
    let shown: String = shown.join("\n").trim().chars().take(STDERR_CAP).collect();
    // A cut could split a redacted marker's neighbour but never reveals the value: lines
    // containing it were replaced before capping.
    let code = status
        .code()
        .map_or("killed by a signal".to_string(), |c| format!("exit {c}"));
    Err(if shown.is_empty() {
        format!("command failed ({code})")
    } else {
        format!("command failed ({code}): {shown}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(vars: &[(&str, EnvValue)]) -> Envs {
        Envs {
            active: Some("dev".into()),
            profiles: vec![EnvProfile {
                name: "dev".into(),
                vars: vars
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            }],
        }
    }

    #[test]
    fn env_resolution_literal_env_cmd() {
        // PATH is always set; reading it avoids mutating the process environment.
        let want_path = std::env::var("PATH").unwrap();
        let envs = profile(&[
            ("A_LIT", EnvValue::Value("plain".into())),
            ("B_ENV", EnvValue::Env("PATH".into())),
            ("C_CMD", EnvValue::Cmd("echo '  tok-123  '".into())),
        ]);
        let got = envs.resolve("dev").unwrap();
        assert_eq!(
            got,
            vec![
                ("A_LIT".to_string(), "plain".to_string()),
                ("B_ENV".to_string(), want_path),
                ("C_CMD".to_string(), "tok-123".to_string()),
            ]
        );
        assert!(envs.resolve("nope").unwrap_err().contains("nope"));
        let missing = profile(&[("X", EnvValue::Env("VOID_SURELY_UNSET_VAR".into()))]);
        let e = missing.resolve("dev").unwrap_err();
        assert!(e.contains("dev") && e.contains('X'));
    }

    #[test]
    fn env_toml_shapes_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env.toml");
        let envs = profile(&[
            ("A", EnvValue::Value("v".into())),
            ("B", EnvValue::Env("HOME".into())),
            ("C", EnvValue::Cmd("pass show x".into())),
        ]);
        envs.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("value = \"v\"") && text.contains("env = \"HOME\""));
        assert!(text.contains("cmd = \"pass show x\""));
        assert_eq!(Envs::load(&path), (envs, None));
        let dbg = format!("{:?}", EnvValue::Value("hunter2".into()));
        assert!(!dbg.contains("hunter2"));
    }

    #[test]
    fn env_cmd_failure_blocks_run_without_leaking() {
        let envs = profile(&[(
            "API_KEY",
            EnvValue::Cmd(
                "echo s3cr3t-value; echo 'bad token s3cr3t-value' >&2; echo 'vault locked' >&2; exit 3"
                    .into(),
            ),
        )]);
        let e = envs.resolve("dev").unwrap_err();
        assert!(e.contains("dev") && e.contains("API_KEY"), "{e}");
        assert!(!e.contains("s3cr3t-value"), "{e}");
        assert!(e.contains("vault locked") && e.contains("exit 3"), "{e}");

        let long = profile(&[(
            "K",
            EnvValue::Cmd("yes x | head -c 5000 >&2; exit 1".into()),
        )]);
        assert!(long.resolve("dev").unwrap_err().len() < 400);

        assert!(
            run_cmd("sleep 5", Duration::from_millis(100))
                .unwrap_err()
                .contains("timed out")
        );
    }
}
