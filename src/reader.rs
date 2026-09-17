//! The reading surface: page cache, strip composition, and protocol lifecycle.
//!
//! Two view modes share one pipeline. Paged mode hands a whole image to the
//! widget. Strip mode stitches a window out of consecutive slices so a webtoon
//! scrolls continuously across the boundaries between them, which is what makes
//! it read like a webtoon instead of a slideshow.

use std::collections::HashMap;
use std::sync::Arc;

use image::{DynamicImage, Rgba, RgbaImage};
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;

use crate::model::{Chapter, Direction, MangaRef, Page};

/// Stand-in height for a slice whose image has not arrived yet, so that
/// scrolling has something to work with before anything is loaded.
const ESTIMATED_SLICE_HEIGHT: u32 = 1600;

/// How many pages ahead of the cursor to fetch in the background.
pub const PREFETCH_AHEAD: usize = 3;
const PREFETCH_BEHIND: usize = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewMode {
    Paged,
    Strip,
}

impl ViewMode {
    pub fn label(self) -> &'static str {
        match self {
            ViewMode::Paged => "paged",
            ViewMode::Strip => "strip",
        }
    }
}

/// Identifies exactly what is on screen. Re-encoding is skipped when unchanged,
/// which is the difference between a responsive reader and a slideshow.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ViewKey {
    mode: ViewMode,
    idx: usize,
    offset: u32,
    area: (u16, u16),
}


/// One slice of one page, placed in the strip window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    pub idx: usize,
    /// First row taken from that page.
    pub offset: u32,
    /// How many rows.
    pub take: u32,
    /// Where those rows land in the window.
    pub y: u32,
}

pub struct Reader {
    pub manga: MangaRef,
    pub chapters: Vec<Chapter>,
    /// The chapter the current scroll position falls in, derived from
    /// `chapter_of` rather than set directly.
    pub chapter: usize,
    pub pages: Vec<Page>,
    /// Which chapter each page belongs to. In strip mode the page list grows
    /// past the end of a chapter so reading runs straight into the next one.
    chapter_of: Vec<usize>,
    /// Highest chapter appended so far.
    last_loaded: usize,
    /// A fetch for the following chapter is already in flight.
    pub pending_chapter: bool,
    /// Bumped whenever the page list is replaced. An image download started
    /// against the old list would otherwise land in whatever slot now has
    /// that index, putting a page from the previous chapter into this one.
    pub epoch: u64,
    /// Width every strip slice is normalised to, taken from the first one.
    strip_width: Option<u32>,
    /// Height of each slice, the estimate until its image arrives and the
    /// real value thereafter. Kept separately from `images` so that evicting
    /// an image cannot make its height unknown again: absolute positions are
    /// sums of these, and a height that changes back moves the whole view.
    heights: Vec<u32>,
    pub images: HashMap<usize, Arc<DynamicImage>>,
    /// Current page in paged mode, or the slice holding the top of the viewport.
    pub idx: usize,
    /// Pixel offset into that slice. Always 0 in paged mode.
    pub offset: u32,
    pub mode: ViewMode,
    /// The encoded image currently on screen. Kept while the next one encodes:
    /// clearing it first is what makes scrolling flicker, because every step
    /// would draw one empty frame before the replacement arrives.
    pub protocol: Option<Protocol>,
    /// Bumped per encode request so late results from an abandoned scroll
    /// position can be discarded instead of snapping the view backwards.
    pub generation: u64,
    /// Generation of the frame on screen. Encodes run concurrently and can
    /// finish out of order, so a frame is only taken if it is newer than
    /// what is already displayed.
    displayed: u64,
    /// Where scrolling is heading, as an absolute pixel offset into the
    /// chapter. Keys move this; the view eases toward it on a clock, which is
    /// what turns a series of jumps into motion.
    target: i64,
    /// True when the frame on screen was encoded with the cheap filter, so a
    /// quality pass is owed once the view settles.
    pub encoded_fast: bool,
    key: Option<ViewKey>,
}

impl Reader {
    pub fn new(manga: MangaRef, chapters: Vec<Chapter>, chapter: usize) -> Self {
        let mode = match manga.direction {
            Direction::Vertical => ViewMode::Strip,
            _ => ViewMode::Paged,
        };
        Self {
            manga,
            chapters,
            chapter,
            pages: Vec::new(),
            chapter_of: Vec::new(),
            last_loaded: chapter,
            pending_chapter: false,
            epoch: 0,
            strip_width: None,
            heights: Vec::new(),
            images: HashMap::new(),
            idx: 0,
            offset: 0,
            mode,
            protocol: None,
            generation: 0,
            displayed: 0,
            target: 0,
            encoded_fast: false,
            key: None,
        }
    }

    pub fn chapter_ref(&self) -> Option<&Chapter> {
        self.chapters.get(self.chapter)
    }

    /// Replace the page list with one chapter, starting from the top.
    pub fn set_pages(&mut self, chapter: usize, pages: Vec<Page>) {
        self.chapter = chapter;
        self.last_loaded = chapter;
        self.chapter_of = vec![chapter; pages.len()];
        self.heights = vec![ESTIMATED_SLICE_HEIGHT; pages.len()];
        self.pages = pages;
        self.pending_chapter = false;
        self.epoch = self.epoch.wrapping_add(1);
        self.strip_width = None;

        self.images.clear();
        self.idx = 0;
        self.offset = 0;
        self.target = 0;
        // A different chapter entirely, so holding the old frame would show the
        // wrong page rather than hide a flicker.
        self.protocol = None;
        self.displayed = 0;
        self.generation = 0;
        self.invalidate();
    }

    /// Add the following chapter onto the end, leaving the view where it is.
    ///
    /// This is what makes reading continue past a chapter boundary instead of
    /// stopping at it: the scroll never resets, the page list simply gets
    /// longer underneath it.
    pub fn append_pages(&mut self, chapter: usize, pages: Vec<Page>) {
        self.chapter_of
            .extend(std::iter::repeat_n(chapter, pages.len()));
        self.heights
            .extend(std::iter::repeat_n(ESTIMATED_SLICE_HEIGHT, pages.len()));
        self.pages.extend(pages);
        self.last_loaded = chapter;
        self.pending_chapter = false;
    }

    /// The next chapter to fetch, once the end of the loaded pages is near.
    ///
    /// Only in strip mode: paged reading has a natural stop at the last page,
    /// and running two chapters together there would just be confusing.
    pub fn next_chapter_to_load(&self) -> Option<usize> {
        if self.mode != ViewMode::Strip || self.pending_chapter || self.pages.is_empty() {
            return None;
        }
        let next = self.last_loaded + 1;
        if next >= self.chapters.len() {
            return None;
        }
        // The same horizon the page prefetcher uses, so the chapter arrives
        // before the reader can scroll into empty space.
        (self.idx + PREFETCH_AHEAD >= self.pages.len().saturating_sub(1)).then_some(next)
    }

    pub fn invalidate(&mut self) {
        self.key = None;
    }

    pub fn insert_image(&mut self, idx: usize, img: Arc<DynamicImage>) {
        // Normalise every slice to the width of the first one.
        //
        // The strip canvas is as wide as whatever slice is at the top of the
        // viewport, so a chapter drawn at a different resolution changes the
        // scale factor and the whole page appears to zoom as you cross into
        // it. Resizing on arrival keeps one width for the entire read.
        let img = match self.strip_width {
            None => {
                self.strip_width = Some(img.width());
                img
            }
            Some(width) if img.width() != width && img.width() > 0 => {
                let height = (img.height() as u64 * width as u64 / img.width() as u64).max(1) as u32;
                Arc::new(img.resize_exact(width, height, image::imageops::FilterType::Triangle))
            }
            Some(_) => img,
        };

        // Record the real height in place of the estimate. Anything above the
        // viewport changes what every absolute position below it means, so the
        // target moves by the same amount — otherwise the view lurches up or
        // down the moment a page finishes loading.
        let height = img.height();
        if let Some(slot) = self.heights.get_mut(idx) {
            let previous = *slot;
            if previous != height {
                *slot = height;
                if idx < self.idx {
                    self.target += height as i64 - previous as i64;
                }
                // The current offset was measured against the old height. If
                // the real page is shorter, that offset now points past its
                // end, and composing skips to the next page — which looks like
                // a page cut in half with the following one starting.
                if idx <= self.idx {
                    self.normalize_position();
                }
            }
        }

        self.images.insert(idx, img);
        // A slice arriving may complete the current window, so force a redraw.
        if idx >= self.idx && idx <= self.idx + PREFETCH_AHEAD {
            self.invalidate();
        }
    }

    /// Page indices worth having in memory right now, nearest first.
    pub fn wanted(&self) -> Vec<usize> {
        let start = self.idx.saturating_sub(PREFETCH_BEHIND);
        let end = (self.idx + PREFETCH_AHEAD).min(self.pages.len().saturating_sub(1));
        let mut out: Vec<usize> = (start..=end).collect();
        out.sort_by_key(|i| i.abs_diff(self.idx));
        out.retain(|i| !self.images.contains_key(i));
        out
    }

    /// Drop slices far from the cursor so a 200-page chapter cannot grow without bound.
    pub fn evict(&mut self) {
        let idx = self.idx;
        self.images
            .retain(|i, _| i.abs_diff(idx) <= PREFETCH_AHEAD + PREFETCH_BEHIND + 2);
    }

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            ViewMode::Paged => ViewMode::Strip,
            ViewMode::Strip => {
                self.offset = 0;
                ViewMode::Paged
            }
        };
        self.sync_target();
        self.invalidate();
    }

    pub fn next_page(&mut self) -> bool {
        if self.idx + 1 < self.pages.len() {
            self.idx += 1;
            self.offset = 0;
            self.sync_chapter();
            self.sync_target();
            self.invalidate();
            true
        } else {
            false
        }
    }

    pub fn prev_page(&mut self) -> bool {
        if self.idx > 0 {
            self.idx -= 1;
            self.offset = 0;
            self.sync_chapter();
            self.sync_target();
            self.invalidate();
            true
        } else {
            false
        }
    }

    /// The current position as one absolute pixel offset into the chapter.
    fn absolute(&self) -> i64 {
        (0..self.idx).map(|i| self.height_of(i) as i64).sum::<i64>() + self.offset as i64
    }

    /// Total scrollable height, using the same estimate for unloaded slices
    /// that `height_of` uses, so the target cannot run past the end.
    fn total_height(&self) -> i64 {
        (0..self.pages.len()).map(|i| self.height_of(i) as i64).sum()
    }

    /// Aim scrolling at a new absolute position. The view catches up in `tick`.
    pub fn scroll(&mut self, delta: i64) {
        if delta == 0 {
            return;
        }
        // Retarget from the previous target rather than from what is on screen,
        // so key repeats during an animation accumulate instead of fighting it.
        //
        // The limit is the download frontier, not the end of the chapter.
        // Without that, holding a scroll key runs the view past pages that have
        // not arrived: they are blank while they pass, and by the time they
        // decode the reader is already beyond them. That reads as the story
        // skipping, which is worse than waiting a moment for a page.
        let hard = (self.total_height() - 1).max(0);
        let frontier = self.loaded_limit().max(self.absolute());
        self.target = (self.target + delta).clamp(0, hard.min(frontier));
    }

    /// Follow the page list across a chapter boundary.
    fn sync_chapter(&mut self) {
        if let Some(chapter) = self.chapter_of.get(self.idx).copied() {
            self.chapter = chapter;
        }
    }

    /// Absolute position where the first slice that cannot be drawn begins,
    /// counting from the one on screen.
    fn loaded_limit(&self) -> i64 {
        let mut pos = 0i64;
        for i in 0..self.pages.len() {
            if i >= self.idx && !self.images.contains_key(&i) {
                return pos;
            }
            pos += self.height_of(i) as i64;
        }
        pos
    }

    /// True when scrolling is held up waiting for a page to arrive.
    pub fn waiting_for_pages(&self) -> bool {
        self.mode == ViewMode::Strip
            && !self.pages.is_empty()
            && self.loaded_limit() <= self.absolute()
    }

    /// Re-aim at the current position, after a move that was not a scroll.
    ///
    /// The target is authoritative, so anything that repositions the view
    /// directly has to say so or the next tick will animate straight back.
    fn sync_target(&mut self) {
        self.target = self.absolute();
    }

    pub fn go_start(&mut self) {
        self.idx = 0;
        self.offset = 0;
        self.sync_chapter();
        self.sync_target();
        self.invalidate();
    }

    pub fn go_end(&mut self) {
        self.idx = self.pages.len().saturating_sub(1);
        self.offset = 0;
        self.sync_chapter();
        self.sync_target();
        self.invalidate();
    }

    /// Step the view toward its target. Returns true when it moved.
    ///
    /// Eases proportionally, so a large jump covers most of the distance in the
    /// first few frames and settles without overshoot.
    pub fn tick(&mut self) -> bool {
        let current = self.absolute();
        let diff = self.target - current;
        if diff == 0 {
            return false;
        }

        // Clamped rather than purely proportional. A plain exponential ease
        // decelerates hard at the tail, which reads as stepping rather than
        // stopping; the floor keeps the last stretch moving and the ceiling
        // stops a long jump from tearing across in one frame.
        const MIN_STEP: i64 = 14;
        const MAX_STEP: i64 = 70;
        let step = ((diff as f64 * 0.25).abs().round() as i64).clamp(MIN_STEP, MAX_STEP);
        let step = step.min(diff.abs()) * diff.signum();

        self.jump(current + step);
        true
    }

    pub fn is_animating(&self) -> bool {
        self.target != self.absolute()
    }

    /// Position within the current chapter, as (page, total).
    ///
    /// The page list spans chapters once reading continues past one, so a raw
    /// index into it would climb forever and the total would be the whole run
    /// rather than the chapter being read.
    pub fn chapter_span(&self) -> (usize, usize) {
        let start = self
            .chapter_of
            .iter()
            .position(|c| *c == self.chapter)
            .unwrap_or(0);
        let total = self.chapter_of.iter().filter(|c| **c == self.chapter).count();
        (self.idx.saturating_sub(start), total)
    }

    /// Pull `(idx, offset)` back into range without moving the reader.
    ///
    /// Needed when a slice's height changes underneath a position that was
    /// measured against the old one.
    fn normalize_position(&mut self) {
        let offset = self.offset as i64;
        self.place(self.idx, offset);
    }

    /// Move immediately to an absolute position, walking across slice bounds.
    fn jump(&mut self, position: i64) {
        let idx = self.idx;
        let offset = position - (self.absolute() - self.offset as i64);
        self.place(idx, offset);
    }

    /// Normalise a raw `(slice, offset)` pair and adopt it.
    fn place(&mut self, start: usize, start_offset: i64) {
        let mut idx = start;
        let mut offset = start_offset;

        while offset < 0 {
            if idx == 0 {
                offset = 0;
                break;
            }
            idx -= 1;
            // Unloaded slices get a conservative guess so scrolling never stalls.
            offset += self.height_of(idx) as i64;
        }

        loop {
            let h = self.height_of(idx) as i64;
            if offset < h || idx + 1 >= self.pages.len() {
                if idx + 1 >= self.pages.len() {
                    offset = offset.min((h - 1).max(0));
                }
                break;
            }
            offset -= h;
            idx += 1;
        }

        if idx != self.idx || offset as u32 != self.offset {
            self.idx = idx;
            self.offset = offset.max(0) as u32;
            self.sync_chapter();
            self.invalidate();
        }
    }

    fn height_of(&self, idx: usize) -> u32 {
        self.heights
            .get(idx)
            .copied()
            .unwrap_or(ESTIMATED_SLICE_HEIGHT)
    }

    /// The image that should be encoded next, or None when the view is already
    /// current or has no pixels available yet.
    ///
    /// Encoding happens off this thread, so the caller gets the composed image
    /// and a generation number rather than a finished protocol.
    pub fn pending_view(&mut self, picker: &Picker, area: Rect) -> Option<(u64, DynamicImage)> {
        let key = ViewKey {
            mode: self.mode,
            idx: self.idx,
            offset: self.offset,
            area: (area.width, area.height),
        };
        if self.key == Some(key) {
            return None;
        }

        let font = picker.font_size();
        let composed = self.compose(area, font.width, font.height)?;

        self.key = Some(key);
        self.generation += 1;
        Some((self.generation, composed))
    }

    /// Accept a finished encode, ignoring anything older than what is shown.
    ///
    /// Several encodes are allowed to be in flight at once, so they can land
    /// out of order. Taking only the newest keeps the view moving forward
    /// without ever stepping back to a frame that has been superseded.
    pub fn accept(&mut self, generation: u64, protocol: Protocol) {
        if generation > self.displayed {
            self.displayed = generation;
            self.protocol = Some(protocol);
        }
    }

    fn compose(&self, area: Rect, cell_w: u16, cell_h: u16) -> Option<DynamicImage> {
        match self.mode {
            // Deliberately opaque RGB, no alpha channel: the iTerm2 encoder
            // only erases the pane before drawing when an image can be seen
            // through, and that erase is what makes every redraw flash.
            ViewMode::Paged => self
                .images
                .get(&self.idx)
                .map(|i| DynamicImage::ImageRgb8(i.to_rgb8())),
            ViewMode::Strip => {
                let area_w = area.width as u32 * cell_w.max(1) as u32;
                let area_h = area.height as u32 * cell_h.max(1) as u32;
                self.compose_strip(area_w.max(1), area_h.max(1))
            }
        }
    }

    /// Build the visible window of a vertical strip at native resolution.
    ///
    /// The window is made as tall as needed so that, once the widget scales it
    /// to the pane width, its height lands exactly on the pane height. Without
    /// that, `Resize::Fit` would letterbox the strip and scrolling would crawl.

    /// Work out which rows of which pages fill a window of `view_h` rows,
    /// starting from the current position.
    ///
    /// Separated from the drawing so it can be tested: a scroll that silently
    /// skips rows here is content the reader never shows, which is impossible
    /// to spot by eye and easy to assert about.
    fn strip_plan(&self, view_h: u32) -> Vec<Cut> {
        let mut plan = Vec::new();
        let mut y = 0u32;
        let mut idx = self.idx;
        let mut offset = self.offset;

        while y < view_h && idx < self.pages.len() {
            let height = self.height_of(idx);
            if offset >= height {
                // Past the end of this page, so continue into the next one
                // rather than dropping the remainder of the window.
                idx += 1;
                offset = 0;
                continue;
            }

            let take = (height - offset).min(view_h - y);
            plan.push(Cut { idx, offset, take, y });

            y += take;
            idx += 1;
            offset = 0;
        }

        plan
    }

    fn compose_strip(&self, area_w: u32, area_h: u32) -> Option<DynamicImage> {
        let first = self.images.get(&self.idx)?;
        let canvas_w = first.width().max(1);

        let view_h = ((area_h as u64 * canvas_w as u64) / area_w.max(1) as u64).max(1) as u32;
        // Guard against a pathological pane aspect asking for a gigantic canvas.
        let view_h = view_h.min(20_000);

        // Same colour the pane is filled with, so any uncovered strip blends in
        // rather than reading as a dark seam between pages.
        let mut canvas = RgbaImage::from_pixel(canvas_w, view_h, Rgba([22, 22, 30, 255]));

        for Cut { idx, offset, take, y } in self.strip_plan(view_h) {
            let Some(img) = self.images.get(&idx) else {
                continue;
            };
            let w = img.width().min(canvas_w);
            let slice = image::imageops::crop_imm(img.as_ref(), 0, offset, w, take).to_image();
            // Centred: a page narrower than the canvas left-aligned would step
            // sideways against its neighbours.
            let x = ((canvas_w - w) / 2) as i64;
            image::imageops::replace(&mut canvas, &slice, x, y as i64);
        }

        // Dropped to RGB for the same reason as the paged branch: an opaque
        // image lets the encoder skip its erase-then-paint, which is the flash.
        Some(DynamicImage::ImageRgb8(
            DynamicImage::ImageRgba8(canvas).into_rgb8(),
        ))
    }

    /// 0.0..=1.0 through the chapter, for the progress bar.
    pub fn fraction(&self) -> f64 {
        // Progress through the current chapter, not through everything loaded:
        // a continuous read appends chapters, so the denominator would
        // otherwise grow and the bar would slide backwards at each boundary.
        let (page, total) = self.chapter_span();
        if total == 0 {
            return 0.0;
        }
        let within = match self.mode {
            ViewMode::Strip => {
                let h = self.height_of(self.idx).max(1) as f64;
                (self.offset as f64 / h).clamp(0.0, 1.0)
            }
            ViewMode::Paged => 0.0,
        };
        ((page as f64 + within) / total as f64).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manga() -> MangaRef {
        MangaRef {
            source: "test".into(),
            id: "t".into(),
            title: "t".into(),
            cover: None,
            description: String::new(),
            status: String::new(),
            direction: Direction::Vertical,
        }
    }

    fn chapters(n: usize) -> Vec<Chapter> {
        (0..n)
            .map(|i| Chapter {
                id: format!("c{i}"),
                number: format!("{i}"),
                title: String::new(),
                lang: String::new(),
            })
            .collect()
    }

    fn slice(height: u32) -> Arc<DynamicImage> {
        Arc::new(DynamicImage::ImageRgb8(image::RgbImage::new(800, height)))
    }

    fn reader_with(heights: &[u32]) -> Reader {
        let mut reader = Reader::new(manga(), chapters(2), 0);
        reader.set_pages(0, heights.iter().map(|_| Page::new("x")).collect());
        for (i, h) in heights.iter().enumerate() {
            reader.insert_image(i, slice(*h));
        }
        reader
    }

    /// Absolute row where a page begins.
    fn page_start(reader: &Reader, idx: usize) -> i64 {
        (0..idx).map(|i| reader.height_of(i) as i64).sum()
    }

    /// The window must map to a contiguous run of source rows starting exactly
    /// at the current position. A gap here is content the reader never draws.
    fn assert_contiguous(reader: &Reader, view_h: u32) {
        let start = reader.absolute();
        let mut expected = start;

        for cut in reader.strip_plan(view_h) {
            let cut_start = page_start(reader, cut.idx) + cut.offset as i64;
            assert_eq!(
                cut_start, expected,
                "discontinuity at page {} (offset {})",
                cut.idx, cut.offset
            );
            expected += cut.take as i64;
        }

        let reachable = reader.total_height().min(start + view_h as i64);
        assert_eq!(expected, reachable, "window short of the rows available");
    }

    #[test]
    fn a_full_scroll_covers_every_row() {
        let mut reader = reader_with(&[900, 1500, 300, 2000, 750]);
        let view_h = 600;

        loop {
            assert_contiguous(&reader, view_h);
            let before = reader.absolute();
            reader.scroll(137);
            while reader.tick() {}
            if reader.absolute() == before {
                break;
            }
        }
        // Reaching the end means the clamp never trapped the view early.
        assert!(reader.absolute() > 5000, "scroll stopped at {}", reader.absolute());
    }

    #[test]
    fn pages_shorter_than_one_window_still_chain() {
        // Several short pages have to be stitched into a single window.
        let reader = reader_with(&[120, 90, 200, 150, 400]);
        assert_contiguous(&reader, 600);
        assert!(reader.strip_plan(600).len() >= 4);
    }

    #[test]
    fn a_late_image_keeps_the_view_in_place() {
        let mut reader = reader_with(&[1000]);
        reader.set_pages(0, (0..3).map(|_| Page::new("x")).collect());
        reader.insert_image(0, slice(1000));

        // Scroll to the bottom of the only loaded page.
        reader.scroll(900);
        while reader.tick() {}
        let before = reader.absolute();

        // The next page turns out far shorter than the estimate.
        reader.insert_image(1, slice(200));
        assert_eq!(reader.absolute(), before, "view moved when a height was corrected");
        assert_contiguous(&reader, 600);
    }

    #[test]
    fn scrolling_stops_at_the_last_loaded_page() {
        let mut reader = reader_with(&[500]);
        reader.set_pages(0, (0..4).map(|_| Page::new("x")).collect());
        reader.insert_image(0, slice(500));

        // Page 1 has not arrived, so the view must not run past page 0.
        reader.scroll(100_000);
        while reader.tick() {}
        assert!(
            reader.absolute() <= 500,
            "scrolled past the download frontier to {}",
            reader.absolute()
        );
    }

    #[test]
    fn mixed_width_pages_are_normalised_without_losing_rows() {
        let mut reader = Reader::new(manga(), chapters(2), 0);
        reader.set_pages(0, (0..3).map(|_| Page::new("x")).collect());

        // Same aspect, three different resolutions: a reader that crops to the
        // first width instead of rescaling would lose the sides of the others.
        reader.insert_image(0, Arc::new(DynamicImage::ImageRgb8(image::RgbImage::new(800, 1200))));
        reader.insert_image(1, Arc::new(DynamicImage::ImageRgb8(image::RgbImage::new(1600, 2400))));
        reader.insert_image(2, Arc::new(DynamicImage::ImageRgb8(image::RgbImage::new(400, 600))));

        for i in 0..3 {
            assert_eq!(reader.images[&i].width(), 800, "page {i} not normalised");
        }
        // Heights must follow the rescale, or scrolling walks off the page.
        assert_eq!(reader.height_of(1), 1200);
        assert_eq!(reader.height_of(2), 1200);
        assert_contiguous(&reader, 600);
    }
}
