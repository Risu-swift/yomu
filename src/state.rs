//! Reading progress and the saved library, persisted as JSON.
//!
//! Deliberately not SQLite: the whole file is a few KB, it survives a crash by
//! being rewritten atomically, and it stays readable by hand.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::MangaRef;
use crate::net::config_dir;

/// How much chrome the reader shows around the page.
///
/// Every row spent on a bar is a row not spent on artwork, so this is worth
/// having under the reader's thumb rather than fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chrome {
    /// Title bar, a progress gauge, and a second line naming the chapter.
    Full,
    /// Title bar and a single combined line.
    Compact,
    /// Nothing but the page, except when something needs saying.
    Hidden,
}

impl Default for Chrome {
    fn default() -> Self {
        Self::Compact
    }
}

impl Chrome {
    pub fn next(self) -> Self {
        match self {
            Chrome::Full => Chrome::Compact,
            Chrome::Compact => Chrome::Hidden,
            Chrome::Hidden => Chrome::Full,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Chrome::Full => "full",
            Chrome::Compact => "compact",
            Chrome::Hidden => "hidden",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub chapter_id: String,
    pub chapter_number: String,
    /// Index of the page/slice that was on screen.
    pub page: usize,
    /// Pixel offset inside that slice, so a strip resumes mid-scroll.
    pub offset: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub library: Vec<MangaRef>,
    #[serde(default)]
    pub progress: BTreeMap<String, Progress>,
    /// Reopen on the source last used, rather than always starting on the
    /// first one — which may well be one this network cannot reach.
    #[serde(default)]
    pub last_source: Option<String>,
    /// Off unless deliberately turned on. Sources that expose a content rating
    /// filter it; ones that do not are unaffected and say so in settings.
    #[serde(default)]
    pub allow_nsfw: bool,
    /// Sources switched off in settings. Kept by name so the list survives
    /// plugins being added, removed or reordered.
    #[serde(default)]
    pub disabled_sources: Vec<String>,
    /// Crop uniform borders off pages. On by default: the margin is dead
    /// space and the page is scaled up to the same pane without it.
    #[serde(default = "yes")]
    pub trim_margins: bool,
    /// Percentage brightness for pages, 100 being untouched.
    #[serde(default = "full_brightness")]
    pub brightness: u8,
    /// How much chrome the reader draws. Remembered between sessions, since it
    /// is a reading preference rather than a per-session choice.
    #[serde(default)]
    pub chrome: Chrome,
}

fn yes() -> bool {
    true
}

fn full_brightness() -> u8 {
    100
}

fn path() -> PathBuf {
    config_dir().join("state.json")
}

impl State {
    pub fn load() -> Self {
        let path = path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };

        // Windows editors and PowerShell write UTF-8 with a byte order mark,
        // which serde_json rejects outright. Losing a library to an invisible
        // three-byte prefix is not a reasonable failure mode.
        let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(raw.as_str());

        match serde_json::from_str(trimmed) {
            Ok(state) => state,
            Err(_) => {
                // Keep whatever could not be parsed: otherwise the next save
                // silently overwrites a library that might be recoverable.
                let _ = std::fs::rename(&path, path.with_extension("json.bad"));
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let p = path();
        let tmp = p.with_extension("json.tmp");
        if let Ok(json) = serde_json::to_string_pretty(self) {
            // Write-then-rename so an interrupted save cannot truncate the state.
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }

    pub fn is_saved(&self, m: &MangaRef) -> bool {
        self.library.iter().any(|x| x.key() == m.key())
    }

    /// Returns true if the series is in the library after the call.
    pub fn toggle_library(&mut self, m: &MangaRef) -> bool {
        let key = m.key();
        if let Some(i) = self.library.iter().position(|x| x.key() == key) {
            self.library.remove(i);
            self.save();
            false
        } else {
            self.library.push(m.clone());
            self.save();
            true
        }
    }

    pub fn progress_of(&self, m: &MangaRef) -> Option<&Progress> {
        self.progress.get(&m.key())
    }

    pub fn record(&mut self, m: &MangaRef, p: Progress) {
        self.progress.insert(m.key(), p);
        self.save();
    }
}
