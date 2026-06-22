//! Color themes. Every color the UI draws comes from a [`Theme`], so the look
//! is swappable at runtime (`/theme`) and can adapt to the terminal palette.
//!
//! The `terminal` theme leans on ANSI named colors and `Color::Reset` (the
//! terminal's own default foreground) and uses reverse-video for selection /
//! mentions, so it stays legible on any background — including when you select
//! text with the mouse.

use ratatui::style::Color;

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub accent: Color,
    /// Default message body text.
    pub text: Color,
    /// Emphasis (active labels, your own messages, card titles).
    pub bright: Color,
    /// Timestamps, borders, system lines, footer.
    pub dim: Color,
    /// Foreground used on top of an `accent` background (header bar).
    pub header_fg: Color,
    pub sel_bg: Color,
    /// Use reverse-video for the selected message instead of `sel_bg`.
    pub sel_reverse: bool,
    pub mention_bg: Color,
    pub mention_reverse: bool,
    pub badge_bg: Color,
    pub badge_fg: Color,
    pub statusbar_bg: Color,
    pub card_desc: Color,
    /// Joins, online presence, voice (+).
    pub good: Color,
    /// Notices, away, halfop (%), the react chip.
    pub warn: Color,
    /// Errors, operator prefix (@), mention counts.
    pub bad: Color,
    /// Actions (`/me`), the reply chip.
    pub special: Color,
    /// Parts / quits.
    pub part: Color,
    /// Palette nicks are hashed into.
    pub nicks: Vec<Color>,
}

impl Theme {
    pub fn by_name(name: &str) -> Theme {
        match name.to_ascii_lowercase().as_str() {
            "light" => light(),
            "terminal" | "auto" | "ansi" => terminal(),
            "nord" => nord(),
            _ => dark(),
        }
    }

    pub const NAMES: &'static [&'static str] = &["dark", "light", "terminal", "nord"];

    pub fn nick(&self, nick: &str) -> Color {
        let mut h: u32 = 2166136261;
        for b in nick.bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(16777619);
        }
        self.nicks[(h as usize) % self.nicks.len()]
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// The default: a soft dark palette (the original irkt look).
pub fn dark() -> Theme {
    Theme {
        name: "dark",
        accent: rgb(0x7a, 0xa2, 0xf7),
        text: rgb(0xc0, 0xc5, 0xce),
        bright: Color::White,
        dim: rgb(0x6c, 0x70, 0x86),
        header_fg: Color::Black,
        sel_bg: rgb(0x2d, 0x33, 0x44),
        sel_reverse: false,
        mention_bg: rgb(0x3a, 0x2f, 0x1a),
        mention_reverse: false,
        badge_bg: rgb(0x2a, 0x2e, 0x3a),
        badge_fg: Color::White,
        statusbar_bg: rgb(0x16, 0x18, 0x22),
        card_desc: rgb(0x9a, 0x9f, 0xb0),
        good: rgb(0x9e, 0xce, 0x6a),
        warn: rgb(0xe0, 0xaf, 0x68),
        bad: rgb(0xf7, 0x76, 0x8e),
        special: rgb(0xbb, 0x9a, 0xf7),
        part: rgb(0xb0, 0x60, 0x60),
        nicks: vec![
            rgb(0xf7, 0x76, 0x8e), rgb(0x9e, 0xce, 0x6a), rgb(0xe0, 0xaf, 0x68),
            rgb(0x7a, 0xa2, 0xf7), rgb(0xbb, 0x9a, 0xf7), rgb(0x7d, 0xcf, 0xff),
            rgb(0xff, 0xa5, 0x00), rgb(0x9b, 0x7e, 0xde), rgb(0x4e, 0xc9, 0xb0),
            rgb(0xd1, 0x9a, 0x66),
        ],
    }
}

/// For terminals with a light background.
pub fn light() -> Theme {
    Theme {
        name: "light",
        accent: rgb(0x30, 0x5c, 0xde),
        text: rgb(0x24, 0x28, 0x30),
        bright: Color::Black,
        dim: rgb(0x8a, 0x8f, 0x98),
        header_fg: Color::White,
        sel_bg: rgb(0xd2, 0xdf, 0xff),
        sel_reverse: false,
        mention_bg: rgb(0xff, 0xec, 0xb3),
        mention_reverse: false,
        badge_bg: rgb(0xe2, 0xe5, 0xec),
        badge_fg: Color::Black,
        statusbar_bg: rgb(0xe6, 0xe9, 0xf0),
        card_desc: rgb(0x50, 0x55, 0x60),
        good: rgb(0x1a, 0x7f, 0x37),
        warn: rgb(0x96, 0x66, 0x00),
        bad: rgb(0xc0, 0x2a, 0x2a),
        special: rgb(0x8a, 0x3c, 0xc0),
        part: rgb(0xa0, 0x50, 0x50),
        nicks: vec![
            rgb(0xc0, 0x2a, 0x2a), rgb(0x1a, 0x7f, 0x37), rgb(0x96, 0x66, 0x00),
            rgb(0x20, 0x4a, 0xc0), rgb(0x8a, 0x3c, 0xc0), rgb(0x0a, 0x6e, 0x8a),
            rgb(0xb5, 0x5a, 0x00), rgb(0x5a, 0x44, 0xa0), rgb(0x0a, 0x7a, 0x6a),
            rgb(0x8a, 0x55, 0x22),
        ],
    }
}

/// Adapts to the terminal's own palette (ANSI named colors + default fg, with
/// reverse-video highlights). Best legibility on unusual terminal themes.
pub fn terminal() -> Theme {
    Theme {
        name: "terminal",
        accent: Color::LightBlue,
        text: Color::Reset,
        bright: Color::White,
        dim: Color::DarkGray,
        header_fg: Color::Black,
        sel_bg: Color::Reset,
        sel_reverse: true,
        mention_bg: Color::Reset,
        mention_reverse: true,
        badge_bg: Color::DarkGray,
        badge_fg: Color::White,
        statusbar_bg: Color::Reset,
        card_desc: Color::Gray,
        good: Color::Green,
        warn: Color::Yellow,
        bad: Color::Red,
        special: Color::Magenta,
        part: Color::Red,
        nicks: vec![
            Color::LightRed, Color::LightGreen, Color::LightYellow, Color::LightBlue,
            Color::LightMagenta, Color::LightCyan, Color::Red, Color::Green,
            Color::Yellow, Color::Cyan,
        ],
    }
}

/// Nord — a cool blue-grey palette.
pub fn nord() -> Theme {
    Theme {
        name: "nord",
        accent: rgb(0x88, 0xc0, 0xd0),
        text: rgb(0xd8, 0xde, 0xe9),
        bright: rgb(0xec, 0xef, 0xf4),
        dim: rgb(0x61, 0x6e, 0x88),
        header_fg: rgb(0x2e, 0x34, 0x40),
        sel_bg: rgb(0x3b, 0x42, 0x52),
        sel_reverse: false,
        mention_bg: rgb(0x4c, 0x56, 0x6a),
        mention_reverse: false,
        badge_bg: rgb(0x3b, 0x42, 0x52),
        badge_fg: rgb(0xec, 0xef, 0xf4),
        statusbar_bg: rgb(0x2e, 0x34, 0x40),
        card_desc: rgb(0xab, 0xb2, 0xbf),
        good: rgb(0xa3, 0xbe, 0x8c),
        warn: rgb(0xeb, 0xcb, 0x8b),
        bad: rgb(0xbf, 0x61, 0x6a),
        special: rgb(0xb4, 0x8e, 0xad),
        part: rgb(0xbf, 0x61, 0x6a),
        nicks: vec![
            rgb(0xbf, 0x61, 0x6a), rgb(0xa3, 0xbe, 0x8c), rgb(0xeb, 0xcb, 0x8b),
            rgb(0x81, 0xa1, 0xc1), rgb(0xb4, 0x8e, 0xad), rgb(0x88, 0xc0, 0xd0),
            rgb(0xd0, 0x87, 0x70), rgb(0x5e, 0x81, 0xac), rgb(0x8f, 0xbc, 0xbb),
            rgb(0xe5, 0xe9, 0xf0),
        ],
    }
}
