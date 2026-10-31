//! Persisted widget preferences: refresh period, pin state and collapsed agents.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{sanitize_interval, DEFAULT_INTERVAL_SECS};
use crate::providers::creds::{first_non_empty_env, home_dir};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub interval_secs: u64,
    pub always_on_top: bool,
    #[serde(default)]
    pub collapsed_agents: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval_secs: DEFAULT_INTERVAL_SECS,
            always_on_top: false,
            collapsed_agents: BTreeSet::new(),
        }
    }
}

impl Settings {
    /// Applies the same floor the UI enforces, so a hand-edited file cannot make
    /// the widget poll faster than the menu allows.
    pub fn sanitized(mut self) -> Self {
        self.interval_secs = sanitize_interval(self.interval_secs);
        self
    }
}

/// Where the widget keeps its own preferences, following each platform's
/// convention: `%APPDATA%` on Windows, `$XDG_CONFIG_HOME` (or `~/.config`)
/// elsewhere.
pub fn settings_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        first_non_empty_env(&["APPDATA"])
            .or_else(|| Some(home_dir()?.join("AppData").join("Roaming")))?
    } else {
        first_non_empty_env(&["XDG_CONFIG_HOME"]).unwrap_or(home_dir()?.join(".config"))
    };
    Some(base.join("limitbar").join("settings.json"))
}

/// Never fails: a missing or unreadable file just means defaults.
pub fn load() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    load_from(&path)
}

pub fn load_from(path: &std::path::Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Settings>(&raw).ok())
        .unwrap_or_default()
        .sanitized()
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let path = settings_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no home directory to save into",
        )
    })?;
    save_to(&path, settings)
}

pub fn save_to(path: &std::path::Path, settings: &Settings) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&settings.clone().sanitized())?;
    std::fs::write(path, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_a_45_second_period_and_an_unpinned_window() {
        let s = Settings::default();
        assert_eq!(s.interval_secs, 45);
        assert!(!s.always_on_top);
        assert!(s.collapsed_agents.is_empty());
    }

    #[test]
    fn a_missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_from(&dir.path().join("absent.json")),
            Settings::default()
        );
    }

    #[test]
    fn a_corrupt_file_yields_defaults_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "}{ not json").unwrap();
        assert_eq!(load_from(&path), Settings::default());
    }

    #[test]
    fn a_hand_edited_period_is_pulled_back_onto_the_menu() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"intervalSecs":1,"alwaysOnTop":true}"#).unwrap();
        let loaded = load_from(&path);
        assert_eq!(
            loaded.interval_secs, 45,
            "polling faster than the floor is refused"
        );
        assert!(
            loaded.always_on_top,
            "the rest of the file is still honoured"
        );
    }

    #[test]
    fn older_settings_preserve_preferences_and_expand_all_agents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"intervalSecs":90,"alwaysOnTop":true}"#).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.interval_secs, 90);
        assert!(loaded.always_on_top);
        assert!(loaded.collapsed_agents.is_empty());
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");
        for interval_secs in crate::model::INTERVAL_CHOICES {
            let settings = Settings {
                interval_secs,
                always_on_top: true,
                collapsed_agents: BTreeSet::from(["codex".into(), "cursor".into()]),
            };
            save_to(&path, &settings).unwrap();
            assert_eq!(load_from(&path), settings);
        }
    }
}
