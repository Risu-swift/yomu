//! All rendering. The app state is read here and nowhere else draws.

mod theme;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, LineGauge, List, ListItem, ListState, Paragraph, Wrap};
use ratatui_image::Image;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, HomeTab, Screen, SettingItem};
use crate::state::Chrome;
use theme::*;

pub fn draw(f: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::Home => home(f, app),
        Screen::Detail => detail(f, app),
        Screen::Reader => reader(f, app),
        Screen::Settings => settings(f, app),
    }
    if app.show_help {
        help(f, app);
    }
}

fn frame(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Span::styled(format!(" {title} "), Style::new().fg(ACCENT).bold()))
}

/// Truncate to a display width, marking the cut with an ellipsis.
fn fit(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.width() <= max {
        return text.to_string();
    }

    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Split a bar into a flexible left part and a fixed-width right part.
///
/// Rendering both halves over the same rect lets a long title overwrite the
/// status on its right, so they get their own areas instead.
fn split_bar(area: Rect, right: &str) -> (Rect, Rect) {
    let right_w = (right.width() as u16).min(area.width);
    let [l, r] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).areas(area);
    (l, r)
}

/// Top bar: where you are, which source, which graphics protocol.
fn chrome(f: &mut Frame, app: &App, area: Rect, left: &str) {
    let proto = format!("{:?}", app.picker.protocol_type()).to_lowercase();
    let right_text = format!("{} · {}  ", app.source_name(), proto);
    let (left_area, right_area) = split_bar(area, &right_text);

    let title = fit(left, left_area.width.saturating_sub(8) as usize);
    let line = Line::from(vec![
        Span::styled("  yomu ", Style::new().fg(ACCENT).bold()),
        Span::styled(title, Style::new().fg(TEXT)),
    ]);
    let right = Line::from(vec![
        Span::styled(app.source_name(), Style::new().fg(ACCENT2)),
        Span::styled(" · ", Style::new().fg(MUTED)),
        Span::styled(proto, Style::new().fg(GOOD)),
        Span::raw("  "),
    ])
    .alignment(Alignment::Right);

    f.render_widget(Paragraph::new(line).style(Style::new().bg(BAR)), left_area);
    f.render_widget(Paragraph::new(right).style(Style::new().bg(BAR)), right_area);
}

/// Status on the left, key hints on the right, each clipped to its own half.
fn status_bar(f: &mut Frame, app: &App, area: Rect, keys: &str) {
    let spinner = if app.busy { "◌ " } else { "" };
    let keys = format!("{keys} ");
    // In a narrow window the hints matter less than the status message.
    let hints = if keys.width() as u16 + 16 <= area.width { keys } else { String::new() };
    let (status_area, keys_area) = split_bar(area, &hints);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fit(&format!(" {spinner}{}", app.status), status_area.width as usize),
            Style::new().fg(TEXT),
        )))
        .style(Style::new().bg(BAR)),
        status_area,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hints, Style::new().fg(MUTED))))
            .alignment(Alignment::Right)
            .style(Style::new().bg(BAR)),
        keys_area,
    );
}

fn home(f: &mut Frame, app: &mut App) {
    let [top, body, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
            .areas(f.area());

    let tab = match app.tab {
        HomeTab::Search => "search",
        HomeTab::Library => "library",
    };
    chrome(f, app, top, tab);

    let [list_area, side] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);

    // --- search box + results
    let [query_area, results_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(list_area);

    let cursor = if app.editing { "▏" } else { "" };
    let query = if app.query.is_empty() && !app.editing {
        Span::styled("press / to search", Style::new().fg(MUTED).italic())
    } else {
        Span::styled(format!("{}{cursor}", app.query), Style::new().fg(TEXT))
    };
    // The active source goes in the box title: a switch that only changed a
    // status line was easy to miss entirely.
    let box_title = format!(
        "{} · {}  [tab to switch]",
        if app.editing { "search ⏎" } else { "search" },
        app.source_name(),
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(" ", Style::new()), query]))
            .block(frame(&box_title)),
        query_area,
    );

    let items: Vec<ListItem> = app
        .list()
        .iter()
        .map(|m| {
            let saved = if app.state.is_saved(m) { "★ " } else { "  " };
            let progress = app
                .state
                .progress_of(m)
                .map(|p| format!("  ch.{}", p.chapter_number))
                .unwrap_or_default();
            // Which source a hit came from only matters once they are mixed.
            let origin = if app.multi_source {
                format!("  [{}]", m.source)
            } else {
                String::new()
            };
            ListItem::new(Line::from(vec![
                Span::styled(saved, Style::new().fg(WARN)),
                Span::styled(m.title.clone(), Style::new().fg(TEXT)),
                Span::styled(progress, Style::new().fg(MUTED)),
                Span::styled(origin, Style::new().fg(ACCENT2)),
            ]))
        })
        .collect();

    let empty = items.is_empty();
    let mut st = ListState::default().with_selected((!empty).then_some(app.sel));
    f.render_stateful_widget(
        List::new(items)
            .block(frame(tab))
            .highlight_style(Style::new().fg(ACCENT).bg(SEL).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌"),
        results_area,
        &mut st,
    );

    sidebar(f, app, side);
    status_bar(f, app, bottom, "/ search · a all sources · ⏎ open · s save · l library · tab source · , settings · ? help");
}

/// Cover art plus metadata for whatever is highlighted.
fn sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let manga = match app.screen {
        Screen::Detail => app.detail.as_ref().map(|d| d.manga.clone()),
        _ => app.selected().cloned(),
    };
    let Some(manga) = manga else {
        f.render_widget(frame("preview"), area);
        return;
    };

    let block = frame("preview");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [cover_area, meta_area] =
        Layout::vertical([Constraint::Percentage(60), Constraint::Min(0)]).areas(inner);

    // Recorded so the event loop can encode for this exact size after the draw.
    app.cover_area = cover_area;

    // Same reason as the reader: covers differ in aspect, so a taller one
    // leaves strips of the previous cover behind without this.
    f.render_widget(Block::new().style(Style::new().bg(BG)), cover_area);

    if let Some(protocol) = &app.cover_protocol {
        f.render_widget(Image::new(protocol), cover_area);
    } else {
        f.render_widget(
            Paragraph::new("\n  loading cover…")
                .style(Style::new().fg(MUTED))
                .alignment(Alignment::Center),
            cover_area,
        );
    }

    let desc = manga
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let meta = vec![
        Line::from(Span::styled(manga.title.clone(), Style::new().fg(TEXT).bold())),
        Line::from(vec![
            Span::styled(manga.direction.label(), Style::new().fg(ACCENT2)),
            Span::styled(
                if manga.status.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", manga.status)
                },
                Style::new().fg(MUTED),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(desc, Style::new().fg(MUTED))),
    ];
    f.render_widget(
        Paragraph::new(meta).wrap(Wrap { trim: true }),
        meta_area,
    );
}

fn detail(f: &mut Frame, app: &mut App) {
    let [top, body, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
            .areas(f.area());

    let title = app
        .detail
        .as_ref()
        .map(|d| d.manga.title.clone())
        .unwrap_or_default();
    chrome(f, app, top, &title);

    let [list_area, side] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);

    let note = app.detail.as_ref().and_then(|d| d.note.clone());
    let (items, sel, loading) = match &app.detail {
        Some(d) => {
            let read_upto = app.state.progress_of(&d.manga).map(|p| p.chapter_id.clone());
            let items: Vec<ListItem> = d
                .chapters
                .iter()
                .map(|c| {
                    let marker = if Some(&c.id) == read_upto.as_ref() { "▸ " } else { "  " };
                    ListItem::new(Line::from(vec![
                        Span::styled(marker, Style::new().fg(GOOD)),
                        Span::styled(c.label(), Style::new().fg(TEXT)),
                    ]))
                })
                .collect();
            (items, d.sel, d.loading)
        }
        None => (Vec::new(), 0, false),
    };

    let label = if loading { "chapters · loading…" } else { "chapters" };

    // An empty list is usually not an error — say which reason it was.
    if let Some(why) = note.filter(|_| items.is_empty() && !loading) {
        f.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::from(Span::styled("  nothing to read here", Style::new().fg(WARN).bold())),
                Line::raw(""),
                Line::from(Span::styled(format!("  {why}"), Style::new().fg(MUTED))),
                Line::raw(""),
                Line::from(Span::styled(
                    "  try another result — licensed series are often",
                    Style::new().fg(MUTED),
                )),
                Line::from(Span::styled(
                    "  listed without any pages to fetch.",
                    Style::new().fg(MUTED),
                )),
            ])
            .block(frame(label))
            .wrap(Wrap { trim: false }),
            list_area,
        );
    } else {
        let mut st = ListState::default().with_selected((!items.is_empty()).then_some(sel));
        f.render_stateful_widget(
            List::new(items)
                .block(frame(label))
                .highlight_style(Style::new().fg(ACCENT).bg(SEL).add_modifier(Modifier::BOLD))
                .highlight_symbol("▌"),
            list_area,
            &mut st,
        );
    }

    sidebar(f, app, side);
    status_bar(f, app, bottom, "⏎ read · j/k move · g/G ends · s save · esc back · ? help");
}

fn reader(f: &mut Frame, app: &mut App) {
    let chrome_mode = app.state.chrome;

    // Anything the reader is waiting on has to be visible even when the bars
    // are hidden, or the app looks frozen with no way to tell why.
    let attention = app.busy
        || app
            .reader
            .as_ref()
            .is_some_and(|r| r.waiting_for_pages() || r.protocol.is_none());

    let title_h = u16::from(chrome_mode != Chrome::Hidden);
    let footer_h = match chrome_mode {
        Chrome::Full => 2,
        Chrome::Compact => 1,
        Chrome::Hidden => u16::from(attention),
    };

    let [top, body, bottom] = Layout::vertical([
        Constraint::Length(title_h),
        Constraint::Min(0),
        Constraint::Length(footer_h),
    ])
    .areas(f.area());

    // Recorded before borrowing the reader, so the event loop can encode for
    // this exact size after the draw completes.
    app.reader_area = body;

    let Some(reader) = app.reader.as_ref() else {
        return;
    };

    let header = match reader.chapter_ref() {
        Some(c) => format!("{} · {}", reader.manga.title, c.label()),
        None => reader.manga.title.clone(),
    };
    let chapter_label = reader
        .chapter_ref()
        .map(|c| c.label())
        .unwrap_or_else(|| reader.manga.title.clone());
    // Chapter-relative: a continuous read appends chapters onto the page list,
    // so a raw index would climb past the end of the chapter being read.
    let (page, pages) = reader.chapter_span();
    let waiting = reader.waiting_for_pages();
    let mode = reader.mode.label();
    let fraction = reader.fraction();

    // Paint the pane first. The encoder no longer erases before drawing, so a
    // cell the new image does not reach keeps showing the previous one —
    // visible as a strip down the side when rounding to whole cells leaves the
    // image slightly smaller than the pane. Filling here makes ratatui's own
    // diff repaint exactly those cells, and leaves the image's own cells alone
    // because the protocol marks them as skipped.
    f.render_widget(Block::new().style(Style::new().bg(BG)), body);

    // The previous frame stays on screen while the next one encodes, which is
    // what keeps scrolling from flickering.
    if let Some(protocol) = &reader.protocol {
        f.render_widget(Image::new(protocol), body);
    } else {
        let msg = if pages == 0 {
            "fetching page list…"
        } else {
            "downloading page…"
        };
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::new().fg(MUTED))
                .alignment(Alignment::Center)
                .block(Block::new()),
            centered(body, 40, 1),
        );
    }

    if title_h > 0 {
        chrome(f, app, top, &header);
    }
    if footer_h == 0 {
        return;
    }

    let position = format!(
        " {}/{} · {mode}{} ",
        (page + 1).min(pages.max(1)),
        pages.max(1),
        // Scrolling stops at the last page that has arrived, so say so rather
        // than letting it look frozen.
        if waiting { " · loading" } else { "" },
    );

    // One row in the hidden case: just enough to explain the pause.
    if chrome_mode == Chrome::Hidden {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit(&format!(" {}", app.status), bottom.width as usize),
                Style::new().fg(MUTED),
            )))
            .style(Style::new().bg(BAR)),
            bottom,
        );
        return;
    }

    let [gauge_row, label_row] = match chrome_mode {
        Chrome::Full => {
            let [a, b] =
                Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(bottom);
            [a, b]
        }
        _ => [bottom, Rect::ZERO],
    };

    // Drop hints progressively rather than letting them be cut mid-word.
    let hints = [
        "j/k scroll · space page · n/p chapter · v mode · f bars · esc back ",
        "j/k · space · n/p · v · f · esc ",
        "",
    ]
    .into_iter()
    .find(|h| h.width() as u16 + 24 <= bottom.width)
    .unwrap_or("");

    // Compact shares one row with the hints; full gives the gauge the row.
    let gauge_area = if chrome_mode == Chrome::Full {
        gauge_row
    } else {
        split_bar(gauge_row, hints).0
    };

    f.render_widget(
        LineGauge::default()
            .filled_style(Style::new().fg(ACCENT))
            .unfilled_style(Style::new().fg(BORDER))
            .label(Span::styled(position, Style::new().fg(MUTED)))
            .ratio(fraction),
        gauge_area,
    );

    let hint_area = if chrome_mode == Chrome::Full {
        // Second row: what is being read on the left, keys on the right.
        let (left, right) = split_bar(label_row, hints);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit(&format!(" {chapter_label}"), left.width as usize),
                Style::new().fg(TEXT),
            )))
            .style(Style::new().bg(BAR)),
            left,
        );
        right
    } else {
        split_bar(gauge_row, hints).1
    };

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hints, Style::new().fg(MUTED))))
            .alignment(Alignment::Right)
            .style(Style::new().bg(BAR)),
        hint_area,
    );
}

fn settings(f: &mut Frame, app: &mut App) {
    let [top, body, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
            .areas(f.area());
    chrome(f, app, top, "settings");

    let mut rows: Vec<ListItem> = Vec::new();
    for (i, item) in app.settings_items().iter().enumerate() {
        let (on, label, note) = match item {
            SettingItem::AllowNsfw => (
                app.state.allow_nsfw,
                "Show adult content".to_string(),
                "applies only to sources that carry a rating".to_string(),
            ),
            SettingItem::Source(name) => {
                let supports = app
                    .registry
                    .sources
                    .get(i.saturating_sub(1))
                    .is_some_and(|s| s.supports_nsfw_filter());
                (
                    app.is_enabled(name),
                    name.clone(),
                    if supports {
                        "content filter supported".to_string()
                    } else {
                        "no content filter — shows everything it finds".to_string()
                    },
                )
            }
        };

        rows.push(ListItem::new(Line::from(vec![
            Span::styled(
                if on { "  [x] " } else { "  [ ] " },
                Style::new().fg(if on { GOOD } else { MUTED }),
            ),
            Span::styled(format!("{label:<28}"), Style::new().fg(TEXT)),
            Span::styled(note, Style::new().fg(MUTED)),
        ])));
    }

    let mut st = ListState::default().with_selected(Some(app.settings_sel));
    f.render_stateful_widget(
        List::new(rows)
            .block(frame("settings"))
            .highlight_style(Style::new().fg(ACCENT).bg(SEL).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌"),
        body,
        &mut st,
    );

    status_bar(f, app, bottom, "space toggle · j/k move · esc back");
}

fn help(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 64, 22);
    f.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled("keys", Style::new().fg(ACCENT).bold())),
        Line::raw(""),
        key_line("/", "search the current source"),
        key_line("tab", "cycle source (built-in + TOML plugins)"),
        key_line("a", "search every enabled source at once"),
        key_line("l", "switch between search results and library"),
        key_line("s", "add/remove from library"),
        key_line("⏎", "open series · start reading"),
        key_line("j k", "move · scroll the strip"),
        key_line("space b", "page down / up"),
        key_line("n p", "next / previous chapter"),
        key_line("v", "toggle paged ↔ strip view"),
        key_line("f", "reader bars: full ↔ compact ↔ hidden"),
        key_line(",", "settings"),
        key_line("esc", "back · q quit"),
        Line::raw(""),
        Line::from(Span::styled(
            format!(
                "rendering: {:?} at {}x{}px cells — override with --protocol / --font-size",
                app.picker.protocol_type(),
                app.picker.font_size().width,
                app.picker.font_size().height,
            ),
            Style::new().fg(MUTED),
        )),
        Line::from(Span::styled(
            format!("plugins: {}", crate::net::config_dir().join("sources").display()),
            Style::new().fg(MUTED),
        )),
    ];

    if !app.registry.errors.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("plugin errors", Style::new().fg(WARN).bold())));
        for e in &app.registry.errors {
            lines.push(Line::from(Span::styled(e.clone(), Style::new().fg(WARN))));
        }
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(frame("help"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn key_line<'a>(key: &'a str, what: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {key:<8}"), Style::new().fg(ACCENT2).bold()),
        Span::styled(what, Style::new().fg(TEXT)),
    ])
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [h] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [v] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(h);
    v
}
