//! Command line handling and the headless `--probe` diagnostic.
//!
//! Protocol detection needs an override on Windows: the ConPTY layer strips
//! the escape sequences that the capability query relies on, so the reply never
//! arrives and detection falls back to half-blocks even when the terminal
//! itself can render images.

use anyhow::Result;
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};

use crate::source::Registry;

#[derive(Debug, Default)]
pub struct Args {
    pub protocol: Option<ProtocolType>,
    pub font_size: Option<(u16, u16)>,
    pub probe: Option<String>,
    /// Restrict --probe to sources whose name contains this.
    pub source: Option<String>,
    pub bench: bool,
    pub help: bool,
}

pub const HELP: &str = "\
yomu — terminal manga and manhwa reader

USAGE:
    yomu [OPTIONS]

OPTIONS:
    --protocol <NAME>    force the graphics protocol instead of detecting it:
                         kitty | sixel | iterm2 | halfblocks | auto
                         (env: YOMU_PROTOCOL)
    --font-size <WxH>    cell size in pixels, e.g. 8x19. Needed when forcing a
                         protocol, since detection normally supplies it.
                         (env: YOMU_FONT_SIZE)
    --probe [QUERY]      run search, chapter and page lookups against every
                         source and print what came back, then exit. No TUI.
    --source <NAME>      limit --probe to sources matching NAME, so one site
                         can be checked without querying all of them.
    -h, --help           show this help

NOTES:
    On Windows the ConPTY layer strips the sequences that protocol detection
    depends on. If the status bar says `halfblocks` in a terminal you know
    supports images, try:  yomu --protocol kitty --font-size 8x19
";

fn parse_protocol(s: &str) -> Option<ProtocolType> {
    match s.to_ascii_lowercase().as_str() {
        "kitty" => Some(ProtocolType::Kitty),
        "sixel" => Some(ProtocolType::Sixel),
        "iterm2" | "iterm" => Some(ProtocolType::Iterm2),
        "halfblocks" | "blocks" => Some(ProtocolType::Halfblocks),
        _ => None,
    }
}

fn parse_font_size(s: &str) -> Option<(u16, u16)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

impl Args {
    pub fn parse() -> Self {
        let mut args = Args {
            protocol: std::env::var("YOMU_PROTOCOL")
                .ok()
                .as_deref()
                .and_then(parse_protocol),
            font_size: std::env::var("YOMU_FONT_SIZE")
                .ok()
                .as_deref()
                .and_then(parse_font_size),
            ..Default::default()
        };

        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-h" | "--help" => args.help = true,
                "--protocol" => args.protocol = it.next().as_deref().and_then(parse_protocol),
                "--font-size" => args.font_size = it.next().as_deref().and_then(parse_font_size),
                "--bench" => args.bench = true,
                "--source" => args.source = it.next(),
                "--probe" => {
                    // The query is optional; without one the source browses.
                    args.probe = Some(it.next().unwrap_or_default());
                }
                _ => {}
            }
        }
        args
    }

    /// Build a picker, honouring any override.
    ///
    /// `from_query_stdio` writes a capability query and blocks reading the
    /// reply. With no terminal on the other end — a pipe, a redirect, `--probe`
    /// — that reply never comes and it hangs forever, so it is only ever called
    /// when there is a real terminal and no override to make it unnecessary.
    pub fn picker(&self) -> (Picker, Option<String>) {
        // An explicit choice skips the query entirely, which is also what makes
        // `--probe` usable when there is no terminal to answer it.
        if let Some(forced) = self.protocol {
            let font = self.font_size.or_else(terminal_cell_size);
            let note = (font.is_none()).then(|| guess_note(FALLBACK_CELL));
            return (build_picker(font.unwrap_or(FALLBACK_CELL), forced), note);
        }

        if !is_interactive() {
            return (Picker::halfblocks(), None);
        }

        let detected = Picker::from_query_stdio().unwrap_or_else(|_| {
            eprintln!("no graphics protocol detected - falling back to half-blocks");
            Picker::halfblocks()
        });

        let found = detected.protocol_type();
        // Half-blocks means the capability query went unanswered, which also
        // makes the reported cell size the library's 10x20 default rather than
        // a measurement. Any other result means the terminal replied, so its
        // cell size is real and must be preserved.
        let detection_worked = found != ProtocolType::Halfblocks;

        // Use iTerm2 in WezTerm, not kitty.
        //
        // WezTerm renders kitty graphics when the image is transmitted for
        // immediate display, but it does not implement the unicode-placeholder
        // variant of the protocol, which is how this crate anchors an image to
        // a rectangle of cells. The placeholders (U+10EEEE plus diacritics)
        // then reach the screen as literal missing-glyph boxes and fill the
        // pane with tofu. iTerm2 is the protocol that actually works here.
        let protocol = if in_wezterm() && found == ProtocolType::Halfblocks {
            ProtocolType::Iterm2
        } else {
            found
        };

        // An explicit size always wins; otherwise keep a measured one, and only
        // fall back to a guess when detection never got an answer.
        if let Some(font) = self.font_size {
            return (build_picker(font, protocol), None);
        }
        if protocol == found {
            return (detected, None);
        }
        if detection_worked {
            let f = detected.font_size();
            return (build_picker((f.width, f.height), protocol), None);
        }
        match terminal_cell_size() {
            Some(font) => (build_picker(font, protocol), None),
            None => (
                build_picker(FALLBACK_CELL, protocol),
                Some(guess_note(FALLBACK_CELL)),
            ),
        }
    }
}

/// A plausible cell size for a ~10pt monospace font at 100% scaling, used only
/// when the terminal will not report its own. Wrong by a pixel or two is fine;
/// wrong by 25% is what makes images overflow their pane.
const FALLBACK_CELL: (u16, u16) = (8, 18);

fn guess_note((w, h): (u16, u16)) -> String {
    format!("cell size guessed at {w}x{h}px — if images look stretched, pass --font-size WxH")
}

/// Build a picker with a known cell size and protocol.
///
/// `from_fontsize` is deprecated in favour of detection, but detection is
/// exactly what is being overridden here: it is the only way to construct a
/// picker when the terminal cannot or will not answer the query.
fn build_picker((width, height): (u16, u16), protocol: ProtocolType) -> Picker {
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(FontSize::new(width.max(1), height.max(1)));
    picker.set_protocol_type(protocol);
    picker
}

/// True only when both ends of stdio are a real terminal.
fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// WezTerm sets these for every pane it spawns, on every platform.
fn in_wezterm() -> bool {
    std::env::var_os("WEZTERM_PANE").is_some()
        || std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "WezTerm")
}

/// Ask the terminal for its pixel dimensions and divide by the grid.
fn terminal_cell_size() -> Option<(u16, u16)> {
    let ws = ratatui::crossterm::terminal::window_size().ok()?;
    if ws.width == 0 || ws.height == 0 || ws.columns == 0 || ws.rows == 0 {
        return None;
    }
    Some((ws.width / ws.columns, ws.height / ws.rows))
}

/// Time the encode step, which is what caps the scroll frame rate.
///
/// Composing a strip window is a memcpy; resizing and compressing it is not.
/// Printing the split tells us whether smoother scrolling needs a cheaper
/// filter, a smaller payload, or something else entirely.
pub fn bench(picker: &Picker) {
    use std::time::Instant;

    let font = picker.font_size();
    println!(
        "protocol {:?}, cell {}x{}px\n",
        picker.protocol_type(),
        font.width,
        font.height
    );

    for (cols, rows) in [(80u16, 24u16), (120, 40), (160, 50)] {
        let w = cols as u32 * font.width.max(1) as u32;
        let h = rows as u32 * font.height.max(1) as u32;

        // A gradient rather than a flat fill: flat colour compresses to almost
        // nothing and would flatter the encoder into a useless number.
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        }));

        let size = ratatui::layout::Size::new(cols, rows);
        let runs = 5;
        let start = Instant::now();
        let mut bytes = 0;
        for _ in 0..runs {
            match picker.new_protocol(image.clone(), size, crate::app::scaling()) {
                Ok(_) => bytes += 1,
                Err(e) => {
                    println!("  {cols}x{rows}: FAILED: {e}");
                    break;
                }
            }
        }
        if bytes == 0 {
            continue;
        }

        let per = start.elapsed().as_secs_f64() * 1000.0 / runs as f64;
        println!(
            "  {cols:>3}x{rows:<3} cells = {w:>4}x{h:<4}px   {per:>6.1} ms/frame   {:.0} fps",
            1000.0 / per
        );
    }
}

/// Exercise every source end to end and print what came back.
pub async fn probe(
    query: &str,
    only: Option<&str>,
    registry: &Registry,
    picker: &Picker,
) -> Result<()> {
    let font = picker.font_size();
    println!(
        "protocol: {:?}   cell size: {}x{}px",
        picker.protocol_type(),
        font.width,
        font.height
    );
    match terminal_cell_size() {
        Some((w, h)) => println!("terminal reports cell size: {w}x{h}px"),
        None => println!("terminal reports cell size: unavailable (ConPTY strips the query)"),
    }

    if !registry.errors.is_empty() {
        println!("\nplugin load errors:");
        for e in &registry.errors {
            println!("  {e}");
        }
    }

    let wanted = only.map(|s| s.to_lowercase());
    for src in &registry.sources {
        // Checking one site should not mean querying every configured one.
        if let Some(want) = &wanted {
            if !src.name().to_lowercase().contains(want.as_str()) {
                continue;
            }
        }
        println!("\n=== {} ===", src.name());

        let results = match src.search(query).await {
            Ok(r) => r,
            Err(e) => {
                println!("  search FAILED: {e:#}");
                continue;
            }
        };
        println!("  search({query:?}): {} results", results.len());

        // Walk the top results rather than only the first: licensed series are
        // commonly listed with no readable pages, and stopping at result #1
        // makes a working source look broken.
        let mut readable = None;
        for manga in results.iter().take(5) {
            match src.chapters(manga).await {
                Ok(c) => {
                    let detail = match c.explain_empty() {
                        Some(why) => format!("0 readable — {why}"),
                        None if c.external > 0 => {
                            format!("{} readable ({} external)", c.items.len(), c.external)
                        }
                        None => format!("{} readable", c.items.len()),
                    };
                    println!("  [{}] {}", detail, manga.title);
                    if readable.is_none() && !c.items.is_empty() {
                        readable = Some((manga.clone(), c.items));
                    }
                }
                Err(e) => println!("  [FAILED: {e:#}] {}", manga.title),
            }
        }

        let Some((manga, chapters)) = readable else {
            println!("  nothing readable in the top 5 results");
            continue;
        };

        println!("\n  reading test: {} [{}]", manga.title, manga.direction.label());
        println!("     cover: {}", manga.cover.as_deref().unwrap_or("none"));
        let chapter = &chapters[0];
        println!("     chapter: {}", chapter.label());

        match src.pages(&manga, chapter).await {
            Ok(pages) => {
                println!("     pages: {}", pages.len());
                if let Some(p) = pages.first() {
                    println!("     first page: {}", p.url);
                }
            }
            Err(e) => println!("     pages FAILED: {e:#}"),
        }
    }

    Ok(())
}
