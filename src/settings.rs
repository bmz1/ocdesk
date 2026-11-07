//! Persisted preferences: a small JSON file in the platform config dir
//! (`~/.config/ocdesk/settings.json`, `~/Library/Application Support/ocdesk/settings.json`).

use std::collections::{BTreeMap, BTreeSet};
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    pub permission_mode: PermissionMode,
    pub project_defaults: BTreeMap<PathBuf, ProjectDefaults>,
    pub keybindings: BTreeMap<String, String>,
    /// Executable name/path, without shell arguments.
    pub editor: Option<String>,
    pub notifications: bool,
    pub unread: BTreeSet<String>,
    /// Scope/file -> fingerprint of the reviewed patch.
    pub viewed: BTreeMap<String, u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            collapsed: Vec::new(),
            theme: None,
            window: None,
            sidebar_width: None,
            sidebar_hidden: false,
            last_session: None,
            draft_model: None,
            draft_agent: None,
            permission_mode: PermissionMode::Supervised,
            project_defaults: BTreeMap::new(),
            keybindings: BTreeMap::new(),
            editor: None,
            notifications: true,
            unread: BTreeSet::new(),
            viewed: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    #[default]
    Supervised,
    AutoEdits,
    FullAccess,
}
impl PermissionMode {
    pub const ALL: [Self; 3] = [Self::Supervised, Self::AutoEdits, Self::FullAccess];
    pub fn label(self) -> &'static str {
        match self {
            Self::Supervised => "Supervised",
            Self::AutoEdits => "Auto-accept edits",
            Self::FullAccess => "Full access",
        }
    }
    pub fn rules(self) -> serde_json::Value {
        use serde_json::json;
        if self == Self::FullAccess {
            return json!([{"action":"*","resource":"*","effect":"allow"}]);
        }
        let mut rules = vec![json!({"action":"*","resource":"*","effect":"ask"})];
        for action in ["read", "glob", "grep"] {
            rules.push(json!({"action":action,"resource":"*","effect":"allow"}));
        }
        if self == Self::AutoEdits {
            rules.push(json!({"action":"edit","resource":"*","effect":"allow"}));
        }
        rules.into()
    }
    pub fn from_rules(v: &serde_json::Value) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.rules() == *v)
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectDefaults {
    pub model: Option<ModelRef>,
    pub agent: Option<String>,
    pub permission: Option<PermissionMode>,
}
impl Settings {
    pub fn permission_for(&self, folder: &std::path::Path) -> PermissionMode {
        self.project_defaults
            .get(folder)
            .and_then(|p| p.permission)
            .unwrap_or(self.permission_mode)
    }
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
        let s: Settings =
            serde_json::from_str(r#"{"folders":["/a"],"collapsed":[],"theme":null}"#).unwrap();
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
        let ok = WindowPlacement {
            x: 0.,
            y: 0.,
            width: 800.,
            height: 600.,
            maximized: false,
        };
        assert!(ok.is_sane());
        assert!(!WindowPlacement { width: 10., ..ok }.is_sane());
        assert!(!WindowPlacement { x: f32::NAN, ..ok }.is_sane());
    }
}

#[cfg(test)]
mod desktop_feature_tests {
    use super::*;
    #[test]
    fn project_defaults_override_only_explicit_fields() {
        let mut settings = Settings::default();
        let folder = PathBuf::from("/project");
        settings.permission_mode = PermissionMode::FullAccess;
        assert_eq!(settings.permission_for(&folder), PermissionMode::FullAccess);
        settings.project_defaults.insert(
            folder.clone(),
            ProjectDefaults {
                permission: Some(PermissionMode::Supervised),
                ..Default::default()
            },
        );
        assert_eq!(settings.permission_for(&folder), PermissionMode::Supervised);
        let back: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(settings, back);
        assert_eq!(PermissionMode::from_rules(&serde_json::json!([])), None);
        for mode in PermissionMode::ALL {
            assert_eq!(PermissionMode::from_rules(&mode.rules()), Some(mode));
        }
    }
}
