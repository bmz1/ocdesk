//! Persisted preferences: a small JSON file in the platform config dir
//! (`~/.config/ocdesk/settings.json`, `~/Library/Application Support/ocdesk/settings.json`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    pub keybindings: BTreeMap<String, String>,
    /// Executable name/path, without shell arguments.
    pub editor: Option<String>,
    pub notifications: bool,
    pub unread: BTreeSet<String>,
    /// Scope/file -> fingerprint of the reviewed patch.
    pub viewed: BTreeMap<String, u64>,
    /// How much of each turn's work the transcript shows.
    pub density: Density,
    /// Sessions kept at the top of their folder, most recently pinned first.
    pub pinned: Vec<String>,
    /// Right-hand panel: open tab (`None`: closed) and width.
    pub panel: Option<PanelTab>,
    pub panel_width: Option<f32>,
    /// Editor tabs open per folder (paths relative to it), in tab order.
    pub open_files: BTreeMap<PathBuf, Vec<String>>,
}

/// Transcript density, cycled with Ctrl+O.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Density {
    /// Work logs collapsed into one row per run; reasoning collapsed.
    #[default]
    Normal,
    /// Work logs and reasoning expanded.
    Verbose,
    /// Successful tool calls and reasoning hidden; running, failed,
    /// approvals and answers stay.
    Quiet,
}

impl Density {
    pub const ALL: [Self; 3] = [Self::Normal, Self::Verbose, Self::Quiet];
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Verbose => "Verbose",
            Self::Quiet => "Hide successful work",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Normal => Self::Verbose,
            Self::Verbose => Self::Quiet,
            Self::Quiet => Self::Normal,
        }
    }
}

/// Tabs of the right-hand panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelTab {
    Changes,
    Tasks,
    Terminal,
    Files,
    Agents,
    PullRequest,
    Browser,
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
            keybindings: BTreeMap::new(),
            editor: None,
            notifications: true,
            unread: BTreeSet::new(),
            viewed: BTreeMap::new(),
            density: Density::Normal,
            pinned: Vec::new(),
            panel: None,
            panel_width: None,
            open_files: BTreeMap::new(),
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
fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("ocdesk").join("settings.json"))
}

/// Order of saves: taken on the UI thread when a save is requested, so a
/// later request always carries a higher number.
static NEXT_SAVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
/// Highest save number written; also serializes writers.
static WRITTEN: std::sync::Mutex<u64> = std::sync::Mutex::new(0);

/// A snapshot of the settings waiting to be written, numbered in request
/// order. Writes are serialized and a snapshot older than one already on
/// disk is dropped, so overlapping background saves (and the final save on
/// quit) can't interleave or put older settings back.
pub struct PendingSave {
    seq: u64,
    settings: Settings,
}

impl PendingSave {
    /// Blocking; call off the UI thread (except the final save on quit).
    pub fn write(self) -> anyhow::Result<()> {
        let p = path().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        self.write_to(&p)
    }

    fn write_to(self, p: &std::path::Path) -> anyhow::Result<()> {
        let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        if self.seq <= *written {
            return Ok(());
        }
        std::fs::create_dir_all(p.parent().expect("has parent"))?;
        // Unique per write, so a crashed or concurrent writer's temp file
        // is never renamed over the real one half-written.
        let tmp = p.with_extension(format!("json.{}.{}.tmp", std::process::id(), self.seq));
        let result = (|| {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(&self.settings)?)?;
            f.sync_all()?;
            std::fs::rename(&tmp, p)?;
            anyhow::Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        } else {
            *written = self.seq;
        }
        result
    }
}

impl Settings {
    /// Never fails: a missing or broken file yields defaults (logged).
    pub fn load() -> Self {
        let Some(p) = path() else {
            return Self::default();
        };
        Self::load_from(&p)
    }

    fn load_from(p: &std::path::Path) -> Self {
        match std::fs::read_to_string(p) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                // Keep the unreadable file for the user instead of letting
                // the next save replace it with defaults.
                let aside = p.with_extension("json.unreadable");
                tracing::warn!(
                    "ignoring unreadable {} ({e}); kept as {}",
                    p.display(),
                    aside.display()
                );
                let _ = std::fs::rename(p, aside);
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Snapshot these settings for writing; take it on the UI thread, then
    /// [`PendingSave::write`] it anywhere.
    pub fn snapshot_for_save(&self) -> PendingSave {
        PendingSave {
            seq: NEXT_SAVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            settings: self.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_are_ordered_and_stale_ones_dropped() {
        let dir = std::env::temp_dir().join(format!("ocdesk-settings-{}", std::process::id()));
        let p = dir.join("settings.json");
        let with = |f: &str| Settings {
            folders: vec![PathBuf::from(f)],
            ..Default::default()
        };
        let older = with("/old").snapshot_for_save();
        let newer = with("/new").snapshot_for_save();
        // The newer save lands first; the older one must not overwrite it.
        newer.write_to(&p).unwrap();
        older.write_to(&p).unwrap();
        assert_eq!(Settings::load_from(&p).folders, [PathBuf::from("/new")]);
        // No temp files left behind.
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
        // A broken file is kept aside, not silently replaced.
        std::fs::write(&p, "{not json").unwrap();
        assert!(Settings::load_from(&p).folders.is_empty());
        assert!(dir.join("settings.json.unreadable").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_files_still_load() {
        // A file written before M5 only had folders/collapsed/theme.
        let s: Settings =
            serde_json::from_str(r#"{"folders":["/a"],"collapsed":[],"theme":null}"#).unwrap();
        assert_eq!(s.folders, vec![PathBuf::from("/a")]);
        assert!(s.window.is_none() && !s.sidebar_hidden);
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
    fn legacy_defaults_load_but_are_no_longer_saved_or_applied() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "theme":"light", "draft_model":{"providerID":"p","id":"m"},
            "draft_agent":"plan", "permission_mode":"full-access",
            "project_defaults":{"/project":{"agent":"build"}}
        }))
        .unwrap();
        assert_eq!(settings.theme.as_deref(), Some("light"));
        let saved = serde_json::to_value(settings).unwrap();
        for key in [
            "draft_model",
            "draft_agent",
            "permission_mode",
            "project_defaults",
        ] {
            assert!(saved.get(key).is_none());
        }
        assert_eq!(PermissionMode::from_rules(&serde_json::json!([])), None);
        for mode in PermissionMode::ALL {
            assert_eq!(PermissionMode::from_rules(&mode.rules()), Some(mode));
        }
    }
}
