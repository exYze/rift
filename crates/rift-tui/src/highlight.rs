//! Syntax highlighting for fenced code blocks, via syntect. Sets are loaded
//! once into statics so per-block highlighters can borrow them. Every miss
//! (unknown language, unknown theme, feature disabled) degrades to the
//! theme's flat code color — highlighting is decoration, never a failure
//! source. Built without the `highlight` feature, this module is a no-op
//! stub and syntect stays out of the binary entirely.

#[cfg(not(feature = "highlight"))]
mod imp {
    use ratatui::style::Color;

    pub(crate) struct BlockHighlighter;

    impl BlockHighlighter {
        pub(crate) fn new(_lang: &str, _theme_name: &str) -> Option<Self> {
            None
        }
        pub(crate) fn line(&mut self, _text: &str) -> Option<Vec<(Color, String)>> {
            None
        }
    }
}

#[cfg(feature = "highlight")]
mod imp {

use std::sync::OnceLock;

use ratatui::style::Color;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

fn syntax_set() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    // Non-newline variant: we highlight lines that have already been split.
    S.get_or_init(SyntaxSet::load_defaults_nonewlines)
}

fn theme_set() -> &'static ThemeSet {
    static T: OnceLock<ThemeSet> = OnceLock::new();
    T.get_or_init(ThemeSet::load_defaults)
}

/// Stateful highlighter for one fenced code block (state carries across
/// lines, so multi-line strings and comments color correctly).
pub(crate) struct BlockHighlighter {
    hl: HighlightLines<'static>,
}

impl BlockHighlighter {
    /// `lang` is the fence tag (```rust → "rust"); returns None when the
    /// language or theme is unknown.
    pub(crate) fn new(lang: &str, theme_name: &str) -> Option<Self> {
        let syntax = syntax_set().find_syntax_by_token(lang)?;
        let theme = theme_set().themes.get(theme_name)?;
        Some(Self { hl: HighlightLines::new(syntax, theme) })
    }

    /// Foreground-color spans for one source line; None on highlighter error.
    pub(crate) fn line(&mut self, text: &str) -> Option<Vec<(Color, String)>> {
        let regions = self.hl.highlight_line(text, syntax_set()).ok()?;
        Some(
            regions
                .into_iter()
                .map(|(style, s)| {
                    let f = style.foreground;
                    (Color::Rgb(f.r, f.g, f.b), s.to_string())
                })
                .collect(),
        )
    }
}

}

pub(crate) use imp::BlockHighlighter;

#[cfg(all(test, feature = "highlight"))]
mod tests {
    use super::*;

    #[test]
    fn common_fence_tags_resolve_to_syntaxes() {
        // The tags users actually type — a miss degrades to flat color, but
        // these staples must highlight (markdown especially: "output this as
        // markdown in a code block" is a common ask).
        // (toml is a known gap in syntect's default set — flat color there.)
        for lang in ["markdown", "md", "python", "rust", "json", "yaml", "diff", "sh", "html", "css", "sql"] {
            assert!(
                BlockHighlighter::new(lang, "base16-ocean.dark").is_some(),
                "no syntax resolved for fence tag '{lang}'"
            );
        }
    }
}
