//! Built-in color palettes. Every color the TUI paints comes from the active
//! `Theme`, so palettes stay consistent and terminals with light backgrounds
//! (or no color trust at all) get a usable scheme. Selected via `"theme"` in
//! config or `/theme <name>` at runtime.
//!
//! The classic three (dark/light/mono) inherit the terminal's own text and
//! background colors (`fg`/`bg` = None) so they blend into any terminal. The
//! named palettes (dracula, nord, gruvbox, …) paint their own background and
//! text — they need a truecolor terminal (Windows Terminal, iTerm2, kitty,
//! and every other modern emulator).

use ratatui::style::Color;

/// `0xRRGGBB` → Color, so palettes below read like their upstream specs.
const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    /// Default text color; None = the terminal's own foreground.
    pub fg: Option<Color>,
    /// Background painted behind every pane; None = the terminal's own.
    pub bg: Option<Color>,
    /// Unfocused pane borders (focused borders use `accent`).
    pub border: Color,
    /// Focused borders, the user prompt marker, selection backgrounds.
    pub accent: Color,
    /// Info lines, thinking text, secondary chrome.
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
    fg: None,
    bg: None,
    border: Color::DarkGray,
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
    fg: None,
    bg: None,
    border: Color::Gray,
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
    fg: None,
    bg: None,
    border: Color::DarkGray,
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

pub const DRACULA: Theme = Theme {
    name: "dracula",
    fg: Some(rgb(0xf8f8f2)),
    bg: Some(rgb(0x282a36)),
    border: rgb(0x6272a4),
    accent: rgb(0xbd93f9),
    muted: rgb(0x6272a4),
    code: rgb(0x50fa7b),
    tool: rgb(0x8be9fd),
    warn: rgb(0xf1fa8c),
    error: rgb(0xff5555),
    ok: rgb(0x50fa7b),
    diff_add: rgb(0x50fa7b),
    diff_del: rgb(0xff5555),
    diff_hunk: rgb(0x8be9fd),
    sel_fg: rgb(0x282a36),
    syntax: Some("base16-eighties.dark"),
};

pub const NORD: Theme = Theme {
    name: "nord",
    fg: Some(rgb(0xd8dee9)),
    bg: Some(rgb(0x2e3440)),
    border: rgb(0x4c566a),
    accent: rgb(0x88c0d0),
    muted: rgb(0x616e88),
    code: rgb(0xa3be8c),
    tool: rgb(0x81a1c1),
    warn: rgb(0xebcb8b),
    error: rgb(0xbf616a),
    ok: rgb(0xa3be8c),
    diff_add: rgb(0xa3be8c),
    diff_del: rgb(0xbf616a),
    diff_hunk: rgb(0x88c0d0),
    sel_fg: rgb(0x2e3440),
    syntax: Some("base16-ocean.dark"),
};

pub const GRUVBOX: Theme = Theme {
    name: "gruvbox",
    fg: Some(rgb(0xebdbb2)),
    bg: Some(rgb(0x282828)),
    border: rgb(0x665c54),
    accent: rgb(0xfabd2f),
    muted: rgb(0x928374),
    code: rgb(0xb8bb26),
    tool: rgb(0x83a598),
    warn: rgb(0xfe8019),
    error: rgb(0xfb4934),
    ok: rgb(0xb8bb26),
    diff_add: rgb(0xb8bb26),
    diff_del: rgb(0xfb4934),
    diff_hunk: rgb(0x8ec07c),
    sel_fg: rgb(0x282828),
    syntax: Some("base16-eighties.dark"),
};

pub const SOLARIZED_DARK: Theme = Theme {
    name: "solarized-dark",
    fg: Some(rgb(0x93a1a1)),
    bg: Some(rgb(0x002b36)),
    border: rgb(0x586e75),
    accent: rgb(0x268bd2),
    muted: rgb(0x586e75),
    code: rgb(0x859900),
    tool: rgb(0x2aa198),
    warn: rgb(0xb58900),
    error: rgb(0xdc322f),
    ok: rgb(0x859900),
    diff_add: rgb(0x859900),
    diff_del: rgb(0xdc322f),
    diff_hunk: rgb(0x268bd2),
    sel_fg: rgb(0xfdf6e3),
    syntax: Some("Solarized (dark)"),
};

pub const SOLARIZED_LIGHT: Theme = Theme {
    name: "solarized-light",
    fg: Some(rgb(0x657b83)),
    bg: Some(rgb(0xfdf6e3)),
    border: rgb(0x93a1a1),
    accent: rgb(0x268bd2),
    muted: rgb(0x93a1a1),
    code: rgb(0x859900),
    tool: rgb(0x2aa198),
    warn: rgb(0xb58900),
    error: rgb(0xdc322f),
    ok: rgb(0x859900),
    diff_add: rgb(0x859900),
    diff_del: rgb(0xdc322f),
    diff_hunk: rgb(0x268bd2),
    sel_fg: rgb(0xfdf6e3),
    syntax: Some("Solarized (light)"),
};

pub const TOKYO_NIGHT: Theme = Theme {
    name: "tokyo-night",
    fg: Some(rgb(0xc0caf5)),
    bg: Some(rgb(0x1a1b26)),
    border: rgb(0x3b4261),
    accent: rgb(0x7aa2f7),
    muted: rgb(0x565f89),
    code: rgb(0x9ece6a),
    tool: rgb(0x7dcfff),
    warn: rgb(0xe0af68),
    error: rgb(0xf7768e),
    ok: rgb(0x9ece6a),
    diff_add: rgb(0x9ece6a),
    diff_del: rgb(0xf7768e),
    diff_hunk: rgb(0x7dcfff),
    sel_fg: rgb(0x1a1b26),
    syntax: Some("base16-ocean.dark"),
};

pub const CATPPUCCIN: Theme = Theme {
    name: "catppuccin",
    fg: Some(rgb(0xcdd6f4)),
    bg: Some(rgb(0x1e1e2e)),
    border: rgb(0x45475a),
    accent: rgb(0xcba6f7),
    muted: rgb(0x6c7086),
    code: rgb(0xa6e3a1),
    tool: rgb(0x89dceb),
    warn: rgb(0xf9e2af),
    error: rgb(0xf38ba8),
    ok: rgb(0xa6e3a1),
    diff_add: rgb(0xa6e3a1),
    diff_del: rgb(0xf38ba8),
    diff_hunk: rgb(0x89b4fa),
    sel_fg: rgb(0x1e1e2e),
    syntax: Some("base16-mocha.dark"),
};

pub const ROSE_PINE: Theme = Theme {
    name: "rose-pine",
    fg: Some(rgb(0xe0def4)),
    bg: Some(rgb(0x191724)),
    border: rgb(0x403d52),
    accent: rgb(0xebbcba),
    muted: rgb(0x6e6a86),
    code: rgb(0x9ccfd8),
    tool: rgb(0xc4a7e7),
    warn: rgb(0xf6c177),
    error: rgb(0xeb6f92),
    ok: rgb(0x9ccfd8),
    diff_add: rgb(0x9ccfd8),
    diff_del: rgb(0xeb6f92),
    diff_hunk: rgb(0xc4a7e7),
    sel_fg: rgb(0x191724),
    syntax: Some("base16-mocha.dark"),
};

pub const MATRIX: Theme = Theme {
    name: "matrix",
    fg: Some(rgb(0x00cc44)),
    bg: Some(rgb(0x000000)),
    border: rgb(0x004d1a),
    accent: rgb(0x00ff66),
    muted: rgb(0x007a29),
    code: rgb(0x00ff9c),
    tool: rgb(0x66ffb2),
    warn: rgb(0xccff33),
    error: rgb(0xff3333),
    ok: rgb(0x00ff66),
    diff_add: rgb(0x00ff66),
    diff_del: rgb(0xff3333),
    diff_hunk: rgb(0x66ffb2),
    sel_fg: rgb(0x000000),
    syntax: None,
};

pub const SYNTHWAVE: Theme = Theme {
    name: "synthwave",
    fg: Some(rgb(0xf0eff1)),
    bg: Some(rgb(0x241b2f)),
    border: rgb(0x495495),
    accent: rgb(0xff7edb),
    muted: rgb(0x848bbd),
    code: rgb(0x72f1b8),
    tool: rgb(0x36f9f6),
    warn: rgb(0xfede5d),
    error: rgb(0xfe4450),
    ok: rgb(0x72f1b8),
    diff_add: rgb(0x72f1b8),
    diff_del: rgb(0xfe4450),
    diff_hunk: rgb(0x36f9f6),
    sel_fg: rgb(0x241b2f),
    syntax: Some("base16-eighties.dark"),
};

pub const THEMES: &[Theme] = &[
    DARK,
    LIGHT,
    MONO,
    DRACULA,
    NORD,
    GRUVBOX,
    SOLARIZED_DARK,
    SOLARIZED_LIGHT,
    TOKYO_NIGHT,
    CATPPUCCIN,
    ROSE_PINE,
    MATRIX,
    SYNTHWAVE,
];

pub fn find(name: &str) -> Option<&'static Theme> {
    THEMES.iter().find(|t| t.name.eq_ignore_ascii_case(name))
}

pub fn names() -> Vec<&'static str> {
    THEMES.iter().map(|t| t.name).collect()
}

/// Resolve a theme name: built-ins first, then custom JSON themes —
/// `~/.config/rift/themes/<name>.json`, then `themes/<name>.json` inside
/// any plugin (user `~/.config/rift/plugins/*/`, project `.rift/plugins/*/`
/// — themes are inert colors, so project ones are safe to load).
pub fn resolve(name: &str, cwd: &std::path::Path) -> Option<Theme> {
    if let Some(t) = find(name) {
        return Some(*t);
    }
    let file = format!("{}.json", name.to_ascii_lowercase());
    let mut candidates: Vec<std::path::PathBuf> = vec![];
    if let Some(cfg) = rift_core::paths::config_dir() {
        candidates.push(cfg.join("rift/themes").join(&file));
        candidates.extend(plugin_theme_paths(&cfg.join("rift/plugins"), &file));
    }
    candidates.extend(plugin_theme_paths(&cwd.join(".rift/plugins"), &file));
    candidates.iter().find_map(|p| parse_custom(name, p))
}

fn plugin_theme_paths(plugins_dir: &std::path::Path, file: &str) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(plugins_dir) else { return vec![] };
    let mut out: Vec<_> = entries.flatten().map(|e| e.path().join("themes").join(file)).collect();
    out.sort();
    out
}

/// Parse `{"base": "dark", "accent": "#39c5cf", ...}` — any field of
/// [`Theme`] as a `#RRGGBB` string (or `"syntax"` as a syntect theme name);
/// everything unspecified inherits from `base` (default: dark).
fn parse_custom(name: &str, path: &std::path::Path) -> Option<Theme> {
    let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let obj = raw.as_object()?;
    let base = obj.get("base").and_then(|b| b.as_str()).unwrap_or("dark");
    let mut theme = *find(base)?;
    // The name and syntax strings must be 'static: themes load once per
    // /theme switch, so the leak is bounded and deliberate.
    theme.name = Box::leak(name.to_ascii_lowercase().into_boxed_str());
    let hex = |key: &str| -> Option<Color> {
        let s = obj.get(key)?.as_str()?.trim_start_matches('#');
        u32::from_str_radix(s, 16).ok().filter(|_| s.len() == 6).map(rgb)
    };
    let set = |key: &str, slot: &mut Color| {
        if let Some(c) = hex(key) {
            *slot = c;
        }
    };
    set("border", &mut theme.border);
    set("accent", &mut theme.accent);
    set("muted", &mut theme.muted);
    set("code", &mut theme.code);
    set("tool", &mut theme.tool);
    set("warn", &mut theme.warn);
    set("error", &mut theme.error);
    set("ok", &mut theme.ok);
    set("diff_add", &mut theme.diff_add);
    set("diff_del", &mut theme.diff_del);
    set("diff_hunk", &mut theme.diff_hunk);
    set("sel_fg", &mut theme.sel_fg);
    if let Some(c) = hex("fg") {
        theme.fg = Some(c);
    }
    if let Some(c) = hex("bg") {
        theme.bg = Some(c);
    }
    if let Some(s) = obj.get("syntax").and_then(|s| s.as_str()) {
        theme.syntax = Some(Box::leak(s.to_string().into_boxed_str()));
    }
    Some(theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_findable() {
        let names = names();
        assert_eq!(names.len(), 13);
        for n in &names {
            assert!(find(n).is_some());
            assert!(find(&n.to_uppercase()).is_some()); // case-insensitive
        }
        let mut dedup = names.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), names.len());
    }

    #[test]
    fn syntax_themes_exist_in_syntect_defaults() {
        // A typo'd syntect name would silently fall back at render time;
        // pin the mapping here instead.
        #[cfg(feature = "highlight")]
        {
            let ts = syntect::highlighting::ThemeSet::load_defaults();
            for t in THEMES {
                if let Some(s) = t.syntax {
                    assert!(ts.themes.contains_key(s), "theme {} references unknown syntect theme '{s}'", t.name);
                }
            }
        }
    }
}

#[cfg(test)]
mod custom_tests {
    use super::*;

    #[test]
    fn custom_json_theme_inherits_base_and_overrides() {
        let dir = std::env::temp_dir().join(format!("rift-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mytheme.json");
        std::fs::write(
            &path,
            r##"{"base": "nord", "accent": "#ff0000", "bg": "#101010", "syntax": "InspiredGitHub"}"##,
        )
        .unwrap();

        let t = parse_custom("MyTheme", &path).unwrap();
        assert_eq!(t.name, "mytheme");
        assert_eq!(t.accent, rgb(0xff0000));
        assert_eq!(t.bg, Some(rgb(0x101010)));
        assert_eq!(t.syntax, Some("InspiredGitHub"));
        // Unspecified fields inherit from the base (nord).
        assert_eq!(t.error, NORD.error);
        assert_eq!(t.muted, NORD.muted);

        // Bad hex is skipped, unknown base fails cleanly.
        std::fs::write(&path, r##"{"accent": "#xyzxyz"}"##).unwrap();
        let t = parse_custom("m", &path).unwrap();
        assert_eq!(t.accent, DARK.accent, "bad hex must not change the field");
        std::fs::write(&path, r##"{"base": "nope"}"##).unwrap();
        assert!(parse_custom("m", &path).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
