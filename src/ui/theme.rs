//! One palette, defined once. Every colour in the UI comes from here.

use ratatui::style::Color;

/// Fill behind images. Matches the `background_color` given to the picker, so
/// the letterbox padding inside an image and the cells around it agree.
pub const BG: Color = Color::Rgb(22, 22, 30);

pub const ACCENT: Color = Color::Rgb(122, 162, 247);
pub const ACCENT2: Color = Color::Rgb(187, 154, 247);
pub const TEXT: Color = Color::Rgb(205, 214, 244);
pub const MUTED: Color = Color::Rgb(122, 132, 160);
pub const BORDER: Color = Color::Rgb(60, 66, 90);
pub const BAR: Color = Color::Rgb(26, 28, 40);
pub const SEL: Color = Color::Rgb(40, 44, 66);
pub const GOOD: Color = Color::Rgb(158, 206, 106);
pub const WARN: Color = Color::Rgb(224, 175, 104);
