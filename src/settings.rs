//! Persisted preferences: a small JSON file in the platform config dir
//! (`~/.config/ocdesk/settings.json`, `~/Library/Application Support/ocdesk/settings.json`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::model::ModelRef;

/// Last window placement, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

impl WindowPlacement {
    /// Smallest size restored; anything below is treated as broken.
    pub const MIN_W: f32 = 640.;
    pub const MIN_H: f32 = 420.;

    pub fn is_sane(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.width >= Self::MIN_W
            && self.height >= Self::MIN_H
            && self.width <= 20_000.
            && self.height <= 20_000.
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Folders shown in the sidebar, in order.
    pub folders: Vec<PathBuf>,
    pub collapsed: Vec<PathBuf>,
    /// "dark" (default), "light" or "system".
    pub theme: Option<String>,
    pub window: Option<WindowPlacement>,
    /// Sidebar width in logical pixels (`None`: default).
    pub sidebar_width: Option<f32>,
    pub sidebar_hidden: bool,
    /// Session open when ocdesk last quit; reopened on start if it still exists.
    pub last_session: Option<String>,
    /// Model/agent picked for new sessions.
    pub draft_model: Option<ModelRef>,
    pub draft_agent: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_files_still_load() {
        // A file written before M5 only had folders/collapsed/theme.
        let s: Settings = serde_json::from_str(r#"{"folders":["/a"],"collapsed":[],"theme":null}"#)
            .unwrap();
        assert_eq!(s.folders, vec![PathBuf::from("/a")]);
        assert!(s.window.is_none() && !s.sidebar_hidden && s.draft_model.is_none());
    }

    #[test]
    fn round_trip() {
        let s = Settings {
            window: Some(WindowPlacement {
                x: 10.,
                y: 20.,
                width: 1000.,
                height: 700.,
                maximized: true,
            }),
            sidebar_width: Some(300.),
            sidebar_hidden: true,
            last_session: Some("ses_1".into()),
            draft_model: Some(ModelRef {
                provider_id: "p".into(),
                id: "m".into(),
                variant: Some("high".into()),
            }),
            draft_agent: Some("plan".into()),
            ..Default::default()
        };
        let back: Settings = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn placement_sanity() {
        let ok = WindowPlacement { x: 0., y: 0., width: 800., height: 600., maximized: false };
        assert!(ok.is_sane());
        assert!(!WindowPlacement { width: 10., ..ok }.is_sane());
        assert!(!WindowPlacement { x: f32::NAN, ..ok }.is_sane());
    }
}
