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

pub struct Reader {
    pub manga: MangaRef,
    pub chapters: Vec<Chapter>,
    pub chapter: usize,
    pub pages: Vec<Page>,
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
            images: HashMap::new(),
            idx: 0,
            offset: 0,
            mode,
            protocol: None,
            generation: 0,
            target: 0,
            encoded_fast: false,
            key: None,
        }
    }

    pub fn chapter_ref(&self) -> Option<&Chapter> {
        self.chapters.get(self.chapter)
    }

    pub fn set_pages(&mut self, pages: Vec<Page>) {
        self.pages = pages;
        self.images.clear();
        self.idx = 0;
        self.offset = 0;
        self.target = 0;
        // A different chapter entirely, so holding the old frame would show the
        // wrong page rather than hide a flicker.
        self.protocol = None;
        self.invalidate();
    }

    pub fn invalidate(&mut self) {
        self.key = None;
    }

    pub fn insert_image(&mut self, idx: usize, img: Arc<DynamicImage>) {
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
        let limit = (self.total_height() - 1).max(0);
        self.target = (self.target + delta).clamp(0, limit);
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
        self.sync_target();
        self.invalidate();
    }

    pub fn go_end(&mut self) {
        self.idx = self.pages.len().saturating_sub(1);
        self.offset = 0;
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

    /// Move immediately to an absolute position, walking across slice bounds.
    fn jump(&mut self, position: i64) {
        let mut idx = self.idx;
        let mut offset = position - (self.absolute() - self.offset as i64);

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
            self.invalidate();
        }
    }

    fn height_of(&self, idx: usize) -> u32 {
        self.images.get(&idx).map(|i| i.height()).unwrap_or(1600)
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

    /// Accept a finished encode, ignoring one the view has already moved past.
    pub fn accept(&mut self, generation: u64, protocol: Protocol) {
        if generation == self.generation {
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
    fn compose_strip(&self, area_w: u32, area_h: u32) -> Option<DynamicImage> {
        let first = self.images.get(&self.idx)?;
        let canvas_w = first.width().max(1);

        let view_h = ((area_h as u64 * canvas_w as u64) / area_w.max(1) as u64).max(1) as u32;
        // Guard against a pathological pane aspect asking for a gigantic canvas.
        let view_h = view_h.min(20_000);

        let mut canvas = RgbaImage::from_pixel(canvas_w, view_h, Rgba([12, 12, 16, 255]));
        let mut y = 0u32;
        let mut idx = self.idx;
        let mut offset = self.offset;

        while y < view_h {
            let Some(img) = self.images.get(&idx) else {
                break; // Not downloaded yet: leave the rest of the window blank.
            };
            let h = img.height();
            if offset >= h {
                idx += 1;
                offset = 0;
                continue;
            }

            let take = (h - offset).min(view_h - y);
            let w = img.width().min(canvas_w);
            let slice = image::imageops::crop_imm(img.as_ref(), 0, offset, w, take).to_image();
            image::imageops::replace(&mut canvas, &slice, 0, y as i64);

            y += take;
            idx += 1;
            offset = 0;
        }

        // Dropped to RGB for the same reason as the paged branch: an opaque
        // image lets the encoder skip its erase-then-paint, which is the flash.
        Some(DynamicImage::ImageRgb8(
            DynamicImage::ImageRgba8(canvas).into_rgb8(),
        ))
    }

    /// 0.0..=1.0 through the chapter, for the progress bar.
    pub fn fraction(&self) -> f64 {
        if self.pages.is_empty() {
            return 0.0;
        }
        let per = 1.0 / self.pages.len() as f64;
        let within = match self.mode {
            ViewMode::Strip => {
                let h = self.height_of(self.idx).max(1) as f64;
                (self.offset as f64 / h).clamp(0.0, 1.0)
            }
            ViewMode::Paged => 0.0,
        };
        ((self.idx as f64 + within) * per).clamp(0.0, 1.0)
    }
}
