# Modifications

A local copy of `ratatui-image` 11.0.8, patched in via `[patch.crates-io]` in
the workspace `Cargo.toml`. Two changes, both in `src/protocol/iterm2.rs`, both
about making a scrolling reader usable.

## 1. Erase only when the image can be seen through

The iTerm2 encoder emitted `clear_area` before every image: an `ESC[<width>X`
per row, blanking the region and then painting it. That is one visible flash per
redraw, and a scrolling reader redraws constantly.

The erase exists so stale characters cannot show through transparent parts of an
image, since the cells under it are marked skip. Nothing can show through an
opaque image, so the call is now conditional.

The test is `has_transparency`, which scans the alpha bytes rather than asking
`color().has_alpha()`. The latter is not usable here: the resize step pads a
scaled image onto an RGBA canvas whenever it does not land exactly on a cell
boundary, so nearly every image arrives carrying an alpha channel even when
every pixel in it is opaque.

For this to help, the caller also has to supply opaque images and set an opaque
`background_color` on the `Picker`, otherwise the letterbox padding is itself
transparent and the erase is genuinely required.

## 2. Fast PNG compression

`write_to(.., ImageFormat::Png)` uses default compression, which dominates the
cost of producing a frame. It now uses `CompressionType::Fast`, which is still
lossless and several times quicker, in exchange for a larger payload that only
travels down a pty rather than a network.

## Upstreaming

Both are plausibly useful upstream, the first more than the second — a caller
that wants the erase for transparent images still gets it. Neither has been
submitted.
