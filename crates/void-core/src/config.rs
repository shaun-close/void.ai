//! App configuration and backend profiles (`~/.config/void.ai/config.toml`).

use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::fsutil;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendKind {
    /// OpenAI-compatible `/v1/chat/completions` (Strata, llama-server, Ollama `/v1`).
    #[default]
    #[serde(rename = "openai")]
    OpenAi,
    /// Anthropic-style `/v1/messages`.
    #[serde(rename = "anthropic")]
    Anthropic,
}

/// Where a profile's API key comes from. Serialised as one of the optional profile fields
/// `api_key`, `api_key_env` or `api_key_cmd`.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum ApiKeySource {
    #[default]
    None,
    Literal(String),
    Env(String),
    /// A shell command whose trimmed stdout is the key.
    Command(String),
}

impl fmt::Debug for ApiKeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Literal(_) => f.write_str("Literal(***)"),
            Self::Env(v) => f.debug_tuple("Env").field(v).finish(),
            Self::Command(c) => f.debug_tuple("Command").field(c).finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ProfileRepr", into = "ProfileRepr")]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub kind: BackendKind,
    pub base_url: String,
    pub api_key: ApiKeySource,
    pub model: String,
    pub context_limit: u32,
    pub max_output: u32,
    /// Merged into every request body (e.g. `chat_template_kwargs.enable_thinking`).
    pub extra_body: Map<String, Value>,
}

#[derive(Serialize, Deserialize)]
struct ProfileRepr {
    id: String,
    name: String,
    kind: BackendKind,
    base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key_cmd: Option<String>,
    model: String,
    context_limit: u32,
    max_output: u32,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    extra_body: Map<String, Value>,
}

impl TryFrom<ProfileRepr> for Profile {
    type Error = String;
    fn try_from(r: ProfileRepr) -> Result<Self, String> {
        let api_key = match (r.api_key, r.api_key_env, r.api_key_cmd) {
            (None, None, None) => ApiKeySource::None,
            (Some(k), None, None) => ApiKeySource::Literal(k),
            (None, Some(v), None) => ApiKeySource::Env(v),
            (None, None, Some(c)) => ApiKeySource::Command(c),
            _ => {
                return Err(format!(
                    "profile {:?}: set at most one of api_key, api_key_env, api_key_cmd",
                    r.id
                ));
            }
        };
        Ok(Self {
            id: r.id,
            name: r.name,
            kind: r.kind,
            base_url: r.base_url,
            api_key,
            model: r.model,
            context_limit: r.context_limit,
            max_output: r.max_output,
            extra_body: r.extra_body,
        })
    }
}

impl From<Profile> for ProfileRepr {
    fn from(p: Profile) -> Self {
        let (api_key, api_key_env, api_key_cmd) = match p.api_key {
            ApiKeySource::None => (None, None, None),
            ApiKeySource::Literal(k) => (Some(k), None, None),
            ApiKeySource::Env(v) => (None, Some(v), None),
            ApiKeySource::Command(c) => (None, None, Some(c)),
        };
        Self {
            id: p.id,
            name: p.name,
            kind: p.kind,
            base_url: p.base_url,
            api_key,
            api_key_env,
            api_key_cmd,
            model: p.model,
            context_limit: p.context_limit,
            max_output: p.max_output,
            extra_body: p.extra_body,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub active_profile: String,
    pub base_system_prompt: String,
    pub theme: Theme,
    pub font_size: f32,
    pub profiles: Vec<Profile>,
    /// Agent profiles (build, plan, …).
    pub agents: Vec<crate::agent::AgentProfile>,
    pub default_agent: String,
    pub sandbox: crate::sandbox::SandboxSettings,
    pub editor: EditorSettings,
}

/// How editor tabs behave.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    /// Vim keys; off means plain editing (arrows, Shift to select, Ctrl+Z/Y).
    pub vim: bool,
    /// Line numbers relative to the cursor line (the cursor line shows its own number).
    pub relative_numbers: bool,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            vim: true,
            relative_numbers: true,
        }
    }
}

const STRATA_MODEL: &str = "qwen3.8-flash-next-iq3_xxs";

impl Default for Config {
    fn default() -> Self {
        let thinking = match json!({"chat_template_kwargs": {"enable_thinking": true}}) {
            Value::Object(m) => m,
            _ => unreachable!(),
        };
        let local = |id: &str, name: &str, base_url: &str, model: &str| Profile {
            id: id.into(),
            name: name.into(),
            kind: BackendKind::OpenAi,
            base_url: base_url.into(),
            api_key: ApiKeySource::None,
            model: model.into(),
            context_limit: 32768,
            max_output: 8192,
            extra_body: Map::new(),
        };
        Self {
            active_profile: "strata".into(),
            base_system_prompt:
                "You are void.ai, a coding assistant working with the user in their editor. \
                Be direct and precise, show code when it helps, and say so when you are unsure."
                    .into(),
            theme: Theme::Dark,
            font_size: 15.0,
            profiles: vec![
                Profile {
                    id: "strata".into(),
                    name: "Strata (OpenAI)".into(),
                    kind: BackendKind::OpenAi,
                    base_url: "http://127.0.0.1:8080/v1".into(),
                    api_key: ApiKeySource::None,
                    model: STRATA_MODEL.into(),
                    context_limit: 262144,
                    max_output: 32768,
                    extra_body: thinking,
                },
                Profile {
                    id: "strata-anthropic".into(),
                    name: "Strata (Anthropic)".into(),
                    kind: BackendKind::Anthropic,
                    base_url: "http://127.0.0.1:8080".into(),
                    api_key: ApiKeySource::None,
                    model: STRATA_MODEL.into(),
                    context_limit: 262144,
                    max_output: 32768,
                    extra_body: Map::new(),
                },
                local(
                    "ollama",
                    "Ollama",
                    "http://127.0.0.1:11434/v1",
                    "gemma4:e4b",
                ),
                local(
                    "llama-server",
                    "llama-server",
                    "http://127.0.0.1:8081/v1",
                    "default",
                ),
            ],
            agents: crate::agent::default_agents(),
            default_agent: "build".into(),
            sandbox: crate::sandbox::SandboxSettings::default(),
            editor: EditorSettings::default(),
        }
    }
}

impl Config {
    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// The agent with this id, else the default agent, else the first one.
    pub fn agent(&self, id: Option<&str>) -> Option<&crate::agent::AgentProfile> {
        id.and_then(|i| self.agents.iter().find(|a| a.id == i))
            .or_else(|| self.agents.iter().find(|a| a.id == self.default_agent))
            .or_else(|| self.agents.first())
    }
}

/// Load the config. A missing file gives the defaults silently; an unreadable one is set aside
/// and the defaults are returned with a warning for the user.
pub fn load_config(path: &Path) -> (Config, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (Config::default(), None),
        Err(e) => {
            return (
                Config::default(),
                Some(format!(
                    "Couldn't read {}: {e}. Using defaults.",
                    path.display()
                )),
            );
        }
    };
    match toml::from_str(&text) {
        Ok(c) => (c, None),
        Err(e) => {
            let kept = match fsutil::set_aside(path) {
                Ok(bad) => format!("it was moved to {}", bad.display()),
                Err(e2) => format!("it couldn't be moved aside ({e2})"),
            };
            (
                Config::default(),
                Some(format!(
                    "{} isn't valid ({}); {kept}. Using defaults.",
                    path.display(),
                    e.message()
                )),
            )
        }
    }
}

pub fn save_config(path: &Path, config: &Config) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let text = toml::to_string_pretty(config).map_err(io::Error::other)?;
    fsutil::atomic_write(path, text.as_bytes())?;
    // The config can hold API keys: owner-only.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("the environment variable {0} isn't set")]
    Env(String),
    #[error("the API key command exited with status {status}: {stderr}")]
    Command { status: i32, stderr: String },
    #[error("the API key command printed nothing")]
    Empty,
    #[error("the API key command took longer than 10 s")]
    Timeout,
    #[error("couldn't run the API key command: {0}")]
    Spawn(String),
}

/// Resolve a key source to the key itself (None when the profile has no key).
pub async fn resolve_api_key(src: &ApiKeySource) -> Result<Option<String>, KeyError> {
    match src {
        ApiKeySource::None => Ok(None),
        ApiKeySource::Literal(k) => Ok(Some(k.clone())),
        ApiKeySource::Env(var) => std::env::var(var)
            .map(Some)
            .map_err(|_| KeyError::Env(var.clone())),
        ApiKeySource::Command(cmd) => {
            let child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output();
            let out = tokio::time::timeout(Duration::from_secs(10), child)
                .await
                .map_err(|_| KeyError::Timeout)?
                .map_err(|e| KeyError::Spawn(e.to_string()))?;
            if !out.status.success() {
                let stderr: String = String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .chars()
                    .take(500)
                    .collect();
                return Err(KeyError::Command {
                    status: out.status.code().unwrap_or(-1),
                    stderr,
                });
            }
            let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if key.is_empty() {
                Err(KeyError::Empty)
            } else {
                Ok(Some(key))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrips_through_toml() {
        let c = Config::default();
        let s = toml::to_string(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn defaults_seed_strata() {
        let c = Config::default();
        assert_eq!(c.active_profile, "strata");
        let p = c.profile("strata").unwrap();
        assert_eq!(p.kind, BackendKind::OpenAi);
        assert_eq!(p.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(p.model, "qwen3.8-flash-next-iq3_xxs");
        assert_eq!(p.context_limit, 262144);
        assert_eq!(p.max_output, 32768);
        // The daily preset serves this computer only and has no key.
        assert_eq!(p.api_key, ApiKeySource::None);
        assert_eq!(
            p.extra_body["chat_template_kwargs"]["enable_thinking"],
            true
        );
        let a = c.profile("strata-anthropic").unwrap();
        assert_eq!(a.kind, BackendKind::Anthropic);
        assert_eq!(a.base_url, "http://127.0.0.1:8080");
        let o = c.profile("ollama").unwrap();
        assert_eq!(o.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(o.model, "gemma4:e4b");
        let l = c.profile("llama-server").unwrap();
        assert_eq!(l.base_url, "http://127.0.0.1:8081/v1");
    }

    #[test]
    fn key_fields_serialize_as_optional_profile_fields() {
        let mut c = Config::default();
        c.profiles[0].api_key =
            ApiKeySource::Command("strata launch --show-key --preset daily".into());
        let s = toml::to_string(&c).unwrap();
        assert!(s.contains("api_key_cmd = \"strata launch --show-key --preset daily\""));
        let bad = r#"
            id = "x"
            name = "x"
            kind = "openai"
            base_url = "u"
            model = "m"
            context_limit = 1
            max_output = 1
            api_key = "a"
            api_key_env = "B"
        "#;
        assert!(toml::from_str::<Profile>(bad).is_err());
    }

    #[test]
    fn literal_key_is_redacted_in_debug() {
        let s = format!("{:?}", ApiKeySource::Literal("sk-secret".into()));
        assert!(!s.contains("sk-secret"));
    }

    #[tokio::test]
    async fn key_sources_resolve() {
        assert_eq!(
            resolve_api_key(&ApiKeySource::Literal("k".into()))
                .await
                .unwrap(),
            Some("k".into())
        );
        // SAFETY: test-only env var with a unique name.
        unsafe { std::env::set_var("VOID_AI_TEST_KEY_8731", "from-env") };
        assert_eq!(
            resolve_api_key(&ApiKeySource::Env("VOID_AI_TEST_KEY_8731".into()))
                .await
                .unwrap(),
            Some("from-env".into())
        );
        assert_eq!(
            resolve_api_key(&ApiKeySource::Command("printf ' abc \\n'".into()))
                .await
                .unwrap(),
            Some("abc".into())
        );
        assert!(matches!(
            resolve_api_key(&ApiKeySource::Command("exit 3".into())).await,
            Err(KeyError::Command { status: 3, .. })
        ));
        assert!(matches!(
            resolve_api_key(&ApiKeySource::Env("VOID_AI_TEST_MISSING_8731".into())).await,
            Err(KeyError::Env(_))
        ));
        assert_eq!(resolve_api_key(&ApiKeySource::None).await.unwrap(), None);
    }

    #[test]
    fn missing_config_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let (c, warning) = load_config(&dir.path().join("config.toml"));
        assert_eq!(c, Config::default());
        assert!(warning.is_none());
    }

    #[test]
    fn corrupt_config_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "not = [toml").unwrap();
        let (c, warning) = load_config(&p);
        assert_eq!(c, Config::default());
        assert!(warning.is_some());
        assert!(!p.exists());
        let bad: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("config.toml.bad-")
            })
            .collect();
        assert_eq!(bad.len(), 1);
        assert_eq!(std::fs::read_to_string(&bad[0]).unwrap(), "not = [toml");
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/config.toml");
        let c = Config {
            font_size: 18.0,
            ..Config::default()
        };
        save_config(&p, &c).unwrap();
        let (back, warning) = load_config(&p);
        assert!(warning.is_none());
        assert_eq!(back, c);
    }
}
