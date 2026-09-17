//! Declarative scraper plugins.
//!
//! A source is described by a TOML file dropped in `<config>/sources/`. Most
//! sites are nothing more than a URL template plus a handful of CSS selectors,
//! so no scripting is needed for them. A selector is written as `sel`, meaning
//! the text of the first match, or `sel@attr` for one of its attributes; a bare
//! `@attr` reads the attribute off the matched item itself.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;

use super::Source;
use crate::model::{Chapter, Chapters, Direction, MangaRef, Page};
use crate::net::get_text;

#[derive(Debug, Deserialize)]
pub struct Spec {
    pub name: String,
    pub base: String,
    #[serde(default)]
    pub direction: Option<String>,
    /// Sent with every request. Hotlink-protected sites usually want a Referer.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub search: SearchSpec,
    pub chapters: ChaptersSpec,
    pub pages: PagesSpec,
    /// How this site expresses a content filter, if it has one at all.
    #[serde(default)]
    pub nsfw: Option<NsfwSpec>,
}

/// Substituted for `{nsfw}` anywhere in a URL template.
///
/// Sites spell this too differently for a fixed parameter name, so the plugin
/// supplies both forms and yomu picks one. A plugin without this section has no
/// filter at all, and the settings screen says so rather than implying the
/// toggle does something.
#[derive(Debug, Deserialize)]
pub struct NsfwSpec {
    /// Used when adult content is allowed, e.g. "&rating=all".
    #[serde(default)]
    pub on: String,
    /// Used otherwise, e.g. "&rating=safe".
    #[serde(default)]
    pub off: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchSpec {
    /// Path or absolute URL. `{query}` is substituted, url-encoded.
    pub url: String,
    /// Selector for one result row.
    pub item: String,
    pub title: String,
    pub link: String,
    pub cover: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChaptersSpec {
    /// Optional template with `{manga}`; defaults to the series URL itself.
    pub url: Option<String>,
    pub item: String,
    pub link: String,
    pub number: Option<String>,
    pub title: Option<String>,
    /// Sites usually list newest first; flip to read in publication order.
    #[serde(default = "yes")]
    pub reverse: bool,
}

#[derive(Debug, Deserialize)]
pub struct PagesSpec {
    /// Optional template with `{chapter}`; defaults to the chapter URL.
    pub url: Option<String>,
    /// Selector for the page images. Omit it when `regex` is given instead.
    pub item: Option<String>,
    /// Alternative to `item`, for sites that hand the page list to JavaScript
    /// rather than putting `<img>` elements in the document: a regex run over
    /// the raw HTML. Capture group 1 holds the URLs, or the whole match if the
    /// pattern has no group.
    pub regex: Option<String>,
    /// Separator that splits one capture into several URLs, for the common
    /// `var pages = "a.jpg,b.jpg"` shape. Without it a capture is one URL and
    /// every match contributes one page.
    pub split: Option<String>,
}

fn yes() -> bool {
    true
}

pub struct DeclarativeSource {
    spec: Spec,
    headers: Vec<(String, String)>,
    /// Compiled once at load time; `None` when `pages.item` is in use.
    pages_re: Option<Regex>,
    /// Interior mutability: the registry hands out shared references, so the
    /// setting has to be flippable through one.
    allow_nsfw: AtomicBool,
}

impl DeclarativeSource {
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).context("reading plugin")?;
        let spec: Spec = toml::from_str(&raw).context("parsing plugin")?;
        Self::from_spec(spec)
    }

    pub fn from_spec(spec: Spec) -> Result<Self> {
        // Fail at load time rather than mid-search on a typo'd selector.
        for s in [
            Some(&spec.search.item),
            Some(&spec.search.title),
            Some(&spec.search.link),
            spec.search.cover.as_ref(),
            spec.search.description.as_ref(),
            Some(&spec.chapters.item),
            Some(&spec.chapters.link),
            spec.chapters.number.as_ref(),
            spec.chapters.title.as_ref(),
            spec.pages.item.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            validate(s)?;
        }

        // Exactly one way of finding pages, decided here rather than per chapter.
        let pages_re = match (&spec.pages.item, &spec.pages.regex) {
            (Some(_), None) => None,
            (None, Some(p)) => {
                Some(Regex::new(p).map_err(|e| anyhow!("bad pages.regex `{p}`: {e}"))?)
            }
            (Some(_), Some(_)) => bail!("pages: set either `item` or `regex`, not both"),
            (None, None) => bail!("pages: needs `item` or `regex`"),
        };

        let headers = spec.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        Ok(Self {
            spec,
            headers,
            pages_re,
            allow_nsfw: AtomicBool::new(false),
        })
    }

    fn direction(&self) -> Direction {
        self.spec
            .direction
            .as_deref()
            .map(Direction::parse)
            .unwrap_or(Direction::RightToLeft)
    }

    fn url(&self, template: &str, key: &str, value: &str) -> String {
        // A plugin with no [nsfw] section drops `{nsfw}` to nothing, so the
        // placeholder is harmless to leave in a template either way.
        let nsfw = self
            .spec
            .nsfw
            .as_ref()
            .map(|n| {
                if self.allow_nsfw.load(Ordering::Relaxed) {
                    n.on.as_str()
                } else {
                    n.off.as_str()
                }
            })
            .unwrap_or("");

        let filled = template.replace(key, value).replace("{nsfw}", nsfw);
        absolute(&self.spec.base, &filled)
    }

    /// True when this plugin declared a content filter at all.
    pub fn has_nsfw_filter(&self) -> bool {
        self.spec.nsfw.is_some()
    }
}

/// `sel@attr` -> (Some("sel"), Some("attr")); `sel` -> (Some("sel"), None).
fn split_spec(spec: &str) -> (Option<&str>, Option<&str>) {
    match spec.rsplit_once('@') {
        Some((sel, attr)) => {
            let sel = sel.trim();
            (if sel.is_empty() { None } else { Some(sel) }, Some(attr.trim()))
        }
        None => (Some(spec.trim()), None),
    }
}

fn validate(spec: &str) -> Result<()> {
    if spec.starts_with("text:") {
        return Ok(());
    }
    if let (Some(sel), _) = split_spec(spec) {
        Selector::parse(sel).map_err(|e| anyhow!("bad selector `{sel}`: {e}"))?;
    }
    Ok(())
}

fn compile(spec: &str) -> Result<Option<Selector>> {
    match split_spec(spec).0 {
        Some(sel) => Ok(Some(
            Selector::parse(sel).map_err(|e| anyhow!("bad selector `{sel}`: {e}"))?,
        )),
        None => Ok(None),
    }
}

/// Pull one value out of an element according to a selector spec.
///
/// A `text:` prefix supplies a literal instead of reading the document, which
/// is how a site hosting a single series names it: there is no element on the
/// page holding the series title or its own URL.
fn extract(el: &ElementRef, spec: &str) -> Option<String> {
    if let Some(literal) = spec.strip_prefix("text:") {
        return (!literal.is_empty()).then(|| literal.to_string());
    }

    let (sel, attr) = split_spec(spec);
    let target = match sel {
        Some(s) => el.select(&Selector::parse(s).ok()?).next()?,
        None => *el,
    };

    let raw = match attr {
        Some(a) => target.value().attr(a)?.to_string(),
        None => target.text().collect::<String>(),
    };

    let cleaned = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Resolve a possibly-relative link against the source base URL.
fn absolute(base: &str, link: &str) -> String {
    let base = base.trim_end_matches('/');
    if link.starts_with("http://") || link.starts_with("https://") {
        link.to_string()
    } else if let Some(rest) = link.strip_prefix("//") {
        format!("https://{rest}")
    } else if link.starts_with('/') {
        format!("{base}{link}")
    } else {
        format!("{base}/{link}")
    }
}

/// Minimal percent-encoding for query values; avoids pulling in a URL crate.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// The parse_* helpers are deliberately synchronous: `Html` is not `Send`, so it
// must never be alive across an await point inside the async trait methods.

fn parse_search(src: &DeclarativeSource, html: &str) -> Result<Vec<MangaRef>> {
    let doc = Html::parse_document(html);
    let item = compile(&src.spec.search.item)?.ok_or_else(|| anyhow!("search.item needs a selector"))?;
    let direction = src.direction();
    let mut out = Vec::new();

    for el in doc.select(&item) {
        let (Some(title), Some(link)) = (
            extract(&el, &src.spec.search.title),
            extract(&el, &src.spec.search.link),
        ) else {
            continue;
        };

        out.push(MangaRef {
            source: src.spec.name.clone(),
            id: absolute(&src.spec.base, &link),
            title,
            cover: src
                .spec
                .search
                .cover
                .as_ref()
                .and_then(|c| extract(&el, c))
                .map(|c| absolute(&src.spec.base, &c)),
            description: src
                .spec
                .search
                .description
                .as_ref()
                .and_then(|d| extract(&el, d))
                .unwrap_or_default(),
            status: String::new(),
            direction,
        });
    }

    Ok(out)
}

fn parse_chapters(src: &DeclarativeSource, html: &str) -> Result<Vec<Chapter>> {
    let doc = Html::parse_document(html);
    let item = compile(&src.spec.chapters.item)?.ok_or_else(|| anyhow!("chapters.item needs a selector"))?;
    let mut out = Vec::new();

    for el in doc.select(&item) {
        let Some(link) = extract(&el, &src.spec.chapters.link) else {
            continue;
        };
        let number = src
            .spec
            .chapters
            .number
            .as_ref()
            .and_then(|n| extract(&el, n))
            .unwrap_or_else(|| "?".into());

        out.push(Chapter {
            id: absolute(&src.spec.base, &link),
            number,
            title: src
                .spec
                .chapters
                .title
                .as_ref()
                .and_then(|t| extract(&el, t))
                .unwrap_or_default(),
            lang: String::new(),
        });
    }

    if src.spec.chapters.reverse {
        out.reverse();
    }
    Ok(out)
}

/// Page URLs read out of the document by selector.
fn urls_by_selector(item: &str, html: &str) -> Result<Vec<String>> {
    let doc = Html::parse_document(html);
    let (sel, attr) = split_spec(item);
    let sel = sel.ok_or_else(|| anyhow!("pages.item needs a selector"))?;
    let selector = Selector::parse(sel).map_err(|e| anyhow!("bad selector `{sel}`: {e}"))?;
    let attr = attr.unwrap_or("src");

    Ok(doc
        .select(&selector)
        .filter_map(|el| el.value().attr(attr))
        .map(|u| u.trim().to_string())
        .collect())
}

/// Page URLs read out of the raw HTML by regex.
///
/// Sites whose reader is built in JavaScript keep the page list in a script
/// variable, so there is no element to select. Every match contributes its
/// first capture group (or the whole match, when the pattern has none), which
/// `split` then cuts into individual URLs if the site packs them into one
/// string.
fn urls_by_regex(re: &Regex, split: Option<&str>, html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for caps in re.captures_iter(html) {
        let Some(m) = caps.get(1).or_else(|| caps.get(0)) else {
            continue;
        };
        match split {
            Some(sep) if !sep.is_empty() => out.extend(m.as_str().split(sep).map(str::to_string)),
            _ => out.push(m.as_str().to_string()),
        }
    }
    out.into_iter()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect()
}

fn parse_pages(src: &DeclarativeSource, html: &str) -> Result<Vec<Page>> {
    let urls = match (&src.pages_re, &src.spec.pages.item) {
        (Some(re), _) => urls_by_regex(re, src.spec.pages.split.as_deref(), html),
        (None, Some(item)) => urls_by_selector(item, html)?,
        (None, None) => bail!("pages: needs `item` or `regex`"),
    };

    Ok(urls
        .into_iter()
        .map(|u| Page {
            url: absolute(&src.spec.base, &u),
            headers: src.headers.clone(),
        })
        .collect())
}

#[async_trait::async_trait]
impl Source for DeclarativeSource {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn set_allow_nsfw(&self, allow: bool) {
        self.allow_nsfw.store(allow, Ordering::Relaxed);
    }

    fn supports_nsfw_filter(&self) -> bool {
        self.has_nsfw_filter()
    }

    async fn search(&self, query: &str) -> Result<Vec<MangaRef>> {
        let url = self.url(&self.spec.search.url, "{query}", &urlencode(query));
        let html = get_text(&url, &self.headers).await?;
        parse_search(self, &html)
    }

    async fn chapters(&self, manga: &MangaRef) -> Result<Chapters> {
        let url = match &self.spec.chapters.url {
            Some(t) => self.url(t, "{manga}", &manga.id),
            None => manga.id.clone(),
        };
        let html = get_text(&url, &self.headers).await?;
        // A scraped site has no notion of external or untranslated chapters:
        // whatever the selector matched is what there is.
        Ok(Chapters::new(parse_chapters(self, &html)?))
    }

    async fn pages(&self, _manga: &MangaRef, chapter: &Chapter) -> Result<Vec<Page>> {
        let url = match &self.spec.pages.url {
            Some(t) => self.url(t, "{chapter}", &chapter.id),
            None => chapter.id.clone(),
        };
        let html = get_text(&url, &self.headers).await?;
        parse_pages(self, &html)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        toml::from_str(
            r##"
name = "Test"
base = "https://example.test"

[search]
url = "/search?q={query}"
item = ".card"
title = "h3 a"
link = "h3 a@href"
cover = "img@data-src"

[chapters]
item = "li.ch"
link = "a@href"
number = ".num"

[pages]
item = "#viewer img@src"
"##,
        )
        .unwrap()
    }

    #[test]
    fn extracts_search_results() {
        let src = DeclarativeSource::from_spec(spec()).unwrap();
        let html = r#"<div class="card">
             <img data-src="/c/1.jpg"><h3><a href="/manga/1">  Some   Title </a></h3>
           </div>"#;
        let out = parse_search(&src, html).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Some Title");
        assert_eq!(out[0].id, "https://example.test/manga/1");
        assert_eq!(out[0].cover.as_deref(), Some("https://example.test/c/1.jpg"));
    }

    #[test]
    fn chapters_flip_into_reading_order() {
        let src = DeclarativeSource::from_spec(spec()).unwrap();
        let html = r#"<ul>
            <li class="ch"><span class="num">2</span><a href="/c/2"></a></li>
            <li class="ch"><span class="num">1</span><a href="/c/1"></a></li>
          </ul>"#;
        let out = parse_chapters(&src, html).unwrap();

        assert_eq!(out[0].number, "1");
        assert_eq!(out[1].number, "2");
    }

    #[test]
    fn pages_read_the_requested_attribute() {
        let src = DeclarativeSource::from_spec(spec()).unwrap();
        let html = r#"<div id="viewer"><img src="//cdn.test/a.png"><img src="/b.png"></div>"#;
        let out = parse_pages(&src, html).unwrap();

        assert_eq!(out[0].url, "https://cdn.test/a.png");
        assert_eq!(out[1].url, "https://example.test/b.png");
    }

    #[test]
    fn pages_can_come_from_a_script_variable() {
        let mut s = spec();
        s.pages.item = None;
        s.pages.regex = Some(r#"var chapImages = "([^"]+)""#.into());
        s.pages.split = Some(",".into());
        let src = DeclarativeSource::from_spec(s).unwrap();

        let html = r#"<script> var chapImages = "https://cdn.test/1.webp,/2.webp"; </script>"#;
        let out = parse_pages(&src, html).unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].url, "https://cdn.test/1.webp");
        assert_eq!(out[1].url, "https://example.test/2.webp");
    }

    #[test]
    fn pages_need_exactly_one_of_item_and_regex() {
        let mut both = spec();
        both.pages.regex = Some(".".into());
        assert!(DeclarativeSource::from_spec(both).is_err());

        let mut neither = spec();
        neither.pages.item = None;
        assert!(DeclarativeSource::from_spec(neither).is_err());
    }

    #[test]
    fn bad_selectors_fail_at_load_time() {
        let mut s = spec();
        s.search.item = ">>>".into();
        assert!(DeclarativeSource::from_spec(s).is_err());
    }
}
