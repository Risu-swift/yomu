//! Built-in source backed by the public MangaDex API.
//!
//! MangaDex documents and permits third-party clients, so it is the one source
//! that ships in the box. Everything else is a user-supplied TOML plugin.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::Source;
use crate::model::{Chapter, Chapters, Direction, MangaRef, Page};
use crate::net::client;

const API: &str = "https://api.mangadex.org";
const COVERS: &str = "https://uploads.mangadex.org/covers";

pub struct MangaDex {
    lang: String,
    /// Interior mutability so the setting can be flipped while the registry
    /// hands out shared references to the source.
    allow_nsfw: AtomicBool,
}

impl MangaDex {
    pub fn new() -> Self {
        Self {
            lang: "en".into(),
            allow_nsfw: AtomicBool::new(false),
        }
    }

    /// The `contentRating[]` values to request.
    ///
    /// Safe and suggestive always; the adult ratings only when asked for.
    fn ratings(&self) -> &'static [&'static str] {
        if self.allow_nsfw.load(Ordering::Relaxed) {
            &["safe", "suggestive", "erotica", "pornographic"]
        } else {
            &["safe", "suggestive"]
        }
    }

    /// Total chapters in every language. Used only to explain an empty list,
    /// so it asks for a single row and reads the count off the envelope.
    async fn total_chapters_all_languages(&self, manga_id: &str) -> Result<usize> {
        let body: Value = client()
            .get(format!("{API}/manga/{manga_id}/feed"))
            .query(&[("limit", "1")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(body["total"].as_u64().unwrap_or(0) as usize)
    }
}

/// Pick a localized string out of the `{lang: text}` maps the API returns.
fn localized(v: &Value, lang: &str) -> String {
    let Some(obj) = v.as_object() else {
        return String::new();
    };
    obj.get(lang)
        .or_else(|| obj.get("en"))
        .or_else(|| obj.values().next())
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

#[async_trait::async_trait]
impl Source for MangaDex {
    fn name(&self) -> &str {
        "MangaDex"
    }

    fn set_allow_nsfw(&self, allow: bool) {
        self.allow_nsfw.store(allow, Ordering::Relaxed);
    }

    fn supports_nsfw_filter(&self) -> bool {
        true
    }

    async fn search(&self, query: &str) -> Result<Vec<MangaRef>> {
        let mut params: Vec<(&str, &str)> = vec![("limit", "30"), ("includes[]", "cover_art")];
        params.extend(self.ratings().iter().map(|r| ("contentRating[]", *r)));
        if query.trim().is_empty() {
            params.push(("order[followedCount]", "desc"));
        } else {
            params.push(("title", query));
            params.push(("order[relevance]", "desc"));
        }

        let body: Value = client()
            .get(format!("{API}/manga"))
            .query(&params)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let data = body["data"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::with_capacity(data.len());

        for m in data {
            let id = m["id"].as_str().unwrap_or_default().to_string();
            if id.is_empty() {
                continue;
            }
            let attrs = &m["attributes"];

            // "Long Strip" is the tag marking webtoon-format releases.
            let vertical = attrs["tags"]
                .as_array()
                .map(|tags| {
                    tags.iter().any(|t| {
                        localized(&t["attributes"]["name"], "en").eq_ignore_ascii_case("long strip")
                    })
                })
                .unwrap_or(false);

            let cover = m["relationships"].as_array().and_then(|rels| {
                rels.iter()
                    .find(|r| r["type"] == "cover_art")
                    .and_then(|r| r["attributes"]["fileName"].as_str())
                    // 512 rather than 256: the preview pane upscales the cover,
                    // and the smaller thumbnail visibly softens at that size.
                    .map(|f| format!("{COVERS}/{id}/{f}.512.jpg"))
            });

            out.push(MangaRef {
                source: self.name().into(),
                id,
                title: localized(&attrs["title"], &self.lang),
                cover,
                description: localized(&attrs["description"], &self.lang),
                status: attrs["status"].as_str().unwrap_or_default().to_string(),
                direction: if vertical {
                    Direction::Vertical
                } else {
                    Direction::RightToLeft
                },
            });
        }

        Ok(out)
    }

    async fn chapters(&self, manga: &MangaRef) -> Result<Chapters> {
        let mut out: Vec<Chapter> = Vec::new();
        let mut external = 0usize;
        let mut in_lang = 0usize;
        let mut offset = 0usize;

        // The feed endpoint caps at 500 per call; walk it until we have them all.
        loop {
            let offset_s = offset.to_string();
            let mut params: Vec<(&str, &str)> = vec![
                ("limit", "500"),
                ("offset", &offset_s),
                ("translatedLanguage[]", &self.lang),
                ("order[chapter]", "asc"),
            ];
            params.extend(self.ratings().iter().map(|r| ("contentRating[]", *r)));

            let body: Value = client()
                .get(format!("{API}/manga/{}/feed", manga.id))
                .query(&params)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let data = body["data"].as_array().cloned().unwrap_or_default();
            let got = data.len();

            for c in data {
                let attrs = &c["attributes"];
                in_lang += 1;
                // Licensed series list chapters that live on an official
                // reader: an external link and pages = 0. Nothing to fetch.
                if attrs["externalUrl"].is_string() || attrs["pages"].as_u64() == Some(0) {
                    external += 1;
                    continue;
                }
                out.push(Chapter {
                    id: c["id"].as_str().unwrap_or_default().to_string(),
                    number: attrs["chapter"].as_str().unwrap_or("?").to_string(),
                    title: attrs["title"].as_str().unwrap_or_default().to_string(),
                    lang: attrs["translatedLanguage"].as_str().unwrap_or_default().to_string(),
                });
            }

            offset += got;
            let total = body["total"].as_u64().unwrap_or(0) as usize;
            if got == 0 || offset >= total {
                break;
            }
        }

        // The same number often appears from several scanlation groups; keep one.
        out.dedup_by(|a, b| a.number == b.number && a.number != "?");

        // When nothing is readable, one cheap unfiltered call tells us whether
        // the language filter is what emptied the list.
        let other_languages = if out.is_empty() {
            self.total_chapters_all_languages(&manga.id)
                .await
                .unwrap_or(0)
                .saturating_sub(in_lang)
        } else {
            0
        };

        Ok(Chapters { items: out, external, other_languages })
    }

    async fn pages(&self, _manga: &MangaRef, chapter: &Chapter) -> Result<Vec<Page>> {
        let body: Value = client()
            .get(format!("{API}/at-home/server/{}", chapter.id))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let base = body["baseUrl"]
            .as_str()
            .ok_or_else(|| anyhow!("no baseUrl in at-home response"))?;
        let hash = body["chapter"]["hash"]
            .as_str()
            .ok_or_else(|| anyhow!("no chapter hash"))?;
        let files = body["chapter"]["data"]
            .as_array()
            .ok_or_else(|| anyhow!("no page list"))?;

        Ok(files
            .iter()
            .filter_map(|f| f.as_str())
            .map(|f| Page::new(format!("{base}/data/{hash}/{f}")))
            .collect())
    }
}
