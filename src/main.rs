mod app;
mod cli;
mod model;
mod net;
mod reader;
mod source;
mod state;
mod ui;

use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui_image::picker::Picker;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use app::App;
use source::Registry;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();
    if args.help {
        print!("{}", cli::HELP);
        return Ok(());
    }

    // Probe the terminal before taking over the screen: the query writes an
    // escape sequence to stdout and reads the reply back off stdin.
    let (mut picker, note) = args.picker();
    // Letterbox padding around a scaled image is transparent by default, which
    // counts as transparency and so forces the encoder's erase-before-draw —
    // the flicker. An opaque fill matching the terminal background is visually
    // identical and lets the erase be skipped.
    picker.set_background_color(Some(image::Rgba([22, 22, 30, 255])));

    write_example_plugin();
    let registry = Registry::load();

    if args.bench {
        cli::bench(&picker);
        return Ok(());
    }

    if let Some(query) = &args.probe {
        return cli::probe(query, args.source.as_deref(), &registry, &picker).await;
    }

    let terminal = ratatui::init();
    let result = run(terminal, picker, registry, note).await;
    ratatui::restore();
    result
}

/// Read terminal events on a dedicated thread.
///
/// `EventStream` hands out a fresh future per poll, so draining "everything
/// queued right now" means creating and dropping futures in a loop — and a
/// dropped pending read can take its event with it and leave the stream silent
/// for good. A blocking reader on its own thread has none of that subtlety,
/// and its channel can be drained safely with `try_recv`.
fn spawn_event_reader() -> UnboundedReceiver<Event> {
    let (tx, rx) = unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if tx.send(event).is_err() {
                break; // The app is gone.
            }
        }
    });
    rx
}

async fn run(
    mut terminal: DefaultTerminal,
    picker: Picker,
    registry: Registry,
    note: Option<String>,
) -> Result<()> {
    let (mut app, mut rx) = App::new(picker, registry);
    if let Some(note) = note {
        app.status = note;
    }
    let mut events = spawn_event_reader();
    // Drives scroll easing. Ticking costs nothing when nothing is animating,
    // because a tick that does not move the view does not trigger a redraw.
    let mut frames = tokio::time::interval(Duration::from_millis(16));
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    terminal.draw(|f| ui::draw(f, &mut app))?;
    app.refresh_images();

    loop {
        let mut dirty = true;
        tokio::select! {
            Some(event) = events.recv() => apply_event(&mut app, event),
            Some(msg) = rx.recv() => app.handle(msg),
            _ = frames.tick() => dirty = app.animate(),
            else => break,
        }

        // Key auto-repeat arrives far faster than a frame can be encoded, so
        // apply everything already waiting before drawing. A burst of repeats
        // then becomes one larger scroll and one encode, instead of a queue of
        // encodes the display can only catch up on after the key is released.
        while let Ok(event) = events.try_recv() {
            apply_event(&mut app, event);
            dirty = true;
        }
        while let Ok(msg) = rx.try_recv() {
            app.handle(msg);
            dirty = true;
        }

        if app.quit {
            break;
        }
        if !dirty {
            continue;
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
        // Encoding is driven from here using the rectangles the draw recorded.
        app.refresh_images();
    }

    Ok(())
}

fn apply_event(app: &mut App, event: Event) {
    match event {
        Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
        Event::Resize(_, _) => {
            // Everything encoded for the old size is now wrong.
            if let Some(r) = app.reader.as_mut() {
                r.invalidate();
            }
        }
        _ => {}
    }
}

/// Drop a documented sample plugin next to the real ones on first run, so the
/// format is discoverable without reading the source.
fn write_example_plugin() {
    let path = net::config_dir().join("sources").join("example.toml.sample");
    if path.exists() {
        return;
    }
    let _ = std::fs::write(&path, include_str!("../sources/example.toml.sample"));
}
