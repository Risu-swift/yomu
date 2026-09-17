# yomu

A terminal manga and manhwa reader. Real images in the terminal, continuous
scrolling for webtoons, and sources you can add yourself with a TOML file.

## Requirements

A terminal with a graphics protocol. In order of quality:

| Terminal | Protocol | Notes |
|---|---|---|
| WezTerm, Kitty, Ghostty | kitty | best; the one to use on Windows |
| iTerm2 | iterm2 | macOS |
| Windows Terminal ≥ 1.22, foot | sixel | good |
| anything else | half-blocks | blocky but usable, always works |

Detection is automatic at startup and the active protocol is shown in the top
right corner. Press `?` to see the protocol and cell size in use.

### Windows notes

**Use a current WezTerm.** The 20240203 stable release does not render the kitty
protocol here at all — not in a native pane, and not in a WSL pane either, so
ConPTY is not the explanation. A 2026 nightly renders both kitty and iTerm2
from the same unchanged config. If images do not appear, check the version
before debugging anything else.

**Detection can fail while rendering works.** The reply to the capability query
does not reliably survive the trip back on Windows, so detection lands on
half-blocks in a terminal that draws images perfectly well. yomu notices WezTerm
via `WEZTERM_PANE` and upgrades half-blocks to kitty.

**In WezTerm, use iTerm2 rather than kitty.** WezTerm renders kitty graphics
when an image is transmitted for immediate display — a raw test image appears
fine — but it does not implement the *unicode-placeholder* variant, which is how
this crate anchors an image to a rectangle of cells. Forcing `--protocol kitty`
therefore fills the pane with tofu: the placeholder characters (U+10EEEE and
combining diacritics) reach the screen as literal missing glyphs.

So "the raw protocol works" does not imply "the app can use it". Test with the
application, not only with `tools/kitty-test.py`.

**Cell size has to be told, not asked.** A failed query leaves the library's
10x20 default in place. If that is wrong, every image is encoded oversized and
overflows its pane. For JetBrains Mono at 10.5pt and 100% scaling the answer is
`8x18` — 0.6em advance, 1.32em line height, 14px em:

```
yomu --protocol kitty --font-size 8x18
```

Press `?` in the app to see the protocol and cell size actually in use.

Two tools help work out what a terminal supports: `tools/image-test.ps1` tries
each protocol under a labelled banner, and `tools/kitty-test.py` emits a kitty
test image with no dependencies beyond python3.

## Run

```
cargo run --release
```

## Keys

| | |
|---|---|
| `/` | search the current source |
| `tab` | cycle sources |
| `l` | switch between search results and library |
| `s` | add / remove from library |
| `⏎` | open series, then start reading |
| `j` `k` | move, or scroll the strip |
| `space` `b` | page down / up |
| `←` `→` | previous / next page, respecting reading direction |
| `n` `p` | next / previous chapter |
| `v` | toggle paged ↔ strip view |
| `esc` | back · `q` quit · `?` help |

Reading position is saved per series — chapter, page, and pixel offset within a
strip — so reopening resumes exactly where you stopped.

## How it reads

Two view modes share one pipeline:

- **Paged** — one image per screen. Arrow keys follow the series' reading
  direction, so `←` advances a right-to-left manga.
- **Strip** — a webtoon chapter is one continuous canvas. The visible window is
  stitched from consecutive slices at native resolution and sized so that
  scaling it to the pane width lands its height exactly on the pane height.
  Without that, fitting would letterbox the strip and scrolling would crawl.

Three things keep it responsive: pages are prefetched three ahead and one
behind, decode and resize+encode run on the blocking pool rather than the UI
task, and the encoded view is rebuilt only when what should be on screen
actually changed.

## Sources

**MangaDex** is built in — it publishes a documented API and permits third-party
clients.

Everything else is a plugin: a TOML file in the config directory, listed in the
help screen (`?`). Most sites are a URL template plus a handful of CSS
selectors, so no scripting is involved:

```toml
name = "Example"
base = "https://example.test"
direction = "vertical"          # vertical | rtl | ltr

[search]
url = "/search?q={query}"       # {query} is url-encoded
item = ".manga-card"
title = "h3 a"
link = "h3 a@href"
cover = "img@data-src"

[chapters]
item = "#chapter-list li"
link = "a@href"
number = ".chapter-number"
reverse = true                  # sites list newest first

[pages]
item = "#viewer img@src"
```

A selector is `sel` for the text of the first match, `sel@attr` for one of its
attributes, or a bare `@attr` to read off the matched item itself. Relative
links resolve against `base`. Bad selectors are reported at startup rather than
failing silently mid-search.

Sites whose reader is built in JavaScript have no `<img>` to select: the page
list lives in a script variable. `[pages]` takes a regex instead, run over the
raw HTML, with `split` cutting one capture into several URLs:

```toml
[pages]
regex = 'var chapImages\s*=\s*"([^"]+)"'
split = ","
```

Capture group 1 holds the URLs, or the whole match when the pattern has no
group. Without `split`, every match contributes one page. `item` and `regex`
are mutually exclusive and one of them is required, both checked at load time.

A plugin says how its site expresses a content filter, and `{nsfw}` in any URL
template is replaced by whichever side the settings toggle is on. Adult content
is off unless deliberately turned on, so `off` is what a fresh install uses:

```toml
[search]
url = "/search?q={query}{nsfw}"

[nsfw]
on = ""
off = "&rating=safe"
```

Sites spell this too differently for a fixed parameter name — one wants
`adult=0`, another a list of `exclude[]` genres — so the plugin supplies both
strings. Omit the section entirely when the site has no such parameter: the
toggle then reports itself as inapplicable for that source rather than
implying it filters something.

`example.toml.sample` is written into the sources directory on first run; copy
it to `example.toml` and edit.

Plugins themselves are not tracked here, and `.gitignore` keeps them out. A
plugin points at one specific site, which makes it configuration chosen by
whoever runs the app rather than part of the program — and which sites are
acceptable to fetch from is not a decision this repository makes on anyone's
behalf. Point plugins at sites whose terms permit it. Nothing ships enabled.

`yomu --probe "<query>"` runs search, chapters and pages against every enabled
source and prints what came back. It is the fastest way to tell a broken
selector from a site that simply has no results.

## Layout

```
src/
  model.rs        MangaRef / Chapter / Page, reading direction
  net.rs          shared HTTP client, on-disk image cache
  source/
    mod.rs        Source trait + plugin registry
    mangadex.rs   built-in source
    declarative.rs  TOML scraper engine
  reader.rs       page cache, strip composition, protocol lifecycle
  app.rs          state, key handling, background tasks
  ui/             rendering and palette
  state.rs        library + reading progress (JSON)
```

Adding a source means implementing three methods:

```rust
trait Source {
    async fn search(&self, query: &str) -> Result<Vec<MangaRef>>;
    async fn chapters(&self, manga: &MangaRef) -> Result<Vec<Chapter>>;
    async fn pages(&self, manga: &MangaRef, chapter: &Chapter) -> Result<Vec<Page>>;
}
```

## Not there yet

- Lua escape hatch for sites that need real logic (signed URLs, JS-rendered
  page lists). The `Source` trait is the seam it will plug into.
- CBZ / local folder reading.
- Download for offline.
