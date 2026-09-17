use serde::{Deserialize, Serialize};

/// How a series is meant to be read. Drives the reader's default view mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Japanese manga: pages advance right-to-left.
    RightToLeft,
    /// Western / translated releases that read left-to-right.
    LeftToRight,
    /// Korean/Chinese webtoons: one continuous vertical strip.
    Vertical,
}

impl Direction {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "vertical" | "strip" | "webtoon" | "manhwa" => Direction::Vertical,
            "ltr" | "left-to-right" => Direction::LeftToRight,
            _ => Direction::RightToLeft,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Direction::RightToLeft => "manga · RTL",
            Direction::LeftToRight => "comic · LTR",
            Direction::Vertical => "webtoon · strip",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MangaRef {
    pub source: String,
    pub id: String,
    pub title: String,
    pub cover: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: String,
    #[serde(default = "default_dir")]
    pub direction: Direction,
}

fn default_dir() -> Direction {
    Direction::RightToLeft
}

impl MangaRef {
    /// Stable identity used as the key for reading progress.
    pub fn key(&self) -> String {
        format!("{}:{}", self.source, self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chapter {
    pub id: String,
    /// Display number as the source reports it ("12", "12.5", "Extra").
    pub number: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub lang: String,
}

impl Chapter {
    pub fn label(&self) -> String {
        // Plenty of sites give a chapter one combined heading with no separate
        // number in the markup. Prefixing "Ch. ?" to it reads worse than just
        // showing what the site said.
        match (self.number.as_str(), self.title.as_str()) {
            ("?", title) if !title.is_empty() => title.to_string(),
            (number, "") => format!("Ch. {number}"),
            (number, title) => format!("Ch. {number} — {title}"),
        }
    }
}

/// The result of a chapter lookup.
///
/// A source can legitimately return nothing readable: licensed series often
/// list chapters that live on an official reader with no images to fetch, and
/// a language filter can exclude everything else. Carrying those counts is what
/// lets the UI say *why* a series is empty instead of just showing "0 chapters".
#[derive(Debug, Clone, Default)]
pub struct Chapters {
    pub items: Vec<Chapter>,
    /// Exist, but are hosted elsewhere and have no pages to fetch here.
    pub external: usize,
    /// Excluded because they are in another language.
    pub other_languages: usize,
}

impl Chapters {
    pub fn new(items: Vec<Chapter>) -> Self {
        Self { items, ..Default::default() }
    }

    /// Why the list is empty, in words, or None when it is not empty.
    pub fn explain_empty(&self) -> Option<String> {
        if !self.items.is_empty() {
            return None;
        }
        Some(match (self.external, self.other_languages) {
            (0, 0) => "no chapters on this source".into(),
            (e, 0) => format!("{e} chapters, all on official readers — none readable here"),
            (0, o) => format!("{o} chapters, none translated into your language"),
            (e, o) => format!("{e} hosted externally, {o} in other languages — none readable"),
        })
    }
}

/// A single image to fetch. Some sources hotlink-protect, hence per-page headers.
#[derive(Debug, Clone)]
pub struct Page {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl Page {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), headers: Vec::new() }
    }
}
