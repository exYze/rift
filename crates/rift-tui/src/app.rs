//! TUI frontend: stable, independently scrollable panes.
//!
//! Layout: transcript pane (conversation) | tool-log pane (tool calls,
//! warnings, step markers), status line, multi-line input. Tab moves focus;
//! each pane keeps its own scroll state.
//!
//! Anti-jank invariants (the fixes for the scrolling bugs endemic to agent
//! TUIs):
//! - every pane's text is pre-wrapped to its current width into a flat line
//!   buffer; the viewport is a pure slice — scroll math is exact, no drift
//! - scroll offsets are measured from the BOTTOM; 0 = follow mode
//! - when tokens stream in while a pane is scrolled up, the offset advances
//!   by the appended line count, so the visible text never moves
//! - only End (or Esc when idle) re-enters follow mode

use std::io::stdout;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use rift_core::{Agent, AgentEvent, AskRequest, PlanItem, SessionStore, Skill, TurnStats};
use rift_ollama::{Message, Role};
use tokio::sync::oneshot;

use crate::commands::{self, CmdCx, PickerItem, UiEffect};
use crate::theme;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    User,
    Assistant,
    Code,
    Thinking,
    Tool,
    ToolErr,
    Warn,
    Info,
    DiffAdd,
    DiffDel,
    DiffHunk,
    DiffMeta,
}

impl Kind {
    /// Kinds whose lines must be hard-cut (never re-wrapped) so indentation
    /// and alignment stay exact.
    fn hard_cut(self) -> bool {
        matches!(self, Kind::Code | Kind::DiffAdd | Kind::DiffDel | Kind::DiffHunk | Kind::DiffMeta)
    }
}

pub(crate) fn style_for(kind: Kind, t: &theme::Theme) -> Style {
    match kind {
        Kind::User => Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        Kind::Assistant => Style::default(),
        Kind::Code => Style::default().fg(t.code),
        Kind::Thinking => Style::default().fg(t.muted).add_modifier(Modifier::ITALIC),
        Kind::Tool => Style::default().fg(t.tool),
        Kind::ToolErr => Style::default().fg(t.error),
        Kind::Warn => Style::default().fg(t.warn),
        Kind::Info => Style::default().fg(t.muted),
        Kind::DiffAdd => Style::default().fg(t.diff_add),
        Kind::DiffDel => Style::default().fg(t.diff_del),
        Kind::DiffHunk => Style::default().fg(t.diff_hunk),
        Kind::DiffMeta => Style::default().fg(t.muted).add_modifier(Modifier::BOLD),
    }
}

struct BlockEntry {
    kind: Kind,
    text: String,
    /// Whether a blank separator line follows this block when rendered.
    gap: bool,
}

/// One pre-wrapped visual line. `spans` (fenced code only) carries syntect
/// foreground colors; plain lines style by `kind` at draw time so a theme
/// switch recolors them without a re-wrap.
struct WrappedLine {
    kind: Kind,
    text: String,
    spans: Option<Vec<(Color, String)>>,
}

impl WrappedLine {
    fn plain(kind: Kind, text: String) -> Self {
        Self { kind, text, spans: None }
    }
}

/// A scrollable region with its own content and bottom-anchored scroll state.
pub(crate) struct Pane {
    blocks: Vec<BlockEntry>,
    wrapped: Vec<WrappedLine>,
    /// syntect theme for fenced code; None = flat theme color.
    syntax: Option<&'static str>,
    /// Wrapped line count per block (parallel to `blocks`), so a streaming
    /// append re-wraps only the tail block instead of the whole transcript.
    line_counts: Vec<usize>,
    /// Index of the first block whose cached wrap is stale.
    first_dirty: usize,
    wrap_width: u16,
    /// Full invalidation (width-independent), e.g. after suspending for $EDITOR.
    pub(crate) dirty: bool,
    pub(crate) scroll_from_bottom: usize,
    pub(crate) view_height: usize,
    /// Last rendered screen area, for mouse-wheel routing.
    pub(crate) area: Rect,
}

/// Strip ANSI escape sequences and control characters before text enters a
/// pane. Tool output (npm, cargo, git…) is full of color codes; written raw
/// into the terminal they desync the cursor and corrupt the whole frame.
/// Tabs become spaces, \r is dropped, \n survives.
pub(crate) fn sanitize_for_pane(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                // CSI: ESC [ … final byte in @..=~
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] … BEL or ESC \
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\x07' || (n == '\x1b' && chars.peek() == Some(&'\\')) {
                            if n == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Two-char escapes (ESC c, ESC 7, …)
                _ => {
                    chars.next();
                }
            },
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

impl Pane {
    pub(crate) fn new() -> Self {
        Self {
            blocks: vec![],
            wrapped: vec![],
            syntax: None,
            line_counts: vec![],
            first_dirty: 0,
            wrap_width: 0,
            dirty: true,
            scroll_from_bottom: 0,
            view_height: 0,
            area: Rect::default(),
        }
    }

    /// Enable/switch syntect highlighting for fenced code (transcript only).
    /// Forces a full re-wrap: cached spans hold the old theme's colors.
    pub(crate) fn set_syntax(&mut self, syntax: Option<&'static str>) {
        if self.syntax != syntax {
            self.syntax = syntax;
            self.dirty = true;
        }
    }

    pub(crate) fn push_block(&mut self, kind: Kind, text: String) {
        self.first_dirty = self.first_dirty.min(self.blocks.len());
        self.blocks.push(BlockEntry { kind, text: sanitize_for_pane(&text), gap: true });
    }

    /// Dense single line, no blank separator after it (tool logs, diffs).
    pub(crate) fn push_line(&mut self, kind: Kind, text: String) {
        self.first_dirty = self.first_dirty.min(self.blocks.len());
        self.blocks.push(BlockEntry { kind, text: sanitize_for_pane(&text), gap: false });
    }

    pub(crate) fn append_stream(&mut self, kind: Kind, text: &str) {
        let text = sanitize_for_pane(text);
        match self.blocks.last_mut() {
            Some(last) if last.kind == kind => last.text.push_str(&text),
            _ => self.blocks.push(BlockEntry { kind, text, gap: true }),
        }
        // Only the tail block changed; leave earlier wrap caches intact.
        self.first_dirty = self.first_dirty.min(self.blocks.len() - 1);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// The pane's full content as plain text (for clipboard export).
    pub(crate) fn raw_text(&self) -> String {
        self.blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    pub(crate) fn rebuild(&mut self, width: u16, t: &theme::Theme) {
        let width_changed = width != self.wrap_width;
        if self.dirty || width_changed {
            // Width change (or external invalidation) voids every block's cache.
            self.first_dirty = 0;
            self.dirty = false;
            self.wrap_width = width;
        }
        if self.first_dirty >= self.blocks.len() {
            return;
        }
        let old_total = self.wrapped.len();
        let w = width.max(10) as usize;
        // Drop cached lines from the first stale block onward; re-wrap the tail.
        self.line_counts.truncate(self.first_dirty);
        let keep: usize = self.line_counts.iter().sum();
        self.wrapped.truncate(keep);
        for block in &self.blocks[self.first_dirty..] {
            let lines_before = self.wrapped.len();
            let prefixed = match block.kind {
                Kind::User => format!("❯ {}", block.text),
                _ => block.text.clone(),
            };
            // Fenced code blocks inside assistant text render as a bordered
            // box with a language-labeled header; the raw ``` fences are
            // consumed, not shown. The highlighter is stateful per block so
            // multi-line strings and comments color correctly.
            let mut in_fence = false;
            let mut hl: Option<crate::highlight::BlockHighlighter> = None;
            for raw_line in prefixed.lines() {
                // A ``` line opens or closes the box (and is not itself drawn).
                if block.kind == Kind::Assistant && raw_line.trim_start().starts_with("```") {
                    if in_fence {
                        in_fence = false;
                        hl = None;
                        self.wrapped.push(code_box_border(false, "", w, t.muted));
                    } else {
                        in_fence = true;
                        let lang = raw_line.trim_start().trim_start_matches('`').trim();
                        hl = match (self.syntax, lang.is_empty()) {
                            (Some(theme), false) => crate::highlight::BlockHighlighter::new(lang, theme),
                            _ => None,
                        };
                        self.wrapped.push(code_box_border(true, lang, w, t.muted));
                    }
                    continue;
                }
                if in_fence {
                    // Boxed code content: a `│ ` gutter + hard-cut source (so
                    // alignment stays exact), highlighted when a syntect theme
                    // and known language are set, else the flat code color.
                    if raw_line.is_empty() {
                        self.wrapped.push(WrappedLine {
                            kind: Kind::Code,
                            text: "│".into(),
                            spans: Some(vec![(t.muted, "│".into())]),
                        });
                        continue;
                    }
                    let cut_w = w.saturating_sub(2).max(1);
                    match hl.as_mut().and_then(|h| h.line(raw_line)) {
                        Some(spans) => {
                            for piece in cut_spans(spans, cut_w, "") {
                                self.wrapped.push(code_box_line(piece, t.muted));
                            }
                        }
                        None => {
                            let mut rest = raw_line;
                            loop {
                                let cut = floor_boundary(rest, cut_w);
                                self.wrapped
                                    .push(code_box_line(vec![(t.code, rest[..cut].to_string())], t.muted));
                                rest = &rest[cut..];
                                if rest.is_empty() {
                                    break;
                                }
                            }
                        }
                    }
                    continue;
                }
                // Non-fenced content: diffs hard-cut, everything else wraps.
                let kind = block.kind;
                if raw_line.is_empty() {
                    self.wrapped.push(WrappedLine::plain(kind, String::new()));
                } else if kind.hard_cut() {
                    let mut rest = raw_line;
                    loop {
                        let cut = floor_boundary(rest, w);
                        self.wrapped.push(WrappedLine::plain(kind, rest[..cut].to_string()));
                        rest = &rest[cut..];
                        if rest.is_empty() {
                            break;
                        }
                    }
                } else {
                    for piece in textwrap::wrap(raw_line, w) {
                        self.wrapped.push(WrappedLine::plain(kind, piece.into_owned()));
                    }
                }
            }
            // Auto-close an unterminated fence (normal mid-stream, or a model
            // that dropped the closing ```), so the box always looks complete.
            if in_fence {
                self.wrapped.push(code_box_border(false, "", w, t.muted));
            }
            if block.gap {
                self.wrapped.push(WrappedLine::plain(block.kind, String::new()));
            }
            self.line_counts.push(self.wrapped.len() - lines_before);
        }
        self.first_dirty = self.blocks.len();
        if self.scroll_from_bottom > 0 && !width_changed && self.wrapped.len() > old_total {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(self.wrapped.len() - old_total);
        }
    }

    pub(crate) fn max_scroll(&self) -> usize {
        self.wrapped.len().saturating_sub(self.view_height)
    }

    pub(crate) fn scroll_up(&mut self, n: usize) {
        self.scroll_from_bottom = (self.scroll_from_bottom + n).min(self.max_scroll());
    }

    pub(crate) fn scroll_down(&mut self, n: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(n);
    }

    pub(crate) fn visible_lines(&mut self, t: &theme::Theme) -> Vec<Line<'static>> {
        self.scroll_from_bottom = self.scroll_from_bottom.min(self.max_scroll());
        let total = self.wrapped.len();
        let end = total - self.scroll_from_bottom.min(total);
        let start = end.saturating_sub(self.view_height);
        self.wrapped[start..end]
            .iter()
            .map(|wl| match &wl.spans {
                Some(spans) => Line::from(
                    spans
                        .iter()
                        .map(|(c, s)| Span::styled(s.clone(), Style::default().fg(*c)))
                        .collect::<Vec<_>>(),
                ),
                None => Line::from(Span::styled(wl.text.clone(), style_for(wl.kind, t))),
            })
            .collect()
    }

    pub(crate) fn contains(&self, col: u16, row: u16) -> bool {
        self.area.contains(Position { x: col, y: row })
    }
}

/// Classify a unified-diff line for coloring (shared with the swarm TUI).
pub(crate) fn diff_kind(line: &str) -> Kind {
    if line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
    {
        Kind::DiffMeta
    } else if line.starts_with("@@") {
        Kind::DiffHunk
    } else if line.starts_with('+') {
        Kind::DiffAdd
    } else if line.starts_with('-') {
        Kind::DiffDel
    } else {
        Kind::Assistant
    }
}

/// What the TUI sends to the agent task: a normal prompt for the model, or a
/// slash command handled locally.
enum UiMsg {
    Prompt(String, CancellationToken),
    Command(String, CancellationToken),
}

/// Interactive list overlay: command pickers (/model, /sessions) and
/// elicitation choices share the same widget and key handling.
struct PickerState {
    title: String,
    items: Vec<PickerItem>,
    selected: usize,
    kind: PickerKind,
}

enum PickerKind {
    /// Enter runs `template` with `{}` replaced by the chosen value.
    Command { template: String },
    /// Enter answers a pending ask_user question.
    Elicit { reply: Option<oneshot::Sender<String>> },
}

/// Hard-cut colored spans into visual lines of at most `width` chars, each
/// prefixed with `indent`. The char under the cut keeps its color; span
/// boundaries never shift.
fn cut_spans(spans: Vec<(Color, String)>, width: usize, indent: &str) -> Vec<Vec<(Color, String)>> {
    let mut out: Vec<Vec<(Color, String)>> = vec![];
    let mut cur: Vec<(Color, String)> = vec![(Color::Reset, indent.to_string())];
    let mut cur_len = 0usize;
    for (color, text) in spans {
        let mut buf = String::new();
        for ch in text.chars() {
            if cur_len >= width {
                if !buf.is_empty() {
                    cur.push((color, std::mem::take(&mut buf)));
                }
                out.push(std::mem::take(&mut cur));
                cur.push((Color::Reset, indent.to_string()));
                cur_len = 0;
            }
            buf.push(ch);
            cur_len += 1;
        }
        if !buf.is_empty() {
            cur.push((color, buf));
        }
    }
    if out.is_empty() || cur.len() > 1 {
        out.push(cur);
    }
    out
}

/// Top (`╭─ python ────`) or bottom (`╰──────`) border of a fenced code box,
/// spanning the full pane width. `label` is the fence's language tag ("" →
/// "code" on the top border; ignored on the bottom).
fn code_box_border(top: bool, label: &str, w: usize, border: Color) -> WrappedLine {
    let s = if top {
        let mut head = format!("╭─ {} ", if label.is_empty() { "code" } else { label });
        let used = head.chars().count();
        if used < w {
            head.push_str(&"─".repeat(w - used));
        }
        head
    } else {
        format!("╰{}", "─".repeat(w.saturating_sub(1)))
    };
    WrappedLine { kind: Kind::Code, text: s.clone(), spans: Some(vec![(border, s)]) }
}

/// One code line inside the box: a border-colored `│ ` gutter followed by the
/// (already width-cut) content spans.
fn code_box_line(inner: Vec<(Color, String)>, border: Color) -> WrappedLine {
    let mut spans = vec![(border, "│ ".to_string())];
    spans.extend(inner);
    let text = spans.iter().map(|(_, s)| s.as_str()).collect();
    WrappedLine { kind: Kind::Code, text, spans: Some(spans) }
}

/// Short token count for the status line: `999`, `1.2k`, `12.3k`.
fn humanize_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

fn floor_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i.max(1) // always make progress even on a pathological boundary
}

// ---- input cursor helpers (byte offsets, always on char boundaries) ----

fn prev_char(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map(|(j, _)| j).unwrap_or(0)
}

fn next_char(s: &str, i: usize) -> usize {
    s[i..].chars().next().map(|c| i + c.len_utf8()).unwrap_or(i)
}

fn prev_word(s: &str, i: usize) -> usize {
    let mut j = i;
    while j > 0 && !s[..j].chars().next_back().unwrap().is_alphanumeric() {
        j = prev_char(s, j);
    }
    while j > 0 && s[..j].chars().next_back().unwrap().is_alphanumeric() {
        j = prev_char(s, j);
    }
    j
}

fn next_word(s: &str, i: usize) -> usize {
    let mut j = i;
    while j < s.len() && !s[j..].chars().next().unwrap().is_alphanumeric() {
        j = next_char(s, j);
    }
    while j < s.len() && s[j..].chars().next().unwrap().is_alphanumeric() {
        j = next_char(s, j);
    }
    j
}

/// Start of the line containing byte offset `i`.
fn line_start(s: &str, i: usize) -> usize {
    s[..i].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

/// End of the line containing byte offset `i` (before its newline).
fn line_end(s: &str, i: usize) -> usize {
    i + s[i..].find('\n').unwrap_or(s.len() - i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_ansi_and_controls_expands_tabs() {
        assert_eq!(sanitize_for_pane("\x1b[1;32m✓\x1b[0m ok"), "✓ ok");
        assert_eq!(sanitize_for_pane("a\tb"), "a    b");
        assert_eq!(sanitize_for_pane("line1\r\nline2"), "line1\nline2");
        assert_eq!(sanitize_for_pane("\x1b]0;win title\x07text"), "text");
        assert_eq!(sanitize_for_pane("bell\x07backspace\x08"), "bellbackspace");
        assert_eq!(sanitize_for_pane("plain text"), "plain text");
    }

    #[test]
    fn humanize_tokens_scales() {
        assert_eq!(humanize_tokens(0), "0");
        assert_eq!(humanize_tokens(999), "999");
        assert_eq!(humanize_tokens(1000), "1.0k");
        assert_eq!(humanize_tokens(12_345), "12.3k");
    }

    #[test]
    fn token_status_shows_live_estimate_then_session_total() {
        let mut app = App::new("m".into(), vec![], &theme::DARK, std::env::temp_dir());
        // Idle with no usage yet → nothing to show.
        assert_eq!(app.token_status(), "");
        // Idle with session totals → cumulative sent (↑) / received (↓).
        app.session_stats.prompt_tokens = 1200;
        app.session_stats.output_tokens = 3400;
        assert_eq!(app.token_status(), " · Σ 1.2k↑ 3.4k↓");
        // Mid-turn → live ~chars/4 estimate of this turn's output.
        app.busy = true;
        app.turn_output_chars = 400;
        assert_eq!(app.token_status(), " · ~100↓");
        // Busy but nothing streamed yet (prompt still processing) → empty.
        app.turn_output_chars = 0;
        assert_eq!(app.token_status(), "");
    }

    #[test]
    fn fenced_code_renders_as_a_labeled_box() {
        let mut p = Pane::new();
        p.set_syntax(None); // flat colors -> deterministic text assertions
        p.push_block(
            Kind::Assistant,
            "before\n```python\nx = 1\n\ny = 2\n```\nafter".into(),
        );
        p.rebuild(40, &theme::DARK);
        let texts: Vec<&str> = p.wrapped.iter().map(|w| w.text.as_str()).collect();

        // Prose around the block survives unchanged.
        assert!(texts.contains(&"before"));
        assert!(texts.contains(&"after"));
        // Language-labeled top border and a bottom border, full width.
        let top = texts.iter().find(|t| t.starts_with("╭")).expect("top border");
        assert!(top.starts_with("╭─ python "), "label missing: {top}");
        assert_eq!(top.chars().count(), 40, "top border should span the width");
        assert!(texts.iter().any(|t| t.starts_with("╰")), "bottom border");
        // Raw fences are consumed, never rendered.
        assert!(!texts.iter().any(|t| t.contains("```")), "raw ``` leaked");
        // Content is gutter-prefixed; a blank code line keeps the bar.
        assert!(texts.contains(&"│ x = 1"));
        assert!(texts.contains(&"│ y = 2"));
        assert!(texts.contains(&"│"), "blank code line keeps the gutter");
    }

    #[test]
    fn unterminated_fence_auto_closes_the_box() {
        // Mid-stream: opening fence + content but no closing ``` yet.
        let mut p = Pane::new();
        p.set_syntax(None);
        p.push_block(Kind::Assistant, "```rust\nfn main() {}".into());
        p.rebuild(30, &theme::DARK);
        let texts: Vec<&str> = p.wrapped.iter().map(|w| w.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.starts_with("╭─ rust")));
        assert!(texts.contains(&"│ fn main() {}"));
        assert!(texts.iter().any(|t| t.starts_with("╰")), "box auto-closes");
    }

    #[test]
    fn cut_spans_preserves_text_and_colors_across_cuts() {
        let spans = vec![
            (Color::Red, "let x".to_string()),
            (Color::Blue, " = 42;".to_string()),
        ];
        let lines = cut_spans(spans, 6, "  ");
        // 11 content chars at width 6 → two visual lines, each indent-prefixed.
        let text = |l: &Vec<(Color, String)>| l.iter().map(|(_, s)| s.as_str()).collect::<String>();
        assert_eq!(lines.len(), 2);
        assert_eq!(text(&lines[0]), "  let x ");
        assert_eq!(text(&lines[1]), "  = 42;");
        // The char under the cut keeps its span's color.
        assert!(lines[0].iter().any(|(c, s)| *c == Color::Blue && s == " "));
        assert!(lines[1].iter().any(|(c, s)| *c == Color::Blue && s == "= 42;"));
    }

    #[test]
    fn expand_mentions_attaches_outline_and_reports_missing() {
        let dir = std::env::temp_dir().join(format!("rift-mention-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.rs"), "pub fn hello() -> u32 { 41 + 1 }\n").unwrap();

        let (expanded, notes) = expand_mentions("check @lib.rs and @nope.rs please", &dir);
        assert!(expanded.starts_with("check @lib.rs and @nope.rs please"));
        assert!(expanded.contains("pub fn hello() -> u32"), "outline missing: {expanded}");
        assert!(!expanded.contains("41 + 1"), "body should be elided: {expanded}");
        assert!(notes.iter().any(|n| n.contains("nope.rs not found")));
        // No mentions → prompt passes through untouched.
        let (same, notes2) = expand_mentions("no mentions here", &dir);
        assert_eq!(same, "no mentions here");
        assert!(notes2.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn head_of_caps_long_files() {
        let content: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let head = head_of(&content);
        assert!(head.contains("line 79"));
        assert!(!head.contains("line 80\n"));
        assert!(head.contains("120 more lines"));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Transcript,
    Log,
}

/// What the right-hand pane shows: the activity log or the live working-tree
/// diff (Ctrl+D toggles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogView {
    Activity,
    Diff,
}

struct App {
    model: String,
    theme: &'static theme::Theme,
    transcript: Pane,
    log: Pane,
    /// Live working-tree diff, refreshed after each write/edit tool result.
    diff: Pane,
    log_view: LogView,
    /// A write/edit landed since the last diff refresh.
    diff_stale: bool,
    /// A `git diff` task is in flight (don't stack them).
    diff_refreshing: bool,
    show_log: bool,
    focus: Focus,
    /// Project root, for @-mention expansion and file completion.
    cwd: PathBuf,
    /// Lazily built file list backing @-mention completion.
    file_index: Option<Vec<String>>,
    input: String,
    /// Byte offset of the input cursor (always on a char boundary).
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    busy: bool,
    status: String,
    cancel: Option<CancellationToken>,
    quit: bool,
    /// Selected row in the slash-command palette popup.
    palette_idx: usize,
    /// Popup dismissed with Esc; cleared on the next input change.
    palette_off: bool,
    /// Mouse capture on = wheel scrolls panes; off = the terminal's native
    /// text selection works (Ctrl+T toggles).
    mouse_capture: bool,
    /// When the in-flight turn/command started (drives the spinner + elapsed
    /// time so long prompt-processing waits visibly aren't a hang).
    turn_started: Option<Instant>,
    /// Chars streamed (content + thinking) so far this turn, for a live
    /// token estimate in the status line. Providers only report real counts
    /// at end-of-turn, so ~chars/4 stands in until then. Reset each turn.
    turn_output_chars: usize,
    /// Open interactive list (model picker, session picker, elicit choices).
    picker: Option<PickerState>,
    /// Pending free-text ask_user question; Enter sends the input as the answer.
    answering: Option<oneshot::Sender<String>>,
    /// The model's task checklist, pinned at the top of the activity pane.
    plan: Vec<PlanItem>,
    /// Discovered skills — palette entries (/skill:<name>) and prompt bodies.
    skills: Vec<Skill>,
    /// The last prompt actually sent to the agent — re-sent by /retry.
    last_prompt: Option<String>,
    /// Cumulative per-session counters surfaced by /stats.
    session_stats: SessionStats,
}

/// Running per-session counters surfaced by the `/stats` command.
#[derive(Default)]
struct SessionStats {
    turns: u64,
    model_calls: u64,
    output_tokens: u64,
    prompt_tokens: u64,
    tool_calls: u64,
    compactions: u64,
    duration_ms: u128,
}

impl App {
    fn new(model: String, skills: Vec<Skill>, theme: &'static theme::Theme, cwd: PathBuf) -> Self {
        let mut transcript = Pane::new();
        transcript.set_syntax(theme.syntax);
        Self {
            model,
            theme,
            transcript,
            log: Pane::new(),
            diff: Pane::new(),
            log_view: LogView::Activity,
            diff_stale: false,
            diff_refreshing: false,
            show_log: true,
            focus: Focus::Transcript,
            cwd,
            file_index: None,
            input: String::new(),
            cursor: 0,
            history: vec![],
            history_idx: None,
            busy: false,
            status: "Enter send · /help commands · Ctrl+T select/copy · Ctrl+L log · Esc cancel · /quit exit".into(),
            cancel: None,
            quit: false,
            palette_idx: 0,
            palette_off: false,
            mouse_capture: true,
            turn_started: None,
            turn_output_chars: 0,
            picker: None,
            answering: None,
            plan: vec![],
            skills,
            last_prompt: None,
            session_stats: SessionStats::default(),
        }
    }

    /// Entries currently shown in the palette popup (empty = hidden):
    /// (completion text, argument hint, description). Built-in commands plus
    /// discovered skills as /skill:<name>. Live while the user is typing the
    /// command word itself (whitespace = they've moved on to arguments).
    fn palette(&self) -> Vec<(String, String, String)> {
        if self.palette_off || self.picker.is_some() || self.answering.is_some() {
            return vec![];
        }
        if !self.input.starts_with('/') || self.input.contains(char::is_whitespace) {
            return vec![];
        }
        let mut entries: Vec<(String, String, String)> = commands::COMMANDS
            .iter()
            .map(|(n, a, d)| (n.to_string(), a.to_string(), d.to_string()))
            .collect();
        for s in &self.skills {
            entries.push((format!("/skill:{}", s.name), "[task]".into(), s.description.clone()));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.retain(|e| e.0.starts_with(&self.input));
        entries
    }

    /// Route an incoming ask_user question to the right surface: choices get
    /// the picker overlay, free-text questions switch the input into answer mode.
    fn handle_ask(&mut self, req: AskRequest) {
        self.transcript.push_block(Kind::Warn, format!("? {}", req.question));
        if req.choices.is_empty() {
            self.answering = Some(req.reply);
            self.status = "the agent is asking — type your answer, Enter to send, Esc to skip".into();
        } else {
            self.picker = Some(PickerState {
                title: req.question,
                items: req
                    .choices
                    .into_iter()
                    .map(|c| PickerItem { value: c.clone(), label: c, detail: String::new() })
                    .collect(),
                selected: 0,
                kind: PickerKind::Elicit { reply: Some(req.reply) },
            });
        }
    }

    /// Rebuild the transcript from a resumed session's message history.
    fn seed_from_messages(&mut self, messages: &[Message]) {
        for msg in messages {
            match msg.role {
                Role::System => {}
                Role::User => {
                    if !msg.content.starts_with("[system]") {
                        self.transcript.push_block(Kind::User, msg.content.clone());
                    }
                }
                Role::Assistant => {
                    if !msg.content.is_empty() {
                        self.transcript.push_block(Kind::Assistant, msg.content.clone());
                    }
                    for tc in &msg.tool_calls {
                        self.log.push_block(
                            Kind::Tool,
                            format!("→ {} {}", tc.function.name, serde_json::to_string(&tc.function.arguments).unwrap_or_default()),
                        );
                    }
                }
                Role::Tool => {
                    let name = msg.tool_name.as_deref().unwrap_or("?");
                    let ok = !msg.content.starts_with("ERROR:");
                    let preview: String = msg.content.chars().take(120).collect::<String>().replace('\n', " ");
                    self.log.push_block(
                        if ok { Kind::Tool } else { Kind::ToolErr },
                        format!("{} {}: {}", if ok { '✓' } else { '✗' }, name, preview),
                    );
                }
            }
        }
        if !self.transcript.blocks.is_empty() {
            self.transcript.push_block(Kind::Info, "── session resumed ──".into());
        }
    }

    fn focused_pane(&mut self) -> &mut Pane {
        match self.focus {
            Focus::Transcript => &mut self.transcript,
            Focus::Log => self.log_like_pane(),
        }
    }

    /// Whichever pane the right-hand slot currently displays.
    fn log_like_pane(&mut self) -> &mut Pane {
        match self.log_view {
            LogView::Activity => &mut self.log,
            LogView::Diff => &mut self.diff,
        }
    }

    /// File list for @-mention completion: workspace files (ignore-aware),
    /// relative forward-slash paths. Built once per session on first use;
    /// the walk is capped so a giant repo can't wedge the UI thread.
    fn file_index(&mut self) -> &[String] {
        if self.file_index.is_none() {
            let mut v: Vec<String> = ignore::WalkBuilder::new(&self.cwd)
                .build()
                .take(10_000)
                .flatten()
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                .filter_map(|e| {
                    e.path()
                        .strip_prefix(&self.cwd)
                        .ok()
                        .map(|p| p.display().to_string().replace('\\', "/"))
                })
                .collect();
            v.sort();
            self.file_index = Some(v);
        }
        self.file_index.as_deref().unwrap_or_default()
    }

    /// The @-token containing the cursor, if any: (byte start, query).
    fn mention_token(&self) -> Option<(usize, String)> {
        let start = self.input[..self.cursor]
            .rfind(char::is_whitespace)
            .map(|i| i + self.input[i..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0);
        let token = &self.input[start..self.cursor];
        let q = token.strip_prefix('@')?;
        (!q.contains('@')).then(|| (start, q.to_string()))
    }

    /// Completion candidates for the @-token under the cursor.
    fn mention_palette(&mut self) -> Vec<String> {
        if self.palette_off || self.picker.is_some() || self.answering.is_some() {
            return vec![];
        }
        let Some((_, q)) = self.mention_token() else { return vec![] };
        let q = q.to_lowercase();
        let mut hits: Vec<String> = self
            .file_index()
            .iter()
            .filter(|p| p.to_lowercase().contains(&q))
            .take(50)
            .cloned()
            .collect();
        // Filename-prefix matches read as the intended target — surface them.
        hits.sort_by_key(|p| {
            let name = p.rsplit('/').next().unwrap_or(p).to_lowercase();
            (!name.starts_with(&q), p.len())
        });
        hits.truncate(8);
        hits
    }

    /// Replace the @-token under the cursor with the chosen path.
    fn complete_mention(&mut self, path: &str) {
        if let Some((start, _)) = self.mention_token() {
            let replacement = format!("@{path} ");
            self.input.replace_range(start..self.cursor, &replacement);
            self.cursor = start + replacement.len();
            self.palette_idx = 0;
        }
    }

    /// Clear the turn-in-flight state (busy flag, cancel token, spinner clock).
    fn idle(&mut self) {
        self.busy = false;
        self.cancel = None;
        self.turn_started = None;
    }

    /// Compact token readout for the status line: a live estimate of this
    /// turn's output while a turn runs (providers report real counts only at
    /// the end), else the session's cumulative sent (↑) / received (↓) totals.
    /// Empty until there's something to show.
    fn token_status(&self) -> String {
        if self.busy {
            if self.turn_output_chars == 0 {
                return String::new();
            }
            format!(" · ~{}↓", humanize_tokens((self.turn_output_chars / 4) as u64))
        } else {
            let s = &self.session_stats;
            if s.prompt_tokens == 0 && s.output_tokens == 0 {
                return String::new();
            }
            format!(" · Σ {}↑ {}↓", humanize_tokens(s.prompt_tokens), humanize_tokens(s.output_tokens))
        }
    }

    fn handle_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Iteration(i) => {
                // Iteration 1 is the turn's first event; reset the live token
                // estimate so it counts this turn's output (across tool loops).
                if i == 1 {
                    self.turn_output_chars = 0;
                }
                self.log.push_block(Kind::Info, format!("· step {i}"));
                self.status = format!("step {i} — waiting for {}…", self.model);
            }
            AgentEvent::Thinking(t) => {
                self.turn_output_chars += t.chars().count();
                self.transcript.append_stream(Kind::Thinking, &t);
            }
            AgentEvent::Content(c) => {
                self.turn_output_chars += c.chars().count();
                self.transcript.append_stream(Kind::Assistant, &c);
            }
            AgentEvent::ToolStart { name, args } => {
                self.session_stats.tool_calls += 1;
                let args: String = args.chars().take(160).collect();
                self.log.push_block(Kind::Tool, format!("→ {name} {args}"));
                self.status = format!("running {name}…");
            }
            AgentEvent::ToolResult { name, ok, preview } => {
                if ok && matches!(name.as_str(), "write" | "edit") {
                    self.diff_stale = true;
                }
                self.log.push_block(
                    if ok { Kind::Tool } else { Kind::ToolErr },
                    format!("{} {name}: {preview}", if ok { '✓' } else { '✗' }),
                );
            }
            AgentEvent::Info(i) => {
                if i.starts_with("compacted") {
                    self.session_stats.compactions += 1;
                }
                self.log.push_block(Kind::Info, format!("· {i}"));
            }
            AgentEvent::Plan(items) => self.plan = items,
            AgentEvent::Warning(w) => {
                self.log.push_block(Kind::Warn, format!("! {w}"));
                self.transcript.push_block(Kind::Warn, format!("! {w}"));
            }
            AgentEvent::Done(stats) => {
                self.idle();
                // Catch changes made via bash (git checkout, formatters…) too.
                self.diff_stale = true;
                if stats.iterations > 0 {
                    self.session_stats.turns += 1;
                    self.session_stats.model_calls += stats.iterations as u64;
                    self.session_stats.output_tokens += stats.output_tokens;
                    self.session_stats.prompt_tokens += stats.prompt_tokens;
                    self.session_stats.duration_ms += stats.duration_ms;
                }
                self.status = format!(
                    "done — {} steps · {} prompt tok · {} out tok · {:.1} tok/s",
                    stats.iterations, stats.prompt_tokens, stats.output_tokens, stats.tokens_per_sec
                );
            }
        }
    }

    fn handle_ui_effect(&mut self, fx: UiEffect) {
        match fx {
            UiEffect::Picker { title, items, template } => {
                self.picker = Some(PickerState {
                    title,
                    items,
                    selected: 0,
                    kind: PickerKind::Command { template },
                });
            }
            UiEffect::Out(kind, text) => self.transcript.push_block(kind, text),
            UiEffect::Log(kind, text) => self.log.push_block(kind, text),
            UiEffect::Diff(text) => {
                for line in text.lines() {
                    self.transcript.push_line(diff_kind(line), line.to_string());
                }
                self.transcript.push_line(Kind::Info, String::new());
            }
            UiEffect::Clear => {
                self.transcript = Pane::new();
                self.transcript.set_syntax(self.theme.syntax);
                self.log = Pane::new();
                self.diff = Pane::new();
            }
            UiEffect::Seed(messages) => {
                self.transcript = Pane::new();
                self.transcript.set_syntax(self.theme.syntax);
                self.log = Pane::new();
                self.seed_from_messages(&messages);
            }
            UiEffect::TurnDiff(text) => {
                self.diff_refreshing = false;
                let scroll = self.diff.scroll_from_bottom;
                self.diff = Pane::new();
                if text.trim().is_empty() {
                    self.diff.push_line(Kind::Info, "working tree clean — no uncommitted changes".into());
                } else {
                    for line in text.lines().take(4000) {
                        self.diff.push_line(diff_kind(line), line.to_string());
                    }
                }
                // Keep the user's reading position across refreshes; a fresh
                // pane anchors to the top (usize::MAX clamps to max_scroll).
                self.diff.scroll_from_bottom = if scroll == 0 { usize::MAX } else { scroll };
            }
            UiEffect::Model(name) => self.model = name,
            UiEffect::Plan(items) => self.plan = items,
            // Handled by the event loop (need stdout/terminal); never reach here.
            UiEffect::Osc52(_) => {}
            UiEffect::EditFile(_) => {}
            UiEffect::Status(status) => self.status = status,
            UiEffect::Done(status) => {
                self.idle();
                self.status = status;
            }
        }
    }
}

fn input_height(input: &str) -> u16 {
    let lines = input.lines().count().clamp(1, 6) as u16;
    lines + 2 // borders
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [main_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(input_height(&app.input)),
    ])
    .areas(frame.area());

    let (transcript_area, log_area) = if app.show_log && main_area.width >= 60 {
        let [t, l] = Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)]).areas(main_area);
        (t, Some(l))
    } else {
        (main_area, None)
    };

    let t = app.theme;
    let focused_style = Style::default().fg(t.accent);
    let unfocused_style = Style::default().fg(t.muted);

    // Transcript pane.
    {
        let focused = app.focus == Focus::Transcript || log_area.is_none();
        let block = Block::bordered()
            .title(" Rift ")
            .border_style(if focused { focused_style } else { unfocused_style });
        let inner = block.inner(transcript_area);
        app.transcript.area = inner;
        app.transcript.rebuild(inner.width, t);
        app.transcript.view_height = inner.height as usize;
        let lines = app.transcript.visible_lines(t);
        frame.render_widget(block, transcript_area);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    // Right-hand pane (activity log or live diff), with the model's plan
    // checklist pinned on top.
    if let Some(log_area) = log_area {
        let focused = app.focus == Focus::Log;
        let title = match app.log_view {
            LogView::Activity => " activity (Ctrl+D diff) ",
            LogView::Diff => " diff — working tree (Ctrl+D activity) ",
        };
        let block = Block::bordered()
            .title(title)
            .border_style(if focused { focused_style } else { unfocused_style });
        let inner = block.inner(log_area);
        frame.render_widget(block, log_area);

        let plan_rows = if app.plan.is_empty() { 0 } else { app.plan.len().min(6) };
        let plan_h = if plan_rows > 0 { (plan_rows + 1) as u16 } else { 0 }.min(inner.height / 2);
        if plan_h > 0 {
            let rows = (plan_h - 1) as usize;
            let done = app.plan.iter().filter(|i| i.done).count();
            let current = app.plan.iter().position(|i| !i.done);
            // Window the list around the active step when it overflows.
            let start = current
                .unwrap_or(0)
                .saturating_sub(1)
                .min(app.plan.len().saturating_sub(rows));
            let mut plines: Vec<Line> = Vec::with_capacity(rows + 1);
            for (i, item) in app.plan.iter().enumerate().skip(start).take(rows) {
                let (mark, style) = if item.done {
                    ("☑", Style::default().fg(t.muted))
                } else if Some(i) == current {
                    ("◐", Style::default().fg(t.warn))
                } else {
                    ("☐", Style::default().fg(t.tool))
                };
                plines.push(Line::from(Span::styled(format!("{mark} {}", item.text), style)));
            }
            let bar = format!("─ plan {done}/{} {}", app.plan.len(), "─".repeat(inner.width as usize));
            plines.push(Line::from(Span::styled(
                bar.chars().take(inner.width as usize).collect::<String>(),
                Style::default().fg(t.muted),
            )));
            let plan_area = Rect { x: inner.x, y: inner.y, width: inner.width, height: plan_h };
            frame.render_widget(Paragraph::new(plines), plan_area);
        }

        let log_inner = Rect {
            x: inner.x,
            y: inner.y + plan_h,
            width: inner.width,
            height: inner.height.saturating_sub(plan_h),
        };
        // The hidden pane gets a zero area so mouse routing can't hit it.
        match app.log_view {
            LogView::Activity => app.diff.area = Rect::default(),
            LogView::Diff => app.log.area = Rect::default(),
        }
        let pane = app.log_like_pane();
        pane.area = log_inner;
        pane.rebuild(log_inner.width, t);
        pane.view_height = log_inner.height as usize;
        let lines = pane.visible_lines(t);
        frame.render_widget(Paragraph::new(lines), log_inner);
    } else {
        app.log.area = Rect::default();
        app.diff.area = Rect::default();
    }

    // Status line.
    let pane = match app.focus {
        Focus::Transcript => &app.transcript,
        Focus::Log => match app.log_view {
            LogView::Activity => &app.log,
            LogView::Diff => &app.diff,
        },
    };
    let scroll_note = if pane.scroll_from_bottom > 0 {
        format!("  [↑{} — End to follow]", pane.scroll_from_bottom)
    } else {
        String::new()
    };
    let elapsed = app.turn_started.map(|t| t.elapsed());
    let busy_marker = if app.busy {
        const FRAMES: [&str; 4] = [" ◐ ", " ◓ ", " ◑ ", " ◒ "];
        FRAMES[(elapsed.map_or(0, |e| e.as_millis() / 250) % 4) as usize]
    } else {
        " ● "
    };
    let elapsed_note = match (app.busy, elapsed) {
        (true, Some(e)) => format!(" · {}s", e.as_secs()),
        _ => String::new(),
    };
    let status = Line::from(vec![
        Span::styled(format!(" {} ", app.model), Style::default().fg(t.sel_fg).bg(t.accent)),
        Span::styled(busy_marker, Style::default().fg(if app.busy { t.warn } else { t.ok })),
        Span::styled(format!("{}{elapsed_note}", app.status), Style::default().fg(t.muted)),
        Span::styled(app.token_status(), Style::default().fg(t.accent)),
        Span::styled(scroll_note, Style::default().fg(t.warn)),
    ]);
    frame.render_widget(Paragraph::new(status), status_area);

    // Input pane.
    let line_count = app.input.lines().count();
    let input_title = if app.answering.is_some() {
        " your answer (Enter send · Esc skip) ".to_string()
    } else if app.busy {
        " input (Esc cancels the running turn) ".to_string()
    } else if line_count > 1 {
        format!(" input ({line_count} lines — all will be sent) ")
    } else {
        " input ".to_string()
    };
    let input_block = Block::bordered().title(input_title).border_style(unfocused_style);
    let input_inner = input_block.inner(input_area);
    frame.render_widget(input_block, input_area);
    let mut lines: Vec<Line> = Vec::new();
    app.cursor = app.cursor.min(app.input.len());
    let cur_line_idx = app.input[..app.cursor].matches('\n').count();
    let col = app.cursor - line_start(&app.input, app.cursor);
    let input_lines: Vec<&str> = if app.input.is_empty() { vec![""] } else { app.input.split('\n').collect() };
    let shown = input_lines.len().min(input_inner.height as usize).max(1);
    // Window follows the cursor, not just the tail.
    let start = (input_lines.len() - shown).min(cur_line_idx);
    for (i, l) in input_lines[start..].iter().take(shown).enumerate() {
        let abs = start + i;
        let prefix = if abs == 0 { "❯ " } else { "… " };
        let mut spans = vec![Span::styled(prefix, Style::default().fg(t.accent))];
        if abs == cur_line_idx {
            // Split the line at the cursor; render the char under it reversed.
            let split = col.min(l.len());
            let (before, rest) = l.split_at(split);
            let (under, after) = match rest.chars().next() {
                Some(c) => (c.to_string(), &rest[c.len_utf8()..]),
                None => (" ".to_string(), rest),
            };
            spans.push(Span::raw(before.to_string()));
            spans.push(Span::styled(under, Style::default().add_modifier(Modifier::REVERSED)));
            spans.push(Span::raw(after.to_string()));
        } else {
            spans.push(Span::raw((*l).to_string()));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), input_inner);

    // Completion popups anchored above the input pane, filtered live by
    // what's typed; Up/Down select, Tab completes, Enter runs (commands).
    // Commands (input starts with '/') and @-file mentions are mutually
    // exclusive by construction.
    let palette = app.palette();
    let mention = if palette.is_empty() { app.mention_palette() } else { vec![] };
    let (entries, popup_title): (Vec<(String, String, String)>, &str) = if !palette.is_empty() {
        (palette, " commands — ↑↓ select · Tab complete · Enter run · Esc dismiss ")
    } else {
        (
            mention.into_iter().map(|p| (format!("@{p}"), String::new(), String::new())).collect(),
            " files — ↑↓ select · Tab attach outline · Esc dismiss ",
        )
    };
    if !entries.is_empty() {
        app.palette_idx = app.palette_idx.min(entries.len() - 1);
        let rows = entries.len().min(8);
        let height = (rows + 2) as u16;
        let width = (frame.area().width.saturating_sub(2)).min(72);
        let area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(height),
            width,
            height,
        };
        // Keep the selection inside the visible window when the list scrolls.
        let start = (app.palette_idx + 1).saturating_sub(rows);
        let mut lines: Vec<Line> = Vec::with_capacity(rows);
        for (row, (name, args, desc)) in entries.iter().enumerate().skip(start).take(rows) {
            let selected = row == app.palette_idx;
            let left = if args.is_empty() { name.to_string() } else { format!("{name} {args}") };
            let base = if selected {
                Style::default().bg(t.accent).fg(t.sel_fg)
            } else {
                Style::default()
            };
            let pad = (width as usize).saturating_sub(2 + 30 + desc.len());
            lines.push(Line::from(vec![
                Span::styled(format!(" {left:<29} "), base.add_modifier(Modifier::BOLD)),
                Span::styled(desc.to_string(), if selected { base } else { Style::default().fg(t.muted) }),
                Span::styled(" ".repeat(pad), base),
            ]));
        }
        let block = Block::bordered().title(popup_title).border_style(Style::default().fg(t.accent));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    // Interactive picker overlay (model/session selection, elicit choices) —
    // drawn last so it sits on top of everything.
    if let Some(p) = &mut app.picker {
        p.selected = p.selected.min(p.items.len().saturating_sub(1));
        let rows = p.items.len().min(10);
        let height = (rows + 2) as u16;
        let width = (frame.area().width.saturating_sub(8)).clamp(40, 90);
        let area = Rect {
            x: (frame.area().width.saturating_sub(width)) / 2,
            y: input_area.y.saturating_sub(height),
            width,
            height,
        };
        let start = (p.selected + 1).saturating_sub(rows);
        let inner_w = width.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = Vec::with_capacity(rows);
        for (row, item) in p.items.iter().enumerate().skip(start).take(rows) {
            let selected = row == p.selected;
            let base = if selected {
                Style::default().bg(t.accent).fg(t.sel_fg)
            } else {
                Style::default()
            };
            let label: String = item.label.chars().take(inner_w.saturating_sub(4)).collect();
            let detail: String = item.detail.chars().take(inner_w.saturating_sub(label.len() + 5)).collect();
            let pad = inner_w.saturating_sub(2 + label.chars().count() + if detail.is_empty() { 0 } else { detail.chars().count() + 2 });
            let mut spans = vec![Span::styled(format!(" {label} "), base.add_modifier(Modifier::BOLD))];
            if !detail.is_empty() {
                spans.push(Span::styled(
                    format!(" {detail}"),
                    if selected { base } else { Style::default().fg(t.muted) },
                ));
            }
            spans.push(Span::styled(" ".repeat(pad), base));
            lines.push(Line::from(spans));
        }
        let title: String = p.title.chars().take(inner_w.saturating_sub(28)).collect();
        let block = Block::bordered()
            .title(format!(" {title} — ↑↓ · Enter · Esc "))
            .border_style(Style::default().fg(t.warn));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Expand `@path` tokens: for each existing file, append an outline (or a
/// capped head for unsupported types) to the prompt. Token-stingy on purpose
/// — the model can still `read` for exact lines. Returns the expanded prompt
/// and activity-log notes about what was attached.
fn expand_mentions(input: &str, cwd: &std::path::Path) -> (String, Vec<String>) {
    let mut expanded = input.to_string();
    let mut notes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for token in input.split_whitespace() {
        let Some(rel) = token.strip_prefix('@') else { continue };
        let rel = rel.trim_end_matches([',', '.', ';', ':', '!', '?', ')']);
        if rel.is_empty() || !seen.insert(rel.to_string()) {
            continue;
        }
        let path = if std::path::Path::new(rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            cwd.join(rel)
        };
        if !path.is_file() {
            notes.push(format!("@{rel} not found — sent as plain text"));
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            notes.push(format!("@{rel} unreadable (binary?) — sent as plain text"));
            continue;
        };
        let (attach, label) = if rift_core::outline::supports(&path) {
            match rift_core::outline::outline_source(&path, &content) {
                Ok(o) => (o, "outline"),
                Err(_) => (head_of(&content), "head"),
            }
        } else {
            (head_of(&content), "head")
        };
        let attach: String = attach.chars().take(6000).collect();
        expanded.push_str(&format!("\n\n[attached {label} of {rel} — mentioned as @{rel}]\n{attach}"));
        notes.push(format!("attached {label} of {rel} ({} chars)", attach.chars().count()));
    }
    (expanded, notes)
}

/// First lines of a file, for @-mentions of types the outliner doesn't know.
fn head_of(content: &str) -> String {
    let total = content.lines().count();
    let mut out: String = content.lines().take(80).collect::<Vec<_>>().join("\n");
    if total > 80 {
        out.push_str(&format!("\n[... {} more lines — use the read tool for the rest]", total - 80));
    }
    out
}

/// The `/init` command body — a normal agent turn with a canned prompt.
const INIT_PROMPT: &str = "Explore this repository (use repo_map and outline to stay cheap; read key files only as needed) and write a RIFT.md file at the project root: a concise guide for AI coding agents working here. Cover: what the project does, how the code is laid out, how to build/test/run it, and any conventions or gotchas you noticed. Keep it under 60 lines.";

/// Everything run_tui needs beyond the agent itself.
pub struct TuiOptions {
    pub model: String,
    pub store: SessionStore,
    pub resumed: Vec<Message>,
    pub mcp: Vec<(String, usize)>,
    pub config_path: Option<PathBuf>,
    pub ask_rx: mpsc::UnboundedReceiver<AskRequest>,
    pub skills: Vec<Skill>,
    pub host: String,
    pub providers: std::collections::HashMap<String, rift_core::ProviderConfig>,
    pub theme: &'static theme::Theme,
}

pub async fn run_tui(agent: Agent, opts: TuiOptions) -> Result<()> {
    let TuiOptions { model, store, resumed, mcp, config_path, mut ask_rx, skills, host, providers, theme } = opts;
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (fx_tx, mut fx_rx) = mpsc::unbounded_channel::<UiEffect>();
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<UiMsg>();

    // Non-blocking update check: at most one network call per 24h, silent
    // unless a newer release actually exists.
    let fx_update = fx_tx.clone();
    tokio::spawn(async move {
        let current = env!("CARGO_PKG_VERSION");
        if let Some(latest) = crate::update::check_for_update(current).await {
            let _ = fx_update.send(UiEffect::Out(
                Kind::Info,
                format!("⬆ rift v{latest} is available (you have v{current}) — run /update or `rift update`"),
            ));
        }
    });

    let cwd = std::env::current_dir()?;
    let cwd_ui = cwd.clone();
    // UI-side effect sender: for commands the UI handles itself (/copy log).
    let fx_ui = fx_tx.clone();
    let agent_task = tokio::spawn(async move {
        let mut agent = agent;
        let mut cx = CmdCx { store, cwd: cwd.clone(), mcp, config_path, host, providers };
        let cwd_str = cwd.display().to_string();
        while let Some(msg) = prompt_rx.recv().await {
            match msg {
                UiMsg::Prompt(prompt, cancel) => {
                    if let Err(e) = agent.run_turn(&prompt, &ev_tx, &cancel).await {
                        let _ = ev_tx.send(AgentEvent::Warning(format!("error: {e:#}")));
                        let _ = ev_tx.send(AgentEvent::Done(TurnStats::default()));
                    }
                    if let Err(e) = cx.store.save(&agent.cfg.model, &cwd_str, &agent.messages) {
                        let _ = ev_tx.send(AgentEvent::Warning(format!("session save failed: {e:#}")));
                    }
                }
                UiMsg::Command(line, cancel) => {
                    commands::run_command(&line, &mut agent, &mut cx, &fx_tx, &cancel).await;
                }
            }
        }
    });

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
    // ratatui's panic hook restores raw mode/alt screen only; also turn off
    // mouse capture + bracketed paste so a panic doesn't leave the shell
    // spewing mouse-escape garbage.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        prev_hook(info);
    }));

    let mut app = App::new(model, skills, theme, cwd_ui.clone());
    app.seed_from_messages(&resumed);

    let result = (|| -> Result<()> {
        let mut needs_redraw = true;
        let mut last_tick = Instant::now();
        loop {
            while let Ok(ev) = ev_rx.try_recv() {
                app.handle_agent_event(ev);
                needs_redraw = true;
            }
            while let Ok(req) = ask_rx.try_recv() {
                app.handle_ask(req);
                needs_redraw = true;
            }
            while let Ok(fx) = fx_rx.try_recv() {
                if let UiEffect::Osc52(text) = fx {
                    // Clipboard escape goes straight to the terminal; it
                    // doesn't move the cursor so the frame stays intact.
                    use std::io::Write;
                    let mut out = stdout();
                    let _ = out.write_all(crate::clipboard::osc52(&text).as_bytes());
                    let _ = out.flush();
                } else if let UiEffect::EditFile(path) = fx {
                    // Hand the terminal to $EDITOR, then take it back.
                    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
                    ratatui::restore();
                    let status = {
                        #[cfg(windows)]
                        let editor = std::env::var("EDITOR")
                            .or_else(|_| std::env::var("VISUAL"))
                            .unwrap_or_else(|_| "notepad".into());
                        #[cfg(not(windows))]
                        let editor = std::env::var("EDITOR")
                            .or_else(|_| std::env::var("VISUAL"))
                            .unwrap_or_else(|_| "vi".into());
                        // Direct spawn first: std handles spaced paths (and .cmd/.bat
                        // on Windows) without shell-quoting pitfalls. Fall back to a
                        // shell only when the spawn itself fails, e.g. EDITOR embeds
                        // flags like "code -w".
                        std::process::Command::new(&editor).arg(&path).status().or_else(|_| {
                            #[cfg(windows)]
                            {
                                let shell = std::env::var("COMSPEC")
                                    .unwrap_or_else(|_| "cmd.exe".into());
                                std::process::Command::new(shell)
                                    .arg("/C")
                                    .arg(format!("{editor} \"{}\"", path.display()))
                                    .status()
                            }
                            #[cfg(not(windows))]
                            {
                                std::process::Command::new("sh")
                                    .arg("-c")
                                    .arg(format!("{editor} '{}'", path.display()))
                                    .status()
                            }
                        })
                    };
                    terminal = ratatui::init();
                    let _ = execute!(stdout(), EnableBracketedPaste);
                    if app.mouse_capture {
                        let _ = execute!(stdout(), EnableMouseCapture);
                    }
                    app.transcript.dirty = true;
                    app.log.dirty = true;
                    app.diff.dirty = true;
                    match status {
                        Ok(s) if s.success() => {
                            let _ = prompt_tx
                                .send(UiMsg::Command("/config reload".into(), CancellationToken::new()));
                        }
                        _ => app
                            .transcript
                            .push_block(Kind::Warn, "! editor exited with an error; config not reloaded".into()),
                    }
                } else {
                    app.handle_ui_effect(fx);
                }
                needs_redraw = true;
            }
            // Live diff refresh: a write/edit landed (or the view was just
            // opened) — run `git diff` off-thread and feed it back as an
            // effect. Guarded so refreshes never stack.
            if app.diff_stale && !app.diff_refreshing && app.log_view == LogView::Diff {
                app.diff_stale = false;
                app.diff_refreshing = true;
                let fx2 = fx_ui.clone();
                let dir = cwd_ui.clone();
                tokio::spawn(async move {
                    let text = match tokio::process::Command::new("git")
                        .args(["diff"])
                        .current_dir(&dir)
                        .output()
                        .await
                    {
                        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
                        Ok(o) => String::from_utf8_lossy(&o.stderr).to_string(),
                        Err(e) => format!("git diff failed: {e}"),
                    };
                    let _ = fx2.send(UiEffect::TurnDiff(text));
                });
            }
            // Defer the (expensive) full redraw while more input is already
            // queued, so a burst of events is consumed in a single frame. This
            // matters most on Windows: crossterm has no bracketed paste there,
            // so a paste arrives as a flood of individual key events. Redrawing
            // between each one lets the app fall behind the finite console input
            // buffer, which then silently drops the tail of long pastes. Drain
            // the queue first, draw once — no more truncated pastes.
            if needs_redraw && !event::poll(Duration::from_millis(0))? {
                terminal.draw(|f| draw(f, &mut app))?;
                needs_redraw = false;
            }
            if app.quit {
                return Ok(());
            }

            if !event::poll(Duration::from_millis(16))? {
                // No input — but while a turn runs, tick the spinner/elapsed
                // display at 4Hz so long model waits visibly aren't a hang.
                if app.busy && last_tick.elapsed() >= Duration::from_millis(250) {
                    last_tick = Instant::now();
                    needs_redraw = true;
                }
                continue;
            }
            // Only state-changing events trigger a redraw — mouse capture
            // floods us with Moved events, and redrawing on each pegs a core.
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    needs_redraw = true;
                    let palette = app.palette();
                    let mention = if palette.is_empty() { app.mention_palette() } else { vec![] };
                    let popup_len = palette.len().max(mention.len());
                    // An open picker owns the keyboard.
                    if app.picker.is_some() {
                        match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                // Ctrl+C no longer exits; it just dismisses the picker.
                                if let Some(p) = app.picker.take() {
                                    if matches!(p.kind, PickerKind::Elicit { .. }) {
                                        app.transcript.push_block(Kind::Info, "(question dismissed)".into());
                                    }
                                }
                                app.status = "picker closed".into();
                            }
                            KeyCode::Up => {
                                if let Some(p) = app.picker.as_mut() {
                                    p.selected = p.selected.saturating_sub(1);
                                }
                            }
                            KeyCode::Down => {
                                if let Some(p) = app.picker.as_mut() {
                                    p.selected = (p.selected + 1).min(p.items.len().saturating_sub(1));
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(p) = app.picker.take() {
                                    let value =
                                        p.items.get(p.selected).map(|i| i.value.clone()).unwrap_or_default();
                                    match p.kind {
                                        PickerKind::Command { template } => {
                                            if app.busy {
                                                app.status = "agent is running — Esc to cancel first".into();
                                            } else if !value.is_empty() {
                                                let line = template.replace("{}", &value);
                                                app.transcript.push_block(Kind::User, line.clone());
                                                app.history.push(line.clone());
                                                app.history_idx = None;
                                                app.busy = true;
                                                app.turn_started = Some(Instant::now());
                                                app.status = "running command…".into();
                                                let cancel = CancellationToken::new();
                                                app.cancel = Some(cancel.clone());
                                                let _ = prompt_tx.send(UiMsg::Command(line, cancel));
                                            }
                                        }
                                        PickerKind::Elicit { mut reply } => {
                                            app.transcript.push_block(Kind::User, value.clone());
                                            if let Some(tx) = reply.take() {
                                                let _ = tx.send(value);
                                            }
                                            app.status = "answer sent".into();
                                        }
                                    }
                                }
                            }
                            KeyCode::Esc => {
                                if let Some(p) = app.picker.take() {
                                    // Dropping an Elicit reply sender tells the tool
                                    // the user dismissed the question.
                                    if matches!(p.kind, PickerKind::Elicit { .. }) {
                                        app.transcript.push_block(Kind::Info, "(question dismissed)".into());
                                    }
                                    app.status = "picker closed".into();
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl+C no longer exits; that is what /quit is for. If a turn is
                        // running, interrupt it (like Esc); otherwise remind the user.
                        if app.busy {
                            if let Some(cancel) = &app.cancel {
                                cancel.cancel();
                            }
                            app.status = "cancelling… (type /quit to exit)".into();
                        } else {
                            app.status = "Ctrl+C won't exit; type /quit to quit".into();
                        }
                    }
                    KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.show_log = !app.show_log;
                        if !app.show_log {
                            app.focus = Focus::Transcript;
                        }
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Toggle the right-hand pane between activity and the
                        // live working-tree diff.
                        app.log_view = match app.log_view {
                            LogView::Activity => LogView::Diff,
                            LogView::Diff => LogView::Activity,
                        };
                        if app.log_view == LogView::Diff {
                            app.show_log = true;
                            if app.diff.is_empty() {
                                app.diff_stale = true;
                            }
                            app.status = "diff view — refreshes as the agent edits · Ctrl+D back".into();
                        }
                    }
                    KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.mouse_capture = !app.mouse_capture;
                        if app.mouse_capture {
                            let _ = execute!(stdout(), EnableMouseCapture);
                            app.status = "mouse capture on — wheel scrolls panes".into();
                        } else {
                            let _ = execute!(stdout(), DisableMouseCapture);
                            app.status =
                                "mouse capture off — select & copy text natively · Ctrl+T to re-enable scrolling".into();
                        }
                    }
                    KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.input.insert(app.cursor, '\n');
                        app.cursor += 1;
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.cursor = line_start(&app.input, app.cursor);
                    }
                    KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.cursor = line_end(&app.input, app.cursor);
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.input.clear();
                        app.cursor = 0;
                        app.palette_off = false;
                        app.palette_idx = 0;
                    }
                    KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let from = prev_word(&app.input, app.cursor);
                        app.input.replace_range(from..app.cursor, "");
                        app.cursor = from;
                    }
                    KeyCode::Tab => {
                        if let Some((name, args, _)) = palette.get(app.palette_idx.min(palette.len().saturating_sub(1))) {
                            // Complete to the selected command; trailing space
                            // if it takes arguments (which also hides the popup).
                            app.input = if args.is_empty() { name.clone() } else { format!("{name} ") };
                            app.cursor = app.input.len();
                        } else if let Some(path) = mention.get(app.palette_idx.min(mention.len().saturating_sub(1))) {
                            let path = path.clone();
                            app.complete_mention(&path);
                        } else {
                            app.focus = match app.focus {
                                Focus::Transcript if app.show_log => Focus::Log,
                                _ => Focus::Transcript,
                            };
                        }
                    }
                    KeyCode::Esc => {
                        if popup_len > 0 {
                            app.palette_off = true;
                        } else if app.answering.is_some() {
                            // Dropping the sender tells the tool the user skipped.
                            app.answering = None;
                            app.input.clear();
                            app.cursor = 0;
                            app.transcript.push_block(Kind::Info, "(question dismissed)".into());
                            app.status = "question dismissed — the agent will proceed".into();
                        } else if app.busy {
                            if let Some(cancel) = &app.cancel {
                                cancel.cancel();
                                app.status = "cancelling…".into();
                            }
                        } else {
                            app.focused_pane().scroll_from_bottom = 0;
                        }
                    }
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                        app.input.insert(app.cursor, '\n');
                        app.cursor += 1;
                    }
                    KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.cursor = prev_word(&app.input, app.cursor);
                    }
                    KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.cursor = next_word(&app.input, app.cursor);
                    }
                    KeyCode::Left => app.cursor = prev_char(&app.input, app.cursor),
                    KeyCode::Right => app.cursor = next_char(&app.input, app.cursor),
                    KeyCode::Delete => {
                        if app.cursor < app.input.len() {
                            let to = next_char(&app.input, app.cursor);
                            app.input.replace_range(app.cursor..to, "");
                            app.palette_off = false;
                            app.palette_idx = 0;
                        }
                    }
                    KeyCode::Enter => {
                        if app.answering.is_some() {
                            // The input is the answer to a pending ask_user question.
                            if !app.input.trim().is_empty() {
                                let answer = std::mem::take(&mut app.input);
                                app.cursor = 0;
                                app.transcript.push_block(Kind::User, answer.clone());
                                if let Some(tx) = app.answering.take() {
                                    let _ = tx.send(answer);
                                }
                                app.status = "answer sent".into();
                            }
                        } else if app.busy {
                            app.status = "agent is running — Esc to cancel first".into();
                        } else if !app.input.trim().is_empty() {
                            // With the palette open, Enter runs the selected
                            // command (completing any partial prefix first).
                            if let Some((name, _, _)) = palette.get(app.palette_idx.min(palette.len().saturating_sub(1))) {
                                app.input = name.clone();
                            }
                            let raw = std::mem::take(&mut app.input);
                            app.cursor = 0;
                            app.history.push(raw.clone());
                            app.history_idx = None;
                            app.transcript.push_block(Kind::User, raw.clone());
                            app.busy = true;
                            app.turn_started = Some(Instant::now());
                            app.transcript.scroll_from_bottom = 0;
                            app.log.scroll_from_bottom = 0;
                            let cancel = CancellationToken::new();
                            app.cancel = Some(cancel.clone());
                            let trimmed = raw.trim().to_string();
                            if trimmed == "/init" {
                                // Syntactic sugar: a canned prompt through the normal agent loop.
                                app.status = "generating RIFT.md…".into();
                                let _ = prompt_tx.send(UiMsg::Prompt(INIT_PROMPT.to_string(), cancel));
                            } else if trimmed.strip_prefix("/copy").is_some_and(|r| r.trim() == "log") {
                                // Handled UI-side: the activity pane lives here,
                                // not in the agent's message history.
                                app.idle();
                                let text = app.log.raw_text();
                                if text.trim().is_empty() {
                                    app.transcript.push_block(Kind::Warn, "! activity log is empty".into());
                                } else {
                                    let fx2 = fx_ui.clone();
                                    tokio::spawn(async move {
                                        let chars = text.chars().count();
                                        let msg = match crate::clipboard::copy_via_tool(&text).await {
                                            Some(tool) => format!("copied {chars} chars of activity log (via {tool})"),
                                            None => {
                                                let _ = fx2.send(UiEffect::Osc52(text));
                                                format!("sent {chars} chars to the terminal clipboard (OSC 52)")
                                            }
                                        };
                                        let _ = fx2.send(UiEffect::Out(Kind::Info, msg.clone()));
                                        // Status only — a Done here would reset the
                                        // busy/cancel state of a turn started meanwhile.
                                        let _ = fx2.send(UiEffect::Status(msg));
                                    });
                                }
                            } else if let Some(rest) = trimmed.strip_prefix("/skill:") {
                                // Skill invocation: expand the body into a prompt.
                                let (name, task) = match rest.split_once(char::is_whitespace) {
                                    Some((n, t)) => (n, t.trim()),
                                    None => (rest, ""),
                                };
                                match app.skills.iter().find(|s| s.name == name) {
                                    Some(s) => {
                                        let task = if task.is_empty() {
                                            "Apply this skill to the current project now."
                                        } else {
                                            task
                                        };
                                        let prompt = format!(
                                            "Follow this skill's instructions.\n\n--- SKILL: {} ---\n{}\n--- END SKILL ---\n\nTask: {task}",
                                            s.name, s.body
                                        );
                                        app.status = format!("running skill {name}…");
                                        let _ = prompt_tx.send(UiMsg::Prompt(prompt, cancel));
                                    }
                                    None => {
                                        app.idle();
                                        app.transcript.push_block(
                                            Kind::Warn,
                                            format!("! unknown skill '{name}' — /skills lists what's available"),
                                        );
                                    }
                                }
                            } else if trimmed == "/skills" {
                                app.idle();
                                if app.skills.is_empty() {
                                    app.transcript.push_block(
                                        Kind::Info,
                                        "no skills found — add .rift/skills/<name>/SKILL.md (project) or ~/.config/rift/skills/ (user)".into(),
                                    );
                                } else {
                                    let mut out = String::from("skills (run with /skill:<name> [task]):\n");
                                    for s in &app.skills {
                                        out.push_str(&format!("  {:<18} {}  ({})\n", s.name, s.description, s.source.display()));
                                    }
                                    app.transcript.push_block(Kind::Info, out.trim_end().to_string());
                                }
                            } else if trimmed == "/theme" || trimmed.starts_with("/theme ") {
                                // UI-side: the theme is pure presentation state.
                                app.idle();
                                let arg = trimmed.strip_prefix("/theme").unwrap_or_default().trim();
                                if arg.is_empty() {
                                    app.transcript.push_block(
                                        Kind::Info,
                                        format!(
                                            "themes: {} (current: {})\nswitch with /theme <name>; persist with \"theme\": \"<name>\" in config",
                                            theme::names().join(", "),
                                            app.theme.name
                                        ),
                                    );
                                } else if let Some(th) = theme::find(arg) {
                                    app.theme = th;
                                    // Cached code spans hold the old syntect colors.
                                    app.transcript.set_syntax(th.syntax);
                                    app.transcript.dirty = true;
                                    app.log.dirty = true;
                                    app.diff.dirty = true;
                                    app.status = format!("theme: {}", th.name);
                                } else {
                                    app.transcript.push_block(
                                        Kind::Warn,
                                        format!("! unknown theme '{arg}' — available: {}", theme::names().join(", ")),
                                    );
                                }
                            } else if trimmed == "/quit" {
                                app.quit = true;
                            } else if trimmed == "/retry" {
                                if let Some(p) = app.last_prompt.clone() {
                                    app.status = "retrying…".into();
                                    app.transcript.push_block(
                                        Kind::Info,
                                        format!("↻ {}", p.chars().take(80).collect::<String>()),
                                    );
                                    let _ = prompt_tx.send(UiMsg::Prompt(p, cancel));
                                } else {
                                    app.idle();
                                    app.transcript.push_block(Kind::Warn, "! nothing to retry yet".into());
                                }
                            } else if trimmed == "/stats" {
                                app.idle();
                                let out = format!(
                                    "session stats:\n  turns:         {}\n  model calls:   {}\n  tool calls:    {}\n  compactions:   {}\n  output tokens: {}\n  prompt tokens: {} (last-of-turn, summed)\n  model time:    {:.1}s",
                                    app.session_stats.turns,
                                    app.session_stats.model_calls,
                                    app.session_stats.tool_calls,
                                    app.session_stats.compactions,
                                    app.session_stats.output_tokens,
                                    app.session_stats.prompt_tokens,
                                    app.session_stats.duration_ms as f64 / 1000.0,
                                );
                                app.transcript.push_block(Kind::Info, out);
                            } else if trimmed.starts_with('/') {
                                app.status = "running command…".into();
                                let _ = prompt_tx.send(UiMsg::Command(trimmed, cancel));
                            } else {
                                app.status = "sending…".into();
                                // Expand @-mentions into attached outlines so
                                // the model sees structure without the tokens
                                // of a full file read.
                                let (expanded, notes) = expand_mentions(&raw, &app.cwd);
                                for n in notes {
                                    app.log.push_block(Kind::Info, format!("· {n}"));
                                }
                                app.last_prompt = Some(expanded.clone());
                                let _ = prompt_tx.send(UiMsg::Prompt(expanded, cancel));
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if app.cursor > 0 {
                            let from = prev_char(&app.input, app.cursor);
                            app.input.replace_range(from..app.cursor, "");
                            app.cursor = from;
                        }
                        app.palette_off = false;
                        app.palette_idx = 0;
                    }
                    KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
                        if !app.history.is_empty() {
                            let idx = match app.history_idx {
                                Some(0) => 0,
                                Some(i) => i - 1,
                                None => app.history.len() - 1,
                            };
                            app.history_idx = Some(idx);
                            app.input = app.history[idx].clone();
                            app.cursor = app.input.len();
                            app.palette_off = false;
                            app.palette_idx = 0;
                        }
                    }
                    KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                        match app.history_idx {
                            Some(i) if i + 1 < app.history.len() => {
                                app.history_idx = Some(i + 1);
                                app.input = app.history[i + 1].clone();
                                app.cursor = app.input.len();
                            }
                            Some(_) => {
                                app.history_idx = None;
                                app.input.clear();
                                app.cursor = 0;
                            }
                            None => {}
                        }
                    }
                    KeyCode::PageUp => {
                        let page = app.focused_pane().view_height.saturating_sub(1).max(1);
                        app.focused_pane().scroll_up(page);
                    }
                    KeyCode::PageDown => {
                        let page = app.focused_pane().view_height.saturating_sub(1).max(1);
                        app.focused_pane().scroll_down(page);
                    }
                    KeyCode::Up => {
                        if popup_len == 0 {
                            app.focused_pane().scroll_up(1);
                        } else {
                            app.palette_idx = app.palette_idx.min(popup_len - 1).saturating_sub(1);
                        }
                    }
                    KeyCode::Down => {
                        if popup_len == 0 {
                            app.focused_pane().scroll_down(1);
                        } else {
                            app.palette_idx = (app.palette_idx + 1).min(popup_len - 1);
                        }
                    }
                    KeyCode::Home => {
                        // Editing-first: jump to line start when typing,
                        // pane top when the input is empty.
                        if app.input.is_empty() {
                            let max = app.focused_pane().max_scroll();
                            app.focused_pane().scroll_from_bottom = max;
                        } else {
                            app.cursor = line_start(&app.input, app.cursor);
                        }
                    }
                    KeyCode::End => {
                        if app.input.is_empty() {
                            app.focused_pane().scroll_from_bottom = 0;
                        } else {
                            app.cursor = line_end(&app.input, app.cursor);
                        }
                    }
                    KeyCode::Char(c) => {
                        app.input.insert(app.cursor, c);
                        app.cursor += c.len_utf8();
                        app.palette_off = false;
                        app.palette_idx = 0;
                    }
                    _ => {}
                    }
                }
                Event::Paste(data) => {
                    needs_redraw = true;
                    // Bracketed paste: the whole clipboard arrives as ONE event,
                    // so embedded newlines never act as Enter presses mid-paste.
                    // Terminals encode paste newlines as \r — normalize to \n.
                    let data = data.replace("\r\n", "\n").replace('\r', "\n");
                    app.input.insert_str(app.cursor, &data);
                    app.cursor += data.len();
                    app.palette_off = false;
                    app.palette_idx = 0;
                }
                Event::Mouse(mouse) => {
                    // Route the wheel to the pane under the cursor (the hidden
                    // log-slot pane has a zero area, so at most one matches).
                    let pane = if app.log.contains(mouse.column, mouse.row) {
                        &mut app.log
                    } else if app.diff.contains(mouse.column, mouse.row) {
                        &mut app.diff
                    } else if app.transcript.contains(mouse.column, mouse.row) {
                        &mut app.transcript
                    } else {
                        app.focused_pane()
                    };
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            pane.scroll_up(3);
                            needs_redraw = true;
                        }
                        MouseEventKind::ScrollDown => {
                            pane.scroll_down(3);
                            needs_redraw = true;
                        }
                        _ => {}
                    }
                }
                Event::Resize(_, _) => {
                    needs_redraw = true;
                    app.transcript.dirty = true;
                    app.log.dirty = true;
                    app.diff.dirty = true;
                }
                _ => {}
            }
        }
    })();

    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    agent_task.abort();
    result
}
