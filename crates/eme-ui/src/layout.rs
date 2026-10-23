//! Layout persistence: `benilla-config/eme-layout.toml` (the path is injected by the host).

use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the layout file lives. `None` = do not persist.
#[derive(Resource, Debug, Clone, Default)]
pub struct EmeLayoutPath(pub Option<PathBuf>);

/// Which right-hand tab is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeRightTab {
    /// Undo stack entries.
    #[default]
    History,
    /// Restore points.
    Restore,
    /// Audit log.
    Log,
}

/// The persisted layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmeLayout {
    /// Left (tool options) panel shown.
    pub left_open: bool,
    /// Right (history / restore / log) panel shown.
    pub right_open: bool,
    /// Left panel width, points.
    pub left_width: f32,
    /// Right panel width, points.
    pub right_width: f32,
    /// Open right tab.
    pub right_tab: EmeRightTab,
}

impl Default for EmeLayout {
    fn default() -> Self {
        Self {
            left_open: true,
            right_open: true,
            left_width: 230.0,
            right_width: 270.0,
            right_tab: EmeRightTab::History,
        }
    }
}

/// Smallest panel width, points.
pub const PANEL_MIN: f32 = 160.0;
/// Largest panel width, points.
pub const PANEL_MAX: f32 = 640.0;

impl EmeLayout {
    /// Clamps hand-edited / stale values into usable ranges.
    pub fn sanitized(mut self) -> Self {
        let fix = |w: f32, d: f32| {
            if w.is_finite() {
                w.clamp(PANEL_MIN, PANEL_MAX)
            } else {
                d
            }
        };
        self.left_width = fix(self.left_width, 230.0);
        self.right_width = fix(self.right_width, 270.0);
        self
    }

    /// TOML text.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("layout serializes")
    }

    /// Parses TOML text (bad text is an error; absent keys take defaults).
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str::<EmeLayout>(text).map(Self::sanitized)
    }

    /// Loads `path`; a missing or broken file gives the default layout.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| Self::from_toml(&t).ok())
            .unwrap_or_default()
    }

    /// Writes `path`, creating its folder.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_toml())
    }
}
