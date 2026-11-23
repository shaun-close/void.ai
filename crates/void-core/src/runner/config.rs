//! Run configs: groups of configs, each a chain of steps. Stored as `runs.toml`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fsutil;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    Moon {
        target: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Shell {
        command: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    #[default]
    Stop,
    Continue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepConfig {
    #[serde(flatten)]
    pub step: Step,
    #[serde(default)]
    pub on_failure: OnFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConfig {
    /// Stable unique id other code can reference (generated when a hand-edited file lacks one).
    #[serde(default = "new_id")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<StepConfig>,
    /// `None` = whichever profile is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_profile: Option<String>,
}

fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

impl RunConfig {
    pub fn new(name: impl Into<String>, steps: Vec<StepConfig>) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            steps,
            env_profile: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    #[serde(default)]
    pub configs: Vec<RunConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runs {
    #[serde(default)]
    pub groups: Vec<Group>,
}

impl Runs {
    /// Load `path`. Missing gives an empty set; a corrupt file is set aside and the warning
    /// returned (never printed). `Err` means the file exists but couldn't be read (permissions,
    /// not a file): callers must not `save` over it.
    pub fn load(path: &Path) -> Result<(Self, Option<String>), String> {
        load_toml(path, false)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        save_toml(self, path, false)
    }
}

/// `private`: the file holds secrets, so it's written 0600 and set aside 0600.
pub(crate) fn load_toml<T: Default + for<'de> Deserialize<'de>>(
    path: &Path,
    private: bool,
) -> Result<(T, Option<String>), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((T::default(), None)),
        Err(e) => return Err(format!("couldn't read {}: {e}", path.display())),
    };
    match toml::from_str(&text) {
        Ok(v) => Ok((v, None)),
        Err(e) => {
            let msg = match fsutil::set_aside(path) {
                Ok(bad) => {
                    if private {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o600));
                    }
                    format!(
                        "{} was unreadable ({}); kept as {} and started fresh",
                        path.display(),
                        e.message(),
                        bad.display()
                    )
                }
                Err(io) => format!(
                    "{} was unreadable ({}) and couldn't be set aside: {io}",
                    path.display(),
                    e.message()
                ),
            };
            Ok((T::default(), Some(msg)))
        }
    }
}

pub(crate) fn save_toml<T: Serialize>(v: &T, path: &Path, private: bool) -> std::io::Result<()> {
    let text = toml::to_string_pretty(v)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    if private {
        fsutil::atomic_write_private(path, text.as_bytes())
    } else {
        fsutil::atomic_write(path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_toml_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w/runs.toml");
        let mut cfg = RunConfig::new(
            "Check all",
            vec![
                StepConfig {
                    step: Step::Moon {
                        target: "workspace:lint".into(),
                        args: vec!["--force".into()],
                    },
                    on_failure: OnFailure::Continue,
                },
                StepConfig {
                    step: Step::Shell {
                        command: "echo done".into(),
                    },
                    on_failure: OnFailure::Stop,
                },
            ],
        );
        cfg.env_profile = Some("dev".into());
        let runs = Runs {
            groups: vec![Group {
                name: "CI".into(),
                configs: vec![cfg, RunConfig::new("Empty", vec![])],
            }],
        };
        runs.save(&path).unwrap();
        let (back, warn) = Runs::load(&path).unwrap();
        assert_eq!(warn, None);
        assert_eq!(back, runs);
        assert_ne!(back.groups[0].configs[0].id, back.groups[0].configs[1].id);
        assert_eq!(
            Runs::load(&dir.path().join("none.toml")).unwrap().0,
            Runs::default()
        );
    }

    #[test]
    fn corrupt_runs_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.toml");
        std::fs::write(&path, "groups = [[[ nope").unwrap();
        let (runs, warn) = Runs::load(&path).unwrap();
        assert!(runs.groups.is_empty());
        assert!(warn.unwrap().contains("runs.toml"));
        assert!(!path.exists());
        let kept: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".bad-"))
            .collect();
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn config_without_id_gets_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.toml");
        std::fs::write(
            &path,
            "[[groups]]\nname = \"g\"\n[[groups.configs]]\nname = \"hand made\"\n",
        )
        .unwrap();
        let (runs, warn) = Runs::load(&path).unwrap();
        assert_eq!(warn, None);
        assert!(path.exists());
        assert!(!runs.groups[0].configs[0].id.is_empty());
    }

    #[test]
    fn unreadable_file_is_an_error_not_defaults() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: reading fails, but it isn't corrupt TOML.
        let path = dir.path().join("runs.toml");
        std::fs::create_dir(&path).unwrap();
        assert!(Runs::load(&path).unwrap_err().contains("runs.toml"));
        assert!(path.exists());
    }
}
