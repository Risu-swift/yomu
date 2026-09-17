pub mod declarative;
pub mod mangadex;

use std::sync::Arc;

use anyhow::Result;

use crate::model::{Chapter, Chapters, MangaRef, Page};
use crate::net::config_dir;

#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &str;

    /// Allow or exclude adult material.
    ///
    /// A no-op by default: a scraped site exposes no rating to filter on, so
    /// only sources that actually carry one need to implement this.
    fn set_allow_nsfw(&self, _allow: bool) {}

    /// Whether the content filter means anything here.
    ///
    /// Settings shows this so a source that cannot filter says so, instead of
    /// letting the toggle imply a guarantee it is not making.
    fn supports_nsfw_filter(&self) -> bool {
        false
    }

    async fn search(&self, query: &str) -> Result<Vec<MangaRef>>;
    /// Readable chapters, plus counts of what was excluded and why.
    async fn chapters(&self, manga: &MangaRef) -> Result<Chapters>;
    async fn pages(&self, manga: &MangaRef, chapter: &Chapter) -> Result<Vec<Page>>;
}

pub struct Registry {
    pub sources: Vec<Arc<dyn Source>>,
    /// Plugins that failed to load, surfaced in the UI instead of swallowed.
    pub errors: Vec<String>,
}

impl Registry {
    /// Built-in sources first, then every `*.toml` plugin in the config dir.
    pub fn load() -> Self {
        let mut sources: Vec<Arc<dyn Source>> = vec![Arc::new(mangadex::MangaDex::new())];
        let mut errors = Vec::new();

        let dir = config_dir().join("sources");
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut files: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "toml"))
                .collect();
            files.sort();

            for path in files {
                match declarative::DeclarativeSource::from_file(&path) {
                    Ok(src) => sources.push(Arc::new(src)),
                    Err(e) => errors.push(format!(
                        "{}: {e:#}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )),
                }
            }
        }

        Self { sources, errors }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Source>> {
        self.sources.iter().find(|s| s.name() == name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.sources.iter().map(|s| s.name().to_string()).collect()
    }

    /// Apply the content filter to every source at once.
    pub fn set_allow_nsfw(&self, allow: bool) {
        for source in &self.sources {
            source.set_allow_nsfw(allow);
        }
    }
}
