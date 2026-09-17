//! Application state, key handling, and the background tasks that feed it.
//!
//! Every slow thing (HTTP, decode, resize+encode) runs off the UI task and
//! reports back through a single `Msg` channel, so the render loop never blocks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use image::DynamicImage;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Rect, Size};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Resize};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::model::{Chapter, Chapters, Direction, MangaRef, Page};
use crate::net::fetch_page;
use crate::reader::{Reader, ViewMode};
use crate::source::Registry;
use crate::state::{Progress, State};

pub enum Msg {
    Results(Vec<MangaRef>),
    Chapters(String, Chapters),
    Pages(String, Vec<Page>),
    Image(usize, Arc<DynamicImage>),
    Cover(String, Arc<DynamicImage>),
    Error(String),
    /// A finished reader frame, tagged with the generation that asked for it.
    /// `None` means the encode failed; it still has to arrive, or the
    /// one-at-a-time guard would never be released.
    Encoded(u64, Option<Protocol>),
    /// A finished cover, tagged with the series it belongs to.
    CoverEncoded(String, Option<Protocol>),
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Screen {
    Home,
    Detail,
    Reader,
    Settings,
}

/// One row on the settings screen.
pub enum SettingItem {
    AllowNsfw,
    /// A source, toggled on or off by name.
    Source(String),
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum HomeTab {
    Search,
    Library,
}

/// How images are scaled into their pane.
///
/// `Resize::Fit` only ever shrinks, so anything smaller than the pane — a
/// stitched webtoon slice, typically — would render at native size and leave
/// the rest of the pane empty. CatmullRom rather than Lanczos3 because this
/// runs on every scroll step.
pub(crate) fn scaling() -> Resize {
    scaling_with(false)
}

/// `fast` picks bilinear over CatmullRom.
///
/// While the view is moving, no one can see the difference between the two
/// filters, but the cheaper one buys frames. The last frame of a scroll is
/// re-encoded at full quality once it settles.
pub(crate) fn scaling_with(fast: bool) -> Resize {
    Resize::Scale(Some(if fast {
        FilterType::Triangle
    } else {
        FilterType::CatmullRom
    }))
}

/// Encode an image into a terminal protocol off the UI task.
///
/// Encoding is CPU-bound, so it goes to the blocking pool rather than stealing
/// a runtime worker that is busy downloading pages.
fn spawn_encode(
    picker: Picker,
    tx: UnboundedSender<Msg>,
    image: DynamicImage,
    area: Rect,
    fast: bool,
    wrap: impl FnOnce(Option<Protocol>) -> Msg + Send + 'static,
) {
    let size = Size::new(area.width, area.height);
    tokio::spawn(async move {
        let done = tokio::task::spawn_blocking(move || {
            picker.new_protocol(image, size, scaling_with(fast))
        })
        .await;
        // Always reports back, success or not, so the caller's in-flight guard
        // is released either way.
        let _ = tx.send(wrap(done.ok().and_then(|r| r.ok())));
    });
}

pub struct Detail {
    pub manga: MangaRef,
    pub chapters: Vec<Chapter>,
    pub sel: usize,
    pub loading: bool,
    /// Set when the list came back empty, explaining why.
    pub note: Option<String>,
}

pub struct App {
    pub screen: Screen,
    pub tab: HomeTab,
    pub registry: Arc<Registry>,
    pub source_idx: usize,
    pub query: String,
    pub editing: bool,
    pub results: Vec<MangaRef>,
    pub sel: usize,
    pub detail: Option<Detail>,
    pub reader: Option<Reader>,
    pub settings_sel: usize,
    pub state: State,
    pub status: String,
    pub busy: bool,
    pub quit: bool,
    pub show_help: bool,

    pub picker: Picker,
    pub cover_protocol: Option<Protocol>,
    /// Which series `cover_protocol` was encoded for, and at what size.
    cover_key: Option<(String, u16, u16)>,
    covers: HashMap<String, Arc<DynamicImage>>,
    inflight_pages: HashSet<usize>,
    inflight_covers: HashSet<String>,

    /// Pane rectangles recorded by the last draw, so image encoding can be
    /// driven from the event loop without duplicating the layout here.
    pub reader_area: Rect,
    pub cover_area: Rect,
    /// At most one reader frame encodes at a time. Without this, holding a
    /// scroll key queues an encode per repeat, the blocking pool falls minutes
    /// behind the input, and the view only catches up on key release.
    reader_encoding: bool,

    pub tx: UnboundedSender<Msg>,
}

impl App {
    pub fn new(picker: Picker, registry: Registry) -> (Self, tokio::sync::mpsc::UnboundedReceiver<Msg>) {
        let (tx, rx) = unbounded_channel();

        let registry = Arc::new(registry);
        let status = if registry.errors.is_empty() {
            format!("{} sources loaded", registry.sources.len())
        } else {
            format!("{} plugin(s) failed to load — press ? for details", registry.errors.len())
        };

        let mut app = Self {
            screen: Screen::Home,
            tab: HomeTab::Search,
            registry,
            source_idx: 0,
            query: String::new(),
            editing: false,
            results: Vec::new(),
            sel: 0,
            detail: None,
            reader: None,
            settings_sel: 0,
            state: State::load(),
            status,
            busy: false,
            quit: false,
            show_help: false,
            picker,
            cover_protocol: None,
            cover_key: None,
            covers: HashMap::new(),
            inflight_pages: HashSet::new(),
            inflight_covers: HashSet::new(),
            reader_area: Rect::ZERO,
            cover_area: Rect::ZERO,
            reader_encoding: false,
            tx,
        };

        app.registry.set_allow_nsfw(app.state.allow_nsfw);

        // Reopen on whichever source was used last, unless it has since been
        // switched off or removed.
        let names = app.registry.names();
        let remembered = app
            .state
            .last_source
            .as_ref()
            .and_then(|name| names.iter().position(|n| n == name));
        app.source_idx = remembered
            .filter(|i| app.is_enabled(&names[*i]))
            .or_else(|| names.iter().position(|n| app.is_enabled(n)))
            .unwrap_or(0);

        if app.state.library.is_empty() {
            app.search();
        } else {
            app.tab = HomeTab::Library;
        }
        (app, rx)
    }

    pub fn source_name(&self) -> String {
        self.registry
            .names()
            .get(self.source_idx)
            .cloned()
            .unwrap_or_default()
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        !self.state.disabled_sources.iter().any(|d| d == name)
    }

    /// Move to the next enabled source and re-run the query against it.
    ///
    /// Re-running matters: leaving the previous source's results on screen
    /// under a new source's name looks exactly like the new one returning
    /// nothing.
    fn cycle_source(&mut self) {
        let names = self.registry.names();
        let n = names.len();
        if n == 0 {
            return;
        }

        // Walk past anything switched off in settings, and stop if everything
        // is, rather than looping forever.
        for step in 1..=n {
            let candidate = (self.source_idx + step) % n;
            if self.is_enabled(&names[candidate]) {
                self.source_idx = candidate;
                self.state.last_source = Some(self.source_name());
                self.state.save();
                self.search();
                return;
            }
        }
        self.status = "every source is disabled — press , for settings".into();
    }

    pub fn settings_items(&self) -> Vec<SettingItem> {
        let mut items = vec![SettingItem::AllowNsfw];
        items.extend(self.registry.names().into_iter().map(SettingItem::Source));
        items
    }

    /// The list currently under the cursor on the home screen.
    pub fn list(&self) -> &[MangaRef] {
        match self.tab {
            HomeTab::Search => &self.results,
            HomeTab::Library => &self.state.library,
        }
    }

    pub fn selected(&self) -> Option<&MangaRef> {
        self.list().get(self.sel)
    }

    // --- background work -------------------------------------------------

    fn search(&mut self) {
        let Some(src) = self.registry.get(&self.source_name()) else {
            self.status = "no source selected".into();
            return;
        };
        let q = self.query.clone();
        let tx = self.tx.clone();
        self.busy = true;
        self.status = if q.is_empty() {
            format!("browsing {}", src.name())
        } else {
            format!("searching {} for {q}", src.name())
        };

        tokio::spawn(async move {
            let msg = match src.search(&q).await {
                Ok(r) => Msg::Results(r),
                Err(e) => Msg::Error(format!("search failed: {e:#}")),
            };
            let _ = tx.send(msg);
        });
    }

    fn load_chapters(&mut self, manga: MangaRef) {
        let Some(src) = self.registry.get(&manga.source) else {
            self.status = format!("source {} is not loaded", manga.source);
            return;
        };
        let tx = self.tx.clone();
        let key = manga.key();
        self.busy = true;

        tokio::spawn(async move {
            let msg = match src.chapters(&manga).await {
                Ok(c) => Msg::Chapters(key, c),
                Err(e) => Msg::Error(format!("chapter list failed: {e:#}")),
            };
            let _ = tx.send(msg);
        });
    }

    fn load_pages(&mut self) {
        let Some(reader) = &self.reader else { return };
        let (Some(src), Some(chapter)) = (
            self.registry.get(&reader.manga.source),
            reader.chapter_ref().cloned(),
        ) else {
            return;
        };
        let manga = reader.manga.clone();
        let key = manga.key();
        let tx = self.tx.clone();
        self.busy = true;
        self.inflight_pages.clear();

        tokio::spawn(async move {
            let msg = match src.pages(&manga, &chapter).await {
                Ok(p) => Msg::Pages(key, p),
                Err(e) => Msg::Error(format!("page list failed: {e:#}")),
            };
            let _ = tx.send(msg);
        });
    }

    /// Kick off downloads for whatever the reader wants next.
    pub fn prefetch(&mut self) {
        let Some(reader) = &self.reader else { return };
        let wanted: Vec<usize> = reader
            .wanted()
            .into_iter()
            .filter(|i| !self.inflight_pages.contains(i))
            .collect();

        for idx in wanted {
            let Some(page) = reader.pages.get(idx).cloned() else {
                continue;
            };
            self.inflight_pages.insert(idx);
            let tx = self.tx.clone();

            tokio::spawn(async move {
                let bytes = match fetch_page(&page).await {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Msg::Error(format!("page {}: {e:#}", idx + 1)));
                        return;
                    }
                };
                // Decoding a 4MB PNG blocks for long enough to stutter the UI.
                match tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await {
                    Ok(Ok(img)) => {
                        let _ = tx.send(Msg::Image(idx, Arc::new(img)));
                    }
                    Ok(Err(e)) => {
                        let _ = tx.send(Msg::Error(format!("decode page {}: {e}", idx + 1)));
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Error(format!("decode task: {e}")));
                    }
                }
            });
        }
    }

    /// Fetch the cover of the highlighted series, once.
    pub fn ensure_cover(&mut self) {
        let Some(manga) = self.selected().cloned() else {
            return;
        };
        let key = manga.key();
        if self.covers.contains_key(&key) || self.inflight_covers.contains(&key) {
            self.refresh_cover_proto();
            return;
        }
        let Some(url) = manga.cover.clone() else { return };

        self.inflight_covers.insert(key.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(bytes) = fetch_page(&Page::new(url)).await {
                if let Ok(Ok(img)) =
                    tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await
                {
                    let _ = tx.send(Msg::Cover(key, Arc::new(img)));
                }
            }
        });
    }

    /// The series whose cover the preview pane should be showing.
    fn preview_key(&self) -> Option<String> {
        match self.screen {
            Screen::Detail => self.detail.as_ref().map(|d| d.manga.key()),
            _ => self.selected().map(|m| m.key()),
        }
    }

    fn refresh_cover_proto(&mut self) {
        if self.preview_key().as_deref() != self.cover_key.as_ref().map(|(k, _, _)| k.as_str()) {
            self.cover_protocol = None;
            self.cover_key = None;
        }
    }

    /// Advance any in-progress scroll animation by one frame.
    ///
    /// Returns true when the view moved and the screen needs redrawing, so the
    /// event loop can stay idle the rest of the time rather than repainting at
    /// the tick rate regardless.
    pub fn animate(&mut self) -> bool {
        if self.screen != Screen::Reader {
            return false;
        }
        match self.reader.as_mut() {
            Some(reader) if reader.mode == ViewMode::Strip => {
                if reader.tick() {
                    self.prefetch();
                    return true;
                }
                // Settled. If the frame on screen came out of the cheap filter,
                // one more encode replaces it with the sharp version.
                let Some(reader) = self.reader.as_mut() else {
                    return false;
                };
                if reader.encoded_fast {
                    reader.encoded_fast = false;
                    reader.invalidate();
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// Encode whatever the visible panes need, once per draw.
    ///
    /// Driven from the event loop using the rectangles the last draw recorded,
    /// so the layout stays defined in one place.
    pub fn refresh_images(&mut self) {
        self.refresh_reader_image();
        self.refresh_cover_image();
    }

    fn refresh_reader_image(&mut self) {
        let area = self.reader_area;
        if self.reader_encoding || self.screen != Screen::Reader {
            return;
        }
        if area.width == 0 || area.height == 0 {
            return;
        }
        let picker = self.picker.clone();
        let Some(reader) = &mut self.reader else { return };
        // Nothing is consumed when an encode is already running, so the view
        // stays marked stale and the next pass picks up wherever scrolling
        // ended up rather than replaying every intermediate position.
        let Some((generation, image)) = reader.pending_view(&picker, area) else {
            return;
        };

        // Cheap filter while the view is moving, sharp once it stops.
        let fast = reader.is_animating();
        reader.encoded_fast = fast;

        self.reader_encoding = true;
        spawn_encode(picker, self.tx.clone(), image, area, fast, move |p| {
            Msg::Encoded(generation, p)
        });
    }

    fn refresh_cover_image(&mut self) {
        let area = self.cover_area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let Some(key) = self.preview_key() else { return };

        // Re-encode when the series changes or the pane is resized.
        let wanted = (key.clone(), area.width, area.height);
        if self.cover_key.as_ref() == Some(&wanted) {
            return;
        }
        let Some(image) = self.covers.get(&key).map(|i| i.as_ref().clone()) else {
            return;
        };

        self.cover_key = Some(wanted);
        // A cover is encoded once and then sits there, so always full quality.
        spawn_encode(self.picker.clone(), self.tx.clone(), image, area, false, move |p| {
            Msg::CoverEncoded(key, p)
        });
    }

    // --- messages --------------------------------------------------------

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Results(r) => {
                self.busy = false;
                self.status = format!("{} results", r.len());
                self.results = r;
                self.sel = 0;
                self.tab = HomeTab::Search;
                self.cover_key = None;
                self.ensure_cover();
            }
            Msg::Chapters(key, chapters) => {
                self.busy = false;
                if let Some(d) = &mut self.detail {
                    if d.manga.key() == key {
                        let note = chapters.explain_empty();
                        // Resume on the chapter the reader stopped at.
                        d.sel = self
                            .state
                            .progress
                            .get(&key)
                            .and_then(|p| chapters.items.iter().position(|c| c.id == p.chapter_id))
                            .unwrap_or(0);
                        d.chapters = chapters.items;
                        d.loading = false;
                        self.status = match &note {
                            Some(why) => why.clone(),
                            None if chapters.external > 0 => format!(
                                "{} chapters ({} on official readers, skipped)",
                                d.chapters.len(),
                                chapters.external
                            ),
                            None => format!("{} chapters", d.chapters.len()),
                        };
                        d.note = note;
                    }
                }
            }
            Msg::Pages(key, pages) => {
                self.busy = false;
                if let Some(reader) = &mut self.reader {
                    if reader.manga.key() == key {
                        let n = pages.len();
                        reader.set_pages(pages);

                        // Restore the exact spot inside this chapter, if we have one.
                        if let Some(p) = self.state.progress.get(&key) {
                            if reader.chapter_ref().map(|c| c.id.as_str()) == Some(p.chapter_id.as_str())
                                && p.page < n
                            {
                                reader.idx = p.page;
                                reader.offset = p.offset;
                                reader.invalidate();
                            }
                        }
                        self.status = format!("{n} pages");
                        self.prefetch();
                    }
                }
            }
            Msg::Image(idx, img) => {
                self.inflight_pages.remove(&idx);
                if let Some(reader) = &mut self.reader {
                    reader.insert_image(idx, img);
                    reader.evict();
                }
                self.prefetch();
            }
            Msg::Cover(key, img) => {
                self.inflight_covers.remove(&key);
                self.covers.insert(key, img);
                self.refresh_cover_proto();
            }
            Msg::Error(e) => {
                self.busy = false;
                self.status = e;
            }
            Msg::Encoded(generation, protocol) => {
                self.reader_encoding = false;
                match protocol {
                    Some(p) => {
                        if let Some(reader) = &mut self.reader {
                            reader.accept(generation, p);
                        }
                    }
                    None => self.status = "failed to encode frame".into(),
                }
            }
            Msg::CoverEncoded(key, protocol) => {
                // Only apply it if the selection has not moved on meanwhile.
                if self.cover_key.as_ref().is_some_and(|(k, _, _)| *k == key) {
                    self.cover_protocol = protocol;
                }
            }
        }
    }

    // --- input -----------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        if self.show_help {
            self.show_help = false;
            return;
        }

        match self.screen {
            Screen::Home => self.home_key(key),
            Screen::Detail => self.detail_key(key),
            Screen::Reader => self.reader_key(key),
            Screen::Settings => self.settings_key(key),
        }
    }

    fn settings_key(&mut self, key: KeyEvent) {
        let len = self.settings_items().len();
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Char(',') => {
                self.screen = Screen::Home;
                self.refresh_cover_proto();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.settings_sel = (self.settings_sel + 1).min(len.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.settings_sel = self.settings_sel.saturating_sub(1)
            }
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_setting(),
            _ => {}
        }
    }

    fn toggle_setting(&mut self) {
        let Some(item) = self.settings_items().into_iter().nth(self.settings_sel) else {
            return;
        };

        match item {
            SettingItem::AllowNsfw => {
                self.state.allow_nsfw = !self.state.allow_nsfw;
                self.registry.set_allow_nsfw(self.state.allow_nsfw);
                self.state.save();
                self.status = if self.state.allow_nsfw {
                    "adult content allowed".into()
                } else {
                    "adult content hidden".into()
                };
                // Results on screen were fetched under the old filter.
                self.results.clear();
                self.search();
            }
            SettingItem::Source(name) => {
                if let Some(i) = self.state.disabled_sources.iter().position(|d| *d == name) {
                    self.state.disabled_sources.remove(i);
                } else {
                    self.state.disabled_sources.push(name.clone());
                    // Leaving the cursor on a source just switched off would
                    // keep searching it.
                    if self.source_name() == name {
                        self.cycle_source();
                    }
                }
                self.state.save();
            }
        }
    }

    fn home_key(&mut self, key: KeyEvent) {
        if self.editing {
            match key.code {
                KeyCode::Esc => self.editing = false,
                // Also handled here: the search box otherwise swallows it, and
                // switching source mid-query is a reasonable thing to want.
                KeyCode::Tab => self.cycle_source(),
                KeyCode::Enter => {
                    self.editing = false;
                    self.search();
                }
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c) => self.query.push(c),
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char(',') => {
                self.screen = Screen::Settings;
                self.settings_sel = 0;
            }
            KeyCode::Char('/') => {
                self.editing = true;
                self.query.clear();
            }
            KeyCode::Tab => self.cycle_source(),
            KeyCode::Char('L') | KeyCode::Char('l') => {
                self.tab = match self.tab {
                    HomeTab::Search => HomeTab::Library,
                    HomeTab::Library => HomeTab::Search,
                };
                self.sel = 0;
                self.cover_key = None;
                self.ensure_cover();
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Char('s') => {
                if let Some(m) = self.selected().cloned() {
                    let saved = self.state.toggle_library(&m);
                    self.status = if saved { "saved to library".into() } else { "removed".into() };
                }
            }
            KeyCode::Enter => {
                if let Some(m) = self.selected().cloned() {
                    self.detail = Some(Detail {
                        manga: m.clone(),
                        chapters: Vec::new(),
                        sel: 0,
                        loading: true,
                        note: None,
                    });
                    self.screen = Screen::Detail;
                    self.refresh_cover_proto();
                    self.load_chapters(m);
                }
            }
            _ => {}
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let len = self.list().len();
        if len == 0 {
            return;
        }
        let next = (self.sel as i64 + delta).clamp(0, len as i64 - 1) as usize;
        if next != self.sel {
            self.sel = next;
            self.ensure_cover();
        }
    }

    fn detail_key(&mut self, key: KeyEvent) {
        let Some(d) = &mut self.detail else {
            self.screen = Screen::Home;
            return;
        };

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Esc | KeyCode::Backspace => {
                self.screen = Screen::Home;
                self.cover_key = None;
                self.refresh_cover_proto();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                d.sel = (d.sel + 1).min(d.chapters.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => d.sel = d.sel.saturating_sub(1),
            KeyCode::PageDown => d.sel = (d.sel + 10).min(d.chapters.len().saturating_sub(1)),
            KeyCode::PageUp => d.sel = d.sel.saturating_sub(10),
            KeyCode::Char('g') => d.sel = 0,
            KeyCode::Char('G') => d.sel = d.chapters.len().saturating_sub(1),
            KeyCode::Char('s') => {
                let m = d.manga.clone();
                let saved = self.state.toggle_library(&m);
                self.status = if saved { "saved to library".into() } else { "removed".into() };
            }
            KeyCode::Enter | KeyCode::Char('r') => self.open_reader(),
            _ => {}
        }
    }

    fn open_reader(&mut self) {
        let Some(d) = &self.detail else { return };
        if d.chapters.is_empty() {
            self.status = "no chapters".into();
            return;
        }
        let mut reader = Reader::new(
            d.manga.clone(),
            d.chapters.clone(),
            d.sel.min(d.chapters.len() - 1),
        );
        reader.mode = match d.manga.direction {
            Direction::Vertical => ViewMode::Strip,
            _ => ViewMode::Paged,
        };
        self.reader = Some(reader);
        self.screen = Screen::Reader;
        self.load_pages();
    }

    fn reader_key(&mut self, key: KeyEvent) {
        let Some(reader) = &mut self.reader else {
            self.screen = Screen::Detail;
            return;
        };
        let rtl = reader.manga.direction == Direction::RightToLeft;

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Esc | KeyCode::Backspace => {
                self.save_progress();
                self.screen = Screen::Detail;
                self.refresh_cover_proto();
            }
            KeyCode::Char('v') => reader.toggle_mode(),
            KeyCode::Char('n') => self.change_chapter(1),
            KeyCode::Char('p') => self.change_chapter(-1),
            KeyCode::Char('g') => {
                reader.go_start();
                self.prefetch();
            }
            KeyCode::Char('G') => {
                reader.go_end();
                self.prefetch();
            }
            code => {
                let strip = reader.mode == ViewMode::Strip;
                match code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        if strip {
                            reader.scroll(120);
                        } else {
                            reader.next_page();
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if strip {
                            reader.scroll(-120);
                        } else {
                            reader.prev_page();
                        }
                    }
                    KeyCode::Char(' ') | KeyCode::PageDown => {
                        if strip {
                            reader.scroll(600);
                        } else {
                            reader.next_page();
                        }
                    }
                    KeyCode::Char('b') | KeyCode::PageUp => {
                        if strip {
                            reader.scroll(-600);
                        } else {
                            reader.prev_page();
                        }
                    }
                    KeyCode::Left => {
                        if rtl {
                            reader.next_page();
                        } else {
                            reader.prev_page();
                        }
                    }
                    KeyCode::Right => {
                        if rtl {
                            reader.prev_page();
                        } else {
                            reader.next_page();
                        }
                    }
                    _ => return,
                }
                self.prefetch();
                self.save_progress();
            }
        }
    }

    fn change_chapter(&mut self, delta: i64) {
        let Some(reader) = &mut self.reader else { return };
        let next = reader.chapter as i64 + delta;
        if next < 0 || next as usize >= reader.chapters.len() {
            self.status = "no more chapters".into();
            return;
        }
        reader.chapter = next as usize;
        reader.set_pages(Vec::new());
        self.load_pages();
    }

    fn save_progress(&mut self) {
        let Some(reader) = &self.reader else { return };
        let Some(chapter) = reader.chapter_ref() else { return };
        let progress = Progress {
            chapter_id: chapter.id.clone(),
            chapter_number: chapter.number.clone(),
            page: reader.idx,
            offset: reader.offset,
        };
        let manga = reader.manga.clone();
        self.state.record(&manga, progress);
    }
}
