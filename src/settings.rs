//! Persisted preferences: a small JSON file in the platform config dir
//! (`~/.config/ocdesk/settings.json`, `~/Library/Application Support/ocdesk/settings.json`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Folders shown in the sidebar, in order.
    pub folders: Vec<PathBuf>,
    pub collapsed: Vec<PathBuf>,
    /// "dark" (default), "light" or "system".
    pub theme: Option<String>,
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("ocdesk").join("settings.json"))
}

impl Settings {
    /// Never fails: a missing or broken file yields defaults (logged).
    pub fn load() -> Self {
        let Some(p) = path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", p.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Blocking write (atomic via rename); call off the UI thread.
    pub fn save(&self) -> anyhow::Result<()> {
        let p = path().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        std::fs::create_dir_all(p.parent().expect("has parent"))?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }
}
