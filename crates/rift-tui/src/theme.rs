//! Built-in color palettes. Every color the TUI paints comes from the active
//! `Theme`, so palettes stay consistent and terminals with light backgrounds
//! (or no color trust at all) get a usable scheme. Selected via `"theme"` in
//! config or `/theme <name>` at runtime.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    /// Focused borders, the user prompt marker, selection backgrounds.
    pub accent: Color,
    /// Info lines, thinking text, unfocused borders.
    pub muted: Color,
    /// Code-block fallback color (when syntax highlighting is off/unknown).
    pub code: Color,
    /// Tool-call lines in the activity pane.
    pub tool: Color,
    pub warn: Color,
    pub error: Color,
    pub ok: Color,
    pub diff_add: Color,
    pub diff_del: Color,
    pub diff_hunk: Color,
    /// Text painted on top of `accent` (selection rows, the model chip).
    pub sel_fg: Color,
    /// syntect theme for fenced code blocks; None = single-color code.
    pub syntax: Option<&'static str>,
}

pub const DARK: Theme = Theme {
    name: "dark",
    accent: Color::Cyan,
    muted: Color::DarkGray,
    code: Color::Green,
    tool: Color::White,
    warn: Color::Yellow,
    error: Color::Red,
    ok: Color::Green,
    diff_add: Color::Green,
    diff_del: Color::Red,
    diff_hunk: Color::Cyan,
    sel_fg: Color::Black,
    syntax: Some("base16-ocean.dark"),
};

/// For terminals with light backgrounds: darker accents, no White text
/// (invisible on white), a light syntect theme.
pub const LIGHT: Theme = Theme {
    name: "light",
    accent: Color::Blue,
    muted: Color::Gray,
    code: Color::Green,
    tool: Color::Black,
    warn: Color::Magenta,
    error: Color::Red,
    ok: Color::Green,
    diff_add: Color::Green,
    diff_del: Color::Red,
    diff_hunk: Color::Blue,
    sel_fg: Color::White,
    syntax: Some("InspiredGitHub"),
};

/// High-contrast two-tone: for color-hostile terminals and accessibility.
/// Semantics survive via weight/reversal rather than hue.
pub const MONO: Theme = Theme {
    name: "mono",
    accent: Color::White,
    muted: Color::DarkGray,
    code: Color::White,
    tool: Color::White,
    warn: Color::White,
    error: Color::White,
    ok: Color::White,
    diff_add: Color::White,
    diff_del: Color::DarkGray,
    diff_hunk: Color::White,
    sel_fg: Color::Black,
    syntax: None,
};

pub const THEMES: &[Theme] = &[DARK, LIGHT, MONO];

pub fn find(name: &str) -> Option<&'static Theme> {
    THEMES.iter().find(|t| t.name.eq_ignore_ascii_case(name))
}

pub fn names() -> Vec<&'static str> {
    THEMES.iter().map(|t| t.name).collect()
}
