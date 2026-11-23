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
    /// Stable unique id other code can reference.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<StepConfig>,
    /// `None` = whichever profile is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_profile: Option<String>,
}

impl RunConfig {
    pub fn new(name: impl Into<String>, steps: Vec<StepConfig>) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
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
    /// Load `path`. Missing gives an empty set; an unreadable file is set aside and the
    /// warning returned (never printed).
    pub fn load(path: &Path) -> (Self, Option<String>) {
        load_toml(path)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        save_toml(self, path)
    }
}

pub(crate) fn load_toml<T: Default + for<'de> Deserialize<'de>>(
    path: &Path,
) -> (T, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (T::default(), None),
        Err(e) => {
            return (
                T::default(),
                Some(format!("couldn't read {}: {e}", path.display())),
            );
        }
    };
    match toml::from_str(&text) {
        Ok(v) => (v, None),
        Err(e) => {
            let msg = match fsutil::set_aside(path) {
                Ok(bad) => format!(
                    "{} was unreadable ({}); kept as {} and started fresh",
                    path.display(),
                    e.message(),
                    bad.display()
                ),
                Err(io) => format!(
                    "{} was unreadable ({}) and couldn't be set aside: {io}",
                    path.display(),
                    e.message()
                ),
            };
            (T::default(), Some(msg))
        }
    }
}

pub(crate) fn save_toml<T: Serialize>(v: &T, path: &Path) -> std::io::Result<()> {
    let text = toml::to_string_pretty(v)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    fsutil::atomic_write(path, text.as_bytes())
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
        let (back, warn) = Runs::load(&path);
        assert_eq!(warn, None);
        assert_eq!(back, runs);
        assert_ne!(back.groups[0].configs[0].id, back.groups[0].configs[1].id);
        assert_eq!(Runs::load(&dir.path().join("none.toml")).0, Runs::default());
    }

    #[test]
    fn corrupt_runs_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.toml");
        std::fs::write(&path, "groups = [[[ nope").unwrap();
        let (runs, warn) = Runs::load(&path);
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
}
