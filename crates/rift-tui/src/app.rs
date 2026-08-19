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
use rift_ollama::{ChatOptions, ChatRequest, Message, Role, StreamDelta};
use tokio::sync::oneshot;

use crate::commands::{self, CmdCx, PickerItem, UiEffect};
use crate::theme;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
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
    /// A markdown heading inside assistant text (`## …`) — bold accent.
    Heading,
    Code,
    Thinking,
    Tool,
    ToolErr,
    Warn,
    Info,
    /// Startup ASCII banner — regenerated to fit the pane width on every
    /// re-wrap (see the Kind::Logo branch in Pane::rebuild), accent-colored.
    Logo,
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
        Kind::Heading => Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        Kind::Code => Style::default().fg(t.code),
        Kind::Thinking => Style::default().fg(t.muted).add_modifier(Modifier::ITALIC),
        Kind::Tool => Style::default().fg(t.tool),
        Kind::ToolErr => Style::default().fg(t.error),
        Kind::Warn => Style::default().fg(t.warn),
        Kind::Info => Style::default().fg(t.muted),
        Kind::Logo => Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
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

/// RIFT rendered on a 5-row pixel grid ('#' = on). Each pixel draws as a
/// 2·s × s block of █, so letters keep their shape at every scale and the
/// banner grows with the terminal instead of arriving at one fixed size.
const LOGO_GRID: [&str; 5] = [
    "####  ### ##### #####",
    "#   #  #  #       #  ",
    "####   #  ####    #  ",
    "#  #   #  #       #  ",
    "#   # ### #       #  ",
];

/// Fallback for panes too narrow for even a scale-1 pixel render.
const LOGO_SMALL: [&str; 6] = [
    "██████╗ ██╗███████╗████████╗",
    "██╔══██╗██║██╔════╝╚══██╔══╝",
    "██████╔╝██║█████╗     ██║",
    "██╔══██╗██║██╔══╝     ██║",
    "██║  ██║██║██║        ██║",
    "╚═╝  ╚═╝╚═╝╚═╝        ╚═╝",
];

/// Largest logo variant that fits in `w` columns: scaled pixel grid (capped
/// at 2× so it stays a banner, not a wall), then the compact box-drawing
/// version, then plain text when the pane can't fit any art at all.
fn logo_art(w: usize) -> Vec<String> {
    let grid_w = LOGO_GRID[0].len(); // 21 pixels
    let scale = (w / (grid_w * 2)).min(2);
    if scale >= 1 {
        let mut out = Vec::with_capacity(LOGO_GRID.len() * scale);
        for row in LOGO_GRID {
            let mut line = String::new();
            for px in row.chars() {
                let cell = if px == '#' { '█' } else { ' ' };
                for _ in 0..(2 * scale) {
                    line.push(cell);
                }
            }
            let line = line.trim_end().to_string();
            for _ in 0..scale {
                out.push(line.clone());
            }
        }
        out
    } else if w >= LOGO_SMALL[0].chars().count() {
        LOGO_SMALL.iter().map(|s| s.to_string()).collect()
    } else {
        vec!["r i f t".into()]
    }
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

/// A mouse text selection inside a pane, in CONTENT coordinates: (wrapped-line
/// index, char column). Content coordinates — not screen rows — so the
/// selection stays anchored to its text while the pane scrolls under it, and
/// while new output streams in below.
#[derive(Clone, Copy)]
pub(crate) struct Sel {
    anchor: (usize, usize),
    cursor: (usize, usize),
}

impl Sel {
    /// (start, end) in reading order — the anchor may be after the cursor when
    /// the drag went up or leftwards.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
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
    /// Live drag-selection, highlighted on screen and copied on mouse-up.
    pub(crate) sel: Option<Sel>,
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
            sel: None,
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
            // Every line index is about to move — a selection held across a
            // re-wrap would highlight (and copy) unrelated text.
            self.sel = None;
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
            // The startup banner is generated here, not stored: every re-wrap
            // picks the largest variant that fits the current width and
            // centers it, so a resize re-renders the logo instead of
            // hard-cutting a fixed-size one into garbage. The block text is
            // the version tagline, centered beneath the art.
            if block.kind == Kind::Logo {
                let art = logo_art(w);
                let art_w = art.iter().map(|l| l.chars().count()).max().unwrap_or(0);
                let pad = " ".repeat(w.saturating_sub(art_w) / 2);
                for line in art {
                    self.wrapped.push(WrappedLine::plain(Kind::Logo, format!("{pad}{line}")));
                }
                if !block.text.is_empty() {
                    self.wrapped.push(WrappedLine::plain(Kind::Logo, String::new()));
                    let tag_pad = " ".repeat(w.saturating_sub(block.text.chars().count()) / 2);
                    self.wrapped.push(WrappedLine::plain(Kind::Info, format!("{tag_pad}{}", block.text)));
                }
                if block.gap {
                    self.wrapped.push(WrappedLine::plain(Kind::Info, String::new()));
                }
                self.line_counts.push(self.wrapped.len() - lines_before);
                continue;
            }
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
            // Indexed loop: tables need lookahead (the |---| delimiter row
            // is what makes the line above it a header).
            let block_lines: Vec<&str> = prefixed.lines().collect();
            let mut li = 0;
            while li < block_lines.len() {
                let raw_line = block_lines[li];
                li += 1;
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
                // Assistant prose: rules collapse to one clean line, pipe
                // tables render as aligned columns instead of raw | walls.
                if block.kind == Kind::Assistant {
                    if md_is_rule(raw_line) {
                        let rule = "─".repeat(w.min(48));
                        // Dedup past blank lines too: `---`, blank, `---`
                        // (a model tic) must still collapse to ONE rule.
                        let last_visible =
                            self.wrapped.iter().rev().find(|l| !l.text.trim().is_empty());
                        if last_visible.is_none_or(|l| l.text != rule) {
                            self.wrapped.push(WrappedLine {
                                kind: Kind::Assistant,
                                text: rule.clone(),
                                spans: Some(vec![(t.muted, rule)]),
                            });
                        }
                        continue;
                    }
                    if raw_line.contains('|')
                        && li < block_lines.len()
                        && md_is_table_delim(block_lines[li])
                    {
                        let mut end = li + 1;
                        while end < block_lines.len()
                            && block_lines[end].contains('|')
                            && !block_lines[end].trim().is_empty()
                        {
                            end += 1;
                        }
                        for wl in md_render_table(raw_line, &block_lines[li + 1..end], w, t) {
                            self.wrapped.push(wl);
                        }
                        li = end;
                        continue;
                    }
                }
                // Non-fenced content: diffs hard-cut, everything else wraps.
                let kind = block.kind;
                if raw_line.is_empty() {
                    self.wrapped.push(WrappedLine::plain(kind, String::new()));
                } else if kind == Kind::Assistant {
                    // Lightweight markdown for assistant prose: headings get
                    // the bold accent, bullets get •, `inline code` and
                    // **bold** get span colors. Anything unmarked is plain.
                    if let Some(text) = md_heading(raw_line) {
                        for piece in textwrap::wrap(text, w) {
                            self.wrapped.push(WrappedLine::plain(Kind::Heading, piece.into_owned()));
                        }
                    } else {
                        let line = md_bullet(raw_line);
                        if line.contains('`') || line.contains("**") {
                            let spans = md_inline_spans(&line, t);
                            for piece in wrap_spans(&spans, w) {
                                self.wrapped.push(WrappedLine {
                                    kind,
                                    text: piece.iter().map(|(_, s)| s.as_str()).collect(),
                                    spans: Some(piece),
                                });
                            }
                        } else {
                            for piece in textwrap::wrap(&line, w) {
                                self.wrapped.push(WrappedLine::plain(kind, piece.into_owned()));
                            }
                        }
                    }
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

    /// Index of the first wrapped line currently on screen — the inverse of
    /// the slice `visible_lines` renders, so screen rows map back to content.
    pub(crate) fn view_start(&self) -> usize {
        let total = self.wrapped.len();
        let end = total - self.scroll_from_bottom.min(total);
        end.saturating_sub(self.view_height)
    }

    /// Content position under a screen cell, clamped to real text: a click
    /// past the end of a line lands at its end, past the last line at the
    /// last line.
    pub(crate) fn pos_at(&self, col: u16, row: u16) -> (usize, usize) {
        if self.wrapped.is_empty() {
            return (0, 0);
        }
        let rel = row.saturating_sub(self.area.y) as usize;
        let idx = (self.view_start() + rel).min(self.wrapped.len() - 1);
        let len = self.wrapped[idx].text.chars().count();
        (idx, (col.saturating_sub(self.area.x) as usize).min(len))
    }

    /// Start a selection at a screen cell (a bare click selects nothing).
    pub(crate) fn select_from(&mut self, col: u16, row: u16) {
        let pos = self.pos_at(col, row);
        self.sel = Some(Sel { anchor: pos, cursor: pos });
    }

    /// Extend the live selection to a screen cell (mouse drag).
    pub(crate) fn select_to(&mut self, col: u16, row: u16) {
        let pos = self.pos_at(col, row);
        if let Some(sel) = &mut self.sel {
            sel.cursor = pos;
        }
    }

    /// The selected text, or None when nothing (or only blank space) is
    /// selected. Code renders inside a `│ ` gutter box; a selection that
    /// covers the gutter means the code, not the border, so the gutter is
    /// dropped — pasted code stays pasteable.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let ((l0, c0), (l1, c1)) = self.sel?.ordered();
        let last = self.wrapped.len().checked_sub(1)?;
        // A streaming re-wrap can shrink the buffer under a live selection.
        let (l0, l1) = (l0.min(last), l1.min(last));
        let mut out = String::new();
        for i in l0..=l1 {
            let text = &self.wrapped[i].text;
            let n = text.chars().count();
            let from = if i == l0 { c0.min(n) } else { 0 };
            let to = if i == l1 { c1.min(n) } else { n };
            let piece: String = text.chars().skip(from).take(to.saturating_sub(from)).collect();
            let piece = match piece.strip_prefix("│ ") {
                Some(rest) => rest,
                None => piece.strip_prefix('│').unwrap_or(&piece),
            };
            out.push_str(piece.trim_end());
            if i < l1 {
                out.push('\n');
            }
        }
        (!out.trim().is_empty()).then_some(out)
    }

    pub(crate) fn visible_lines(&mut self, t: &theme::Theme) -> Vec<Line<'static>> {
        self.scroll_from_bottom = self.scroll_from_bottom.min(self.max_scroll());
        let total = self.wrapped.len();
        let end = total - self.scroll_from_bottom.min(total);
        let start = end.saturating_sub(self.view_height);
        let sel = self.sel.map(|s| s.ordered());
        self.wrapped[start..end]
            .iter()
            .enumerate()
            .map(|(row, wl)| render_wrapped(wl, t, selected_cols(sel, start + row)))
            .collect()
    }

    pub(crate) fn contains(&self, col: u16, row: u16) -> bool {
        self.area.contains(Position { x: col, y: row })
    }
}

/// The selected char range within wrapped line `idx`, if any. `usize::MAX`
/// as the end means "to the end of the line" (a middle line of a multi-line
/// selection) — `render_wrapped` clamps it to the line's own length.
fn selected_cols(sel: Option<((usize, usize), (usize, usize))>, idx: usize) -> Option<(usize, usize)> {
    let ((l0, c0), (l1, c1)) = sel?;
    if idx < l0 || idx > l1 {
        return None;
    }
    let from = if idx == l0 { c0 } else { 0 };
    let to = if idx == l1 { c1 } else { usize::MAX };
    (from < to).then_some((from, to))
}

/// One wrapped line as a ratatui `Line`, with `sel`'s char range drawn
/// reversed. Reverse video rather than a fixed highlight color: it reads
/// correctly on every theme, including the syntect-colored code spans.
fn render_wrapped(wl: &WrappedLine, t: &theme::Theme, sel: Option<(usize, usize)>) -> Line<'static> {
    let base: Vec<(Style, &str)> = match &wl.spans {
        Some(spans) => spans.iter().map(|(c, s)| (Style::default().fg(*c), s.as_str())).collect(),
        None => vec![(style_for(wl.kind, t), wl.text.as_str())],
    };
    let Some((from, to)) = sel else {
        return Line::from(
            base.into_iter().map(|(st, s)| Span::styled(s.to_string(), st)).collect::<Vec<_>>(),
        );
    };
    let mut out: Vec<Span<'static>> = Vec::with_capacity(base.len() + 2);
    // Walk the spans in char space, splitting each one where it crosses a
    // selection edge.
    let mut span_start = 0usize;
    for (st, text) in base {
        let n = text.chars().count();
        let mut i = 0usize;
        while i < n {
            let abs = span_start + i;
            let inside = abs >= from && abs < to;
            // Next boundary in absolute chars: the selection's start when
            // we're before it, its end when we're inside it.
            let boundary = if inside {
                to
            } else if abs < from {
                from
            } else {
                usize::MAX
            };
            let stop = boundary.saturating_sub(span_start).min(n).max(i + 1);
            let piece: String = text.chars().skip(i).take(stop - i).collect();
            out.push(Span::styled(piece, if inside { st.add_modifier(Modifier::REVERSED) } else { st }));
            i = stop;
        }
        span_start += n;
    }
    Line::from(out)
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

/// What the TUI sends to the agent task: a normal prompt for the model
/// (with any image attachments from @-mentions), or a slash command
/// handled locally.
enum UiMsg {
    Prompt(String, Vec<String>, CancellationToken),
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

/// The `/release-notes` popup: a heading, the raw markdown body lines, and
/// the current scroll offset (in wrapped display lines).
struct NotesPopup {
    title: String,
    body: Vec<String>,
    scroll: usize,
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

/// `## Heading` → `Heading` (1-6 hashes followed by a space). Anything else
/// — including `#hashtag` and shebang-looking lines — is not a heading.
fn md_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        Some(trimmed[hashes + 1..].trim_start())
    } else {
        None
    }
}

/// `- item` / `* item` → `• item`, preserving leading indent.
fn md_bullet(line: &str) -> String {
    let indent_len = line.len() - line.trim_start().len();
    let rest = &line[indent_len..];
    for marker in ["- ", "* "] {
        if let Some(item) = rest.strip_prefix(marker) {
            return format!("{}• {}", &line[..indent_len], item);
        }
    }
    line.to_string()
}

/// A thematic-break line: nothing but 3+ rule characters. Models (DeepSeek
/// especially) also draw ASCII rules as long runs of -/─/═; both collapse
/// to one clean horizontal rule instead of a raw dash wall.
fn md_is_rule(line: &str) -> bool {
    let s = line.trim();
    s.chars().count() >= 3 && s.chars().all(|c| matches!(c, '-' | '─' | '—' | '━' | '═' | '_' | '=' | '*'))
}

/// A GFM table delimiter row (`|---|:--:|…`) — the line that makes the row
/// above it a header.
fn md_is_table_delim(line: &str) -> bool {
    let s = line.trim();
    s.contains('|')
        && s.matches('-').count() >= 2
        && s.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Cells of one table row: split on `|`, outer pipes dropped, `` ` `` and
/// `**` markers consumed (color is per-row here, not per-cell).
fn md_table_cells(row: &str) -> Vec<String> {
    let mut s = row.trim();
    s = s.strip_prefix('|').unwrap_or(s);
    s = s.strip_suffix('|').unwrap_or(s);
    s.split('|').map(|c| c.trim().replace("**", "").replace('`', "")).collect()
}

/// Render a pipe table as aligned columns: heading-colored header, a ─ rule
/// under it, cells padded (and `…`-truncated) to fit the pane width.
fn md_render_table(header: &str, body: &[&str], w: usize, t: &theme::Theme) -> Vec<WrappedLine> {
    const GAP: usize = 2;
    const MIN_COL: usize = 4;
    let rows: Vec<Vec<String>> =
        std::iter::once(header).chain(body.iter().copied()).map(md_table_cells).collect();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return vec![];
    }
    let mut widths: Vec<usize> = (0..cols)
        .map(|i| rows.iter().map(|r| r.get(i).map_or(0, |c| c.chars().count())).max().unwrap_or(0).max(MIN_COL))
        .collect();
    // Over-wide table: shave the widest column until it fits (or every
    // column is at the floor — then the pane hard-clips, which is fine).
    let fits = |ws: &[usize]| ws.iter().sum::<usize>() + GAP * (cols - 1) <= w;
    while !fits(&widths) {
        let Some((imax, _)) = widths.iter().enumerate().max_by_key(|(_, n)| **n) else { break };
        if widths[imax] <= MIN_COL {
            break;
        }
        widths[imax] -= 1;
    }
    let pad_row = |cells: &[String]| -> String {
        let mut line = String::new();
        for (i, width) in widths.iter().enumerate() {
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            let mut shown: String = cell.chars().take(*width).collect();
            if shown.chars().count() < cell.chars().count() {
                shown.pop();
                shown.push('…');
            }
            let pad = width.saturating_sub(shown.chars().count());
            line.push_str(&shown);
            if i + 1 < cols {
                line.push_str(&" ".repeat(pad + GAP));
            }
        }
        line
    };
    let total: usize = widths.iter().sum::<usize>() + GAP * (cols - 1);
    let mut out = Vec::with_capacity(rows.len() + 1);
    out.push(WrappedLine {
        kind: Kind::Assistant,
        text: pad_row(&rows[0]),
        spans: Some(vec![(t.accent, pad_row(&rows[0]))]),
    });
    out.push(WrappedLine {
        kind: Kind::Assistant,
        text: "─".repeat(total.min(w)),
        spans: Some(vec![(t.muted, "─".repeat(total.min(w)))]),
    });
    for cells in &rows[1..] {
        out.push(WrappedLine::plain(Kind::Assistant, pad_row(cells)));
    }
    out
}

/// Compact display of a tool call for the activity log: the salient
/// argument first (path, command, pattern…), remaining args as k=v, JSON
/// braces and quotes dropped. Falls back to the raw JSON head when the
/// arguments don't parse.
fn tool_call_summary(name: &str, args_json: &str) -> String {
    let clip = |s: &str, max: usize| -> String {
        let flat = s.replace('\n', " ");
        let mut out: String = flat.chars().take(max).collect();
        if out.chars().count() < flat.chars().count() {
            out.push('…');
        }
        out
    };
    let scalar = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(args_json)
    else {
        return format!("→ {name} {}", clip(args_json, 120));
    };
    const SALIENT: &[&str] = &["path", "command", "pattern", "url", "query", "condition"];
    let salient = SALIENT.iter().find(|k| map.contains_key(**k)).copied();
    let mut out = format!("→ {name}");
    if let Some(key) = salient {
        out.push(' ');
        out.push_str(&clip(&scalar(&map[key]), 90));
    }
    for (key, value) in &map {
        if Some(key.as_str()) == salient {
            continue;
        }
        out.push_str(&format!(" {key}={}", clip(&scalar(value), 40)));
    }
    out
}

/// Split one line into colored spans: `` `code` `` gets the code color,
/// `**bold**` gets the accent, everything else the default foreground. The
/// markers themselves are consumed. Unclosed markers render literally.
fn md_inline_spans(line: &str, t: &theme::Theme) -> Vec<(Color, String)> {
    let mut out: Vec<(Color, String)> = Vec::new();
    let mut plain = String::new();
    let mut rest = line;
    let flush = |plain: &mut String, out: &mut Vec<(Color, String)>| {
        if !plain.is_empty() {
            out.push((Color::Reset, std::mem::take(plain)));
        }
    };
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                flush(&mut plain, &mut out);
                out.push((t.accent, after[..end].to_string()));
                rest = &after[end + 2..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                flush(&mut plain, &mut out);
                out.push((t.code, after[..end].to_string()));
                rest = &after[end + 1..];
                continue;
            }
        }
        let ch = rest.chars().next().unwrap();
        plain.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush(&mut plain, &mut out);
    out
}

/// Word-wrap colored spans to `w` columns (greedy fill; a single word longer
/// than the width is hard-cut). Space runs collapse at wrap points only.
fn wrap_spans(spans: &[(Color, String)], w: usize) -> Vec<Vec<(Color, String)>> {
    // Tokenize into (color, word) + implicit single-space separators.
    let mut words: Vec<(Color, String)> = Vec::new();
    for (color, text) in spans {
        for word in text.split(' ') {
            if word.is_empty() {
                continue;
            }
            words.push((*color, word.to_string()));
        }
    }
    let mut lines: Vec<Vec<(Color, String)>> = Vec::new();
    let mut cur: Vec<(Color, String)> = Vec::new();
    let mut cur_w = 0usize;
    for (color, word) in words {
        let wl = word.chars().count();
        let sep = usize::from(!cur.is_empty());
        if cur_w + sep + wl > w && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if wl > w {
            // Hard-cut an overlong word across lines.
            let mut restw = word.as_str();
            while restw.chars().count() > w {
                let cut = floor_boundary(restw, w);
                lines.push(vec![(color, restw[..cut].to_string())]);
                restw = &restw[cut..];
            }
            cur.push((color, restw.to_string()));
            cur_w = restw.chars().count();
            continue;
        }
        if !cur.is_empty() {
            // The separator space stays in the PRECEDING span so colored
            // spans hold exactly their own text (spaces render identically
            // in any color).
            if let Some((_, s)) = cur.last_mut() {
                s.push(' ');
            }
            match cur.last_mut() {
                Some((c, s)) if *c == color => s.push_str(&word),
                _ => cur.push((color, word)),
            }
            cur_w += 1 + wl;
        } else {
            cur.push((color, word));
            cur_w = wl;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(vec![(Color::Reset, String::new())]);
    }
    lines
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
    fn split_bold_handles_markers_and_wraps_notes() {
        let base = Style::default();
        let bold = Style::default().add_modifier(Modifier::BOLD);
        // Balanced markers → bold run between, base runs outside.
        let runs = split_bold("a **b** c", base, bold);
        assert_eq!(
            runs.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
            vec!["a ", "b", " c"]
        );
        assert!(runs[1].0.add_modifier.contains(Modifier::BOLD));
        assert!(!runs[0].0.add_modifier.contains(Modifier::BOLD));
        // Unbalanced marker → the remainder stays verbatim (no panic, no drop).
        let runs = split_bold("x **y", base, bold);
        assert_eq!(runs.iter().map(|(_, s)| s.clone()).collect::<String>(), "x **y");

        // render_notes_lines: a bullet gets a marker, and a line longer than
        // the width wraps onto more than one display line.
        let t = theme::DARK;
        let lines = render_notes_lines(
            &["- **feat**: a reasonably long sentence that must wrap here".into()],
            20,
            &t,
        );
        assert!(lines.len() >= 2, "long bullet should wrap");
        let first: String =
            lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.starts_with("• "), "bullet marker missing: {first:?}");
    }

    #[test]
    fn input_wraps_to_width_and_tracks_cursor() {
        // A single long line folds into ceil(len/width) rows instead of
        // running off the edge — the reported bug.
        let (rows, cr, cc) = wrap_input("abcdefghij", 0, 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
        assert_eq!((cr, cc), (0, 0));
        // Cursor mid-text lands on the right wrapped row/column.
        let (_, cr, cc) = wrap_input("abcdefghij", 6, 4);
        assert_eq!((cr, cc), (1, 2)); // 6/4 = row 1, 6%4 = col 2
        // Cursor at the very end of a full-width line gets its own row so it
        // stays visible (terminal-style wrap), not clipped past the edge.
        let (rows, cr, cc) = wrap_input("abcd", 4, 4);
        assert_eq!(rows, vec!["abcd", ""]);
        assert_eq!((cr, cc), (1, 0));
        // Explicit newlines still split; continuation rows are counted.
        let (rows, _, _) = wrap_input("hello world\nbye", 999, 5);
        assert_eq!(rows, vec!["hello", " worl", "d", "bye"]);
        // Empty input is one row; height includes the two borders.
        assert_eq!(wrap_input("", 0, 4).0, vec![String::new()]);
        assert_eq!(input_height("", 0, 80), 3);
    }

    #[test]
    fn input_pane_renders_long_line_across_multiple_rows() {
        // End-to-end render at a narrow width: a long line must appear on
        // several rows, not run off the right edge (the reported bug).
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::new(String::new(), vec![], theme::DARK, std::path::PathBuf::from("."));
        app.input = "x".repeat(100);
        app.cursor = app.input.len();
        let mut term = Terminal::new(TestBackend::new(30, 20)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let mut rows_with_text = 0;
        for y in 0..buf.area.height {
            let mut s = String::new();
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            if s.contains("xx") {
                // No row may overflow the frame width.
                assert!(s.chars().count() <= 30, "row wider than frame: {s:?}");
                rows_with_text += 1;
            }
        }
        assert!(rows_with_text >= 2, "input did not wrap (rows_with_text={rows_with_text})");
    }

    #[test]
    fn input_height_grows_with_wrapping_and_is_capped() {
        // Narrow terminal: a line that fits at full width now wraps to more
        // rows, so the box grows — this is the fix for text disappearing.
        let long = "x".repeat(50);
        assert!(input_height(&long, 0, 20) > input_height(&long, 0, 200));
        // But never past the cap (+2 borders), so a huge paste can't eat the
        // whole screen — it scrolls instead.
        let huge = "y".repeat(10_000);
        assert_eq!(input_height(&huge, 0, 30), (MAX_INPUT_ROWS + 2) as u16);
    }

    #[test]
    fn gui_editors_recognized_terminal_editors_left_to_handover() {
        // GUI editors keep the TUI up with the modal; recognizing these is
        // what fixes the notepad "blank terminal" bug.
        for e in ["notepad", "code", "code -w", "cursor", "subl", "gedit", "zed", "notepad++"] {
            assert!(editor_is_gui(e), "{e} should be treated as a GUI editor");
        }
        // Terminal editors (and anything unknown) MUST fall to the TTY
        // handover — misclassifying one as GUI leaves it fighting the TUI
        // for the console. Full paths and .exe suffixes still classify.
        for e in ["vim", "vi", "nvim", "nano", "hx", "emacs", "micro", "some-unknown-editor"] {
            assert!(!editor_is_gui(e), "{e} must use the TTY handover");
        }
        // Full paths classify by basename — using each platform's own
        // separator (backslash is not a path separator on Unix).
        #[cfg(windows)]
        {
            assert!(editor_is_gui(r"C:\Program Files\Notepad++\notepad++.exe"));
            assert!(!editor_is_gui(r"C:\tools\vim\vim.exe"));
        }
        #[cfg(not(windows))]
        {
            assert!(editor_is_gui("/usr/bin/gedit"));
            assert!(!editor_is_gui("/usr/local/bin/nvim"));
        }
    }

    #[test]
    fn config_editor_beats_env_and_blank_means_unset() {
        set_config_editor(Some("code -w".into()));
        assert_eq!(resolve_editor_with_source(), ("code -w".into(), "config \"editor\""));
        // Whitespace-only is treated as unset, falling through to the env
        // vars / default (which of those wins depends on the test env, so
        // only the source not being "config" is asserted).
        set_config_editor(Some("   ".into()));
        assert_ne!(resolve_editor_with_source().1, "config \"editor\"");
        set_config_editor(None);
    }

    #[test]
    fn default_editor_prefers_terminal_over_notepad() {
        // The whole point of the probe: any terminal editor on PATH beats
        // notepad, and `edit` (in-box on Windows 11) wins when present.
        let all = pick_terminal_editor(|_| true);
        assert_eq!(all.as_deref(), Some("edit"));
        let vim_only = pick_terminal_editor(|name| name == "vim");
        assert_eq!(vim_only.as_deref(), Some("vim"));
        // Notepad is strictly a last resort — never picked by the probe
        // itself, only by `default_editor` when the probe comes up empty.
        assert_eq!(pick_terminal_editor(|_| false), None);
        // Everything the probe can return must take the TTY handover, not
        // the GUI modal — otherwise the "fix" reintroduces the popup.
        for name in ["edit", "nano", "vim", "nvim", "vi", "hx", "micro"] {
            let picked = pick_terminal_editor(|n| n == name).unwrap();
            assert!(!editor_is_gui(&picked), "{picked} must not classify as GUI");
        }
    }

    #[test]
    fn logo_scales_to_width_and_always_fits() {
        // Every variant must fit the width it was asked for; the pixel grid
        // rows must agree on width or the letters shear.
        for row in LOGO_GRID {
            assert_eq!(row.len(), LOGO_GRID[0].len());
        }
        for w in [10, 20, 27, 28, 41, 42, 80, 84, 120, 300] {
            for line in logo_art(w) {
                assert!(line.chars().count() <= w, "logo line wider than pane at w={w}");
            }
        }
        // Tier selection: 2x pixel art on wide panes, 1x when it fits,
        // box-drawing fallback, then plain text.
        assert_eq!(logo_art(84).len(), LOGO_GRID.len() * 2);
        assert_eq!(logo_art(42).len(), LOGO_GRID.len());
        assert_eq!(logo_art(28).len(), LOGO_SMALL.len());
        assert_eq!(logo_art(10), vec!["r i f t".to_string()]);
        // The cap keeps huge terminals at 2x rather than a screen-filling 7x.
        assert_eq!(logo_art(300).len(), LOGO_GRID.len() * 2);
    }

    #[tokio::test]
    async fn btw_without_provider_reports_cleanly() {
        // A ctx with no sub-agent handle (fresh, no frontend install) must
        // produce a friendly error effect, not hang or panic.
        let (fx, mut rx) = mpsc::unbounded_channel();
        let ctx = rift_core::ToolCtx::new(std::env::temp_dir());
        spawn_btw(fx, ctx, std::env::temp_dir().join("no-such-session.json"), vec![], "hi?".into());
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap() {
            UiEffect::Btw { ok, reply, .. } => {
                assert!(!ok);
                assert!(reply.contains("no provider"));
            }
            _ => panic!("expected Btw effect"),
        }
    }

    /// Live test against a local OpenAI-compatible server — run manually:
    /// `RIFT_BTW_TEST_URL=http://host:8000/v1 RIFT_BTW_TEST_MODEL=<model> cargo test btw_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn btw_live_side_question() {
        let url = std::env::var("RIFT_BTW_TEST_URL").expect("set RIFT_BTW_TEST_URL");
        let model = std::env::var("RIFT_BTW_TEST_MODEL").expect("set RIFT_BTW_TEST_MODEL");
        let ctx = rift_core::ToolCtx::new(std::env::temp_dir());
        ctx.set_subagent(rift_core::SubAgentHandle {
            client: std::sync::Arc::new(rift_openai::OpenAiClient::new(&url, None)),
            cfg: rift_core::AgentConfig { model, ..Default::default() },
            factory: None,
            roles: std::collections::HashMap::new(),
            personas: vec![],
        });
        // A session snapshot the side question should see through.
        let session = std::env::temp_dir().join("rift-btw-test-session.json");
        let saved = rift_core::SavedSession {
            model: "test".into(),
            saved_at: 0,
            cwd: ".".into(),
            messages: vec![
                Message::system("You are a coding agent."),
                Message::user("The secret project codename is ZEPHYR-9."),
                Message {
                    role: Role::Assistant,
                    content: "Noted — the codename is ZEPHYR-9.".into(),
                    thinking: None,
                    tool_calls: vec![],
                    tool_name: None,
                    tool_call_id: None,
                    provider_data: None,
                    images: vec![],
                },
            ],
        };
        std::fs::write(&session, serde_json::to_string(&saved).unwrap()).unwrap();

        let (fx, mut rx) = mpsc::unbounded_channel();
        spawn_btw(fx, ctx, session, vec![], "btw, what was the codename again? Reply with just it.".into());
        match tokio::time::timeout(Duration::from_secs(120), rx.recv()).await.unwrap().unwrap() {
            UiEffect::Btw { ok, reply, .. } => {
                println!("btw reply: {reply}");
                assert!(ok, "side question failed: {reply}");
                assert!(reply.to_uppercase().contains("ZEPHYR"), "answer ignored conversation context: {reply}");
            }
            _ => panic!("expected Btw effect"),
        }
    }

    #[test]
    fn seed_user_preview_collapses_walls_and_strips_notes() {
        // Short messages pass through untouched.
        assert_eq!(seed_user_preview("hi there"), "hi there");
        // Long expanded prompts collapse to a head + count.
        let wall = (1..=30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let seeded = seed_user_preview(&wall);
        assert!(seeded.starts_with("line 1\n"));
        assert!(seeded.ends_with("… [22 more lines]"), "got: {seeded}");
        // The Esc-interrupt note is bookkeeping, not something to re-show.
        let noted = "[note: the user pressed Esc to CANCEL your previous, incomplete turn. Treat that \
                     interrupted task as abandoned — do NOT resume it unless this message asks you to.]\n\nwhat is 2+2?";
        assert_eq!(seed_user_preview(noted), "what is 2+2?");
    }

    #[test]
    fn apply_theme_switches_and_rejects_unknown() {
        // Regression: theme switching is UI-side; both the typed form and
        // the picker route through apply_theme (the picker used to forward
        // "/theme <name>" to the agent dispatcher → "unknown command").
        let mut app = App::new("m".into(), vec![], theme::DARK, std::env::temp_dir());
        app.apply_theme("mono");
        assert_eq!(app.theme.name, "mono");
        app.apply_theme("dracula");
        assert_eq!(app.theme.name, "dracula");
        app.apply_theme("not-a-theme");
        assert_eq!(app.theme.name, "dracula"); // unchanged, warning pushed
        assert!(app.transcript.raw_text().contains("unknown theme"));
    }

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
    fn interval_parses_units_and_rejects_junk() {
        assert_eq!(parse_interval("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_interval("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_interval("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_interval("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_interval("0s"), None);
        assert_eq!(parse_interval("m"), None);
        assert_eq!(parse_interval(""), None);
        assert_eq!(parse_interval("soon"), None);
        assert_eq!(parse_interval("5x"), None);

        assert_eq!(fmt_interval(Duration::from_secs(90)), "90s");
        assert_eq!(fmt_interval(Duration::from_secs(300)), "5m");
        assert_eq!(fmt_interval(Duration::from_secs(7200)), "2h");
    }

    #[test]
    fn goal_met_is_line_anchored() {
        assert!(goal_met("All tests pass.\nGOAL MET — 42/42 green"));
        assert!(goal_met("  GOAL MET: verified with cargo test"));
        // Mentions mid-sentence (echoing the instruction) don't count.
        assert!(!goal_met("I will write GOAL MET once the suite is green."));
        assert!(!goal_met("still two failures — not done yet"));
        assert!(!goal_met(""));
    }

    #[test]
    fn scope_flag_parses_and_placements_differ() {
        assert_eq!(parse_scope_flag(" make coffee"), (false, "make coffee".into()));
        assert_eq!(parse_scope_flag(" --global make coffee"), (true, "make coffee".into()));
        assert_eq!(parse_scope_flag(" -g make coffee"), (true, "make coffee".into()));
        assert_eq!(parse_scope_flag(" --user x"), (true, "x".into()));
        // Flag must be a whole word, not a prefix of the description.
        assert_eq!(parse_scope_flag(" --globalist ideas"), (false, "--globalist ideas".into()));
        assert_eq!(parse_scope_flag(" --global"), (true, String::new()));

        let (dir, scope) = skill_target(false);
        assert_eq!(dir, ".rift/skills");
        assert!(scope.contains("project"));
        let (dir, scope) = skill_target(true);
        assert!(dir.ends_with("rift/skills") && dir != ".rift/skills");
        assert!(scope.contains("every project"));

        let p = mcp_placement(false);
        assert_eq!(p.config_path, ".rift.json");
        assert_eq!(p.args_path, ".rift/mcp/<name>.py");
        assert!(p.trust_note.contains("trust"));
        let p = mcp_placement(true);
        // Global registrations MUST be absolute — relative args resolve
        // against whatever cwd rift happens to run from.
        assert!(std::path::Path::new(&p.args_path).is_absolute(), "{}", p.args_path);
        assert!(p.config_path.ends_with("rift/config.json"));
    }

    #[test]
    fn markdown_headings_bullets_and_inline_styles_render() {
        let mut p = Pane::new();
        p.set_syntax(None);
        p.push_block(
            Kind::Assistant,
            "## Results\n- fix applied in `stats.py`\nThis is **important** to know\n#not-a-heading".into(),
        );
        p.rebuild(60, &theme::DARK);
        let lines = &p.wrapped;

        // Heading: hashes stripped, Kind::Heading (bold accent via style_for).
        assert_eq!(lines[0].kind, Kind::Heading);
        assert_eq!(lines[0].text, "Results");

        // Bullet marker becomes •, inline code gets a code-colored span.
        assert!(lines[1].text.starts_with("• fix applied in "), "{}", lines[1].text);
        let spans = lines[1].spans.as_ref().expect("inline code produces spans");
        assert!(spans.iter().any(|(c, s)| s.trim() == "stats.py" && *c == theme::DARK.code));
        assert!(!lines[1].text.contains('`'), "backticks must be consumed");

        // Bold: markers consumed, accent-colored span.
        let spans = lines[2].spans.as_ref().expect("bold produces spans");
        assert!(spans.iter().any(|(c, s)| s.trim() == "important" && *c == theme::DARK.accent));
        assert!(!lines[2].text.contains("**"));

        // A #hashtag without a following space is NOT a heading.
        assert_eq!(lines[3].kind, Kind::Assistant);
        assert_eq!(lines[3].text, "#not-a-heading");
    }

    #[test]
    fn markdown_inline_spans_wrap_by_words() {
        let spans = vec![
            (Color::Reset, "alpha beta".to_string()),
            (Color::Red, "gamma".to_string()),
        ];
        let wrapped = wrap_spans(&spans, 11);
        // "alpha beta" fits line 1 (10 cols); "gamma" wraps to line 2.
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].iter().map(|(_, s)| s.as_str()).collect::<String>(), "alpha beta");
        assert_eq!(wrapped[1][0], (Color::Red, "gamma".to_string()));

        // Unclosed markers render literally, nothing is eaten.
        let spans = md_inline_spans("a `dangling and **half", &theme::DARK);
        let text: String = spans.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(text, "a `dangling and **half");
    }

    #[test]
    fn rules_and_table_delims_detect() {
        assert!(md_is_rule("---"));
        assert!(md_is_rule("  ────────────  "));
        assert!(md_is_rule(&"-".repeat(72)));
        assert!(md_is_rule("========"));
        assert!(!md_is_rule("--"));
        assert!(!md_is_rule("- item"));
        assert!(!md_is_rule("a---b"));

        assert!(md_is_table_delim("|---|---|"));
        assert!(md_is_table_delim("| :--- | :--: |"));
        assert!(!md_is_table_delim("| File | Purpose |"));
        assert!(!md_is_table_delim("just text"));
    }

    #[test]
    fn tables_render_as_aligned_columns() {
        let lines = md_render_table(
            "| File | Purpose |",
            &["| `README.md` | Project overview |", "| main.py | FastAPI entry point |"],
            60,
            &theme::DARK,
        );
        // header + rule + 2 body rows
        assert_eq!(lines.len(), 4);
        // Backticks are consumed, columns align at the same offset.
        assert!(lines[2].text.starts_with("README.md"));
        assert!(!lines[2].text.contains('`') && !lines[2].text.contains('|'));
        let col2 = |s: &str| s.find("Purpose").or_else(|| s.find("Project")).or_else(|| s.find("FastAPI")).unwrap();
        assert_eq!(col2(&lines[0].text), col2(&lines[2].text));
        assert_eq!(col2(&lines[2].text), col2(&lines[3].text));
        assert!(lines[1].text.starts_with('─'), "rule under the header");

        // Over-wide tables shrink and truncate with … instead of wrapping.
        let narrow = md_render_table(
            "| A | B |",
            &["| averyveryverylongcellvaluethatcannotfit | second |"],
            20,
            &theme::DARK,
        );
        assert!(narrow.iter().all(|l| l.text.chars().count() <= 20));
        assert!(narrow[2].text.contains('…'));
    }

    #[test]
    fn tool_call_summaries_lead_with_the_salient_arg() {
        assert_eq!(
            tool_call_summary("read", r#"{"path":"backend/main.py"}"#),
            "→ read backend/main.py"
        );
        let s = tool_call_summary(
            "edit",
            r#"{"path":"stats.py","old_string":"a\nb","new_string":"c"}"#,
        );
        assert!(s.starts_with("→ edit stats.py"), "got: {s}");
        assert!(s.contains("old_string=a b"), "newlines flatten: {s}");
        // Unparseable args fall back to the raw head, capped.
        let raw = tool_call_summary("bash", "not-json");
        assert_eq!(raw, "→ bash not-json");
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
    fn drag_selection_extracts_pane_text() {
        let mut p = Pane::new();
        p.push_line(Kind::Info, "first line".into());
        p.push_line(Kind::Info, "second line".into());
        p.push_line(Kind::Info, "third line".into());
        p.rebuild(40, &theme::DARK);
        p.area = Rect { x: 0, y: 0, width: 40, height: 10 };
        p.view_height = 10;
        // Fewer lines than the viewport: screen row N is wrapped line N.
        p.select_from(6, 0);
        p.select_to(6, 2);
        assert_eq!(p.selected_text().unwrap(), "line\nsecond line\nthird");
        // Dragging backwards selects the same span.
        p.select_from(6, 2);
        p.select_to(6, 0);
        assert_eq!(p.selected_text().unwrap(), "line\nsecond line\nthird");
        // A click without a drag selects nothing.
        p.select_from(3, 1);
        assert!(p.selected_text().is_none());
        // Past the right edge clamps to the end of the line.
        p.select_from(0, 1);
        p.select_to(200, 1);
        assert_eq!(p.selected_text().unwrap(), "second line");
        // A re-wrap moves every line index — the selection must not survive it.
        p.rebuild(20, &theme::DARK);
        assert!(p.sel.is_none());
    }

    #[test]
    fn selection_drops_the_code_box_gutter() {
        let mut p = Pane::new();
        p.set_syntax(None);
        p.push_block(Kind::Assistant, "```\nlet x = 1;\nlet y = 2;\n```".into());
        p.rebuild(40, &theme::DARK);
        p.area = Rect { x: 0, y: 0, width: 40, height: 10 };
        p.view_height = 10;
        // Rows: 0 = ╭─ code ─…, 1..=2 = gutter-prefixed content.
        assert!(p.wrapped[1].text.starts_with("│ "));
        p.select_from(0, 1);
        p.select_to(200, 2);
        assert_eq!(p.selected_text().unwrap(), "let x = 1;\nlet y = 2;");
    }

    #[test]
    fn selection_highlight_splits_only_the_selected_run() {
        let wl = WrappedLine::plain(Kind::Info, "abcdef".into());
        let line = render_wrapped(&wl, &theme::DARK, Some((2, 4)));
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["ab", "cd", "ef"]);
        assert!(line.spans[1].style.add_modifier.contains(Modifier::REVERSED));
        assert!(!line.spans[0].style.add_modifier.contains(Modifier::REVERSED));
        assert!(!line.spans[2].style.add_modifier.contains(Modifier::REVERSED));
        // No selection on this line → one span, untouched.
        let plain = render_wrapped(&wl, &theme::DARK, None);
        assert_eq!(plain.spans.len(), 1);
        assert!(!plain.spans[0].style.add_modifier.contains(Modifier::REVERSED));

        // Middle lines of a multi-line selection highlight to end of line.
        let sel = Some(((1usize, 3usize), (3usize, 2usize)));
        assert_eq!(selected_cols(sel, 0), None);
        assert_eq!(selected_cols(sel, 1), Some((3, usize::MAX)));
        assert_eq!(selected_cols(sel, 2), Some((0, usize::MAX)));
        assert_eq!(selected_cols(sel, 3), Some((0, 2)));
        assert_eq!(selected_cols(sel, 4), None);
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

        let (expanded, notes, images) = expand_mentions("check @lib.rs and @nope.rs please", &dir);
        assert!(expanded.starts_with("check @lib.rs and @nope.rs please"));
        assert!(expanded.contains("pub fn hello() -> u32"), "outline missing: {expanded}");
        assert!(!expanded.contains("41 + 1"), "body should be elided: {expanded}");
        assert!(notes.iter().any(|n| n.contains("nope.rs not found")));
        assert!(images.is_empty());
        // No mentions → prompt passes through untouched.
        let (same, notes2, _) = expand_mentions("no mentions here", &dir);
        assert_eq!(same, "no mentions here");
        assert!(notes2.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn image_mentions_attach_as_data_urls() {
        let dir = std::env::temp_dir().join(format!("rift-img-mention-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A tiny valid PNG header + junk is fine — attachment doesn't decode.
        std::fs::write(dir.join("shot.png"), [0x89, b'P', b'N', b'G', 1, 2, 3, 4]).unwrap();

        let (expanded, notes, images) = expand_mentions("what's wrong in @shot.png here", &dir);
        assert_eq!(images.len(), 1);
        assert!(images[0].starts_with("data:image/png;base64,"), "got: {}", &images[0][..40]);
        assert!(expanded.contains("[attached image shot.png"));
        assert!(!expanded.contains("base64"), "raw data must not enter the prompt text");
        assert!(notes.iter().any(|n| n.contains("vision-capable")));
        // And the data URL round-trips through the provider-side parser.
        let (mime, data) = rift_ollama::parse_data_url(&images[0]).unwrap();
        assert_eq!(mime, "image/png");
        assert!(!data.is_empty());
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

/// Which pane a mouse event landed in. Distinct from `Focus`: the right-hand
/// slot holds two panes (log and diff) and only one of them is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneId {
    Transcript,
    Log,
    Diff,
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
    theme: theme::Theme,
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
    /// Set by /restart: after teardown, main relaunches the (possibly
    /// updated) binary resuming this session.
    restart: bool,
    /// /goal: completion condition; turns auto-continue until the model
    /// verifies it (GOAL MET line) or the run cap / /goal clear stops it.
    goal: Option<GoalState>,
    /// /loop: recurring prompt/command, fixed interval or back-to-back.
    loop_state: Option<LoopState>,
    /// Auto-submission queued by goal/loop logic; the main loop (which owns
    /// prompt_tx) picks it up when the agent is idle.
    pending_auto: Option<String>,
    /// Assistant text accumulated over the current turn (goal-met check).
    turn_reply: String,
    /// Selected row in the slash-command palette popup.
    palette_idx: usize,
    /// Popup dismissed with Esc; cleared on the next input change.
    palette_off: bool,
    /// Mouse capture on = wheel scrolls panes and drag selects text; off =
    /// the terminal's own selection works instead (Ctrl+T toggles).
    mouse_capture: bool,
    /// Pane a left-button drag started in, while the button is still down.
    drag: Option<PaneId>,
    /// When the in-flight turn/command started (drives the spinner + elapsed
    /// time so long prompt-processing waits visibly aren't a hang).
    turn_started: Option<Instant>,
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
    /// Background tasks currently running (id, label) — drives the status
    /// bar count; maintained from TaskStarted/TaskFinished events.
    bg_running: Vec<(u64, String)>,
    /// Context-window occupancy (estimated used tokens, num_ctx) — the
    /// status bar gauge; refreshed by AgentEvent::Context after every
    /// turn/command and at startup.
    ctx_gauge: Option<(u64, u64)>,
    /// While a GUI editor (notepad, VS Code…) holds the config open in its
    /// own window, the TUI stays up but dimmed behind a modal telling the
    /// user to close the file. `Some((editor, path))` = that modal is shown;
    /// the event loop watches the editor process and reloads on close.
    /// Terminal editors (vim/nano) don't set this — they take the whole TTY.
    editing_file: Option<(String, String)>,
    /// `/release-notes` overlay: `Some((title, wrapped-off-by scroll))` when
    /// the closable popup is up. A dimmed modal like `editing_file`, but
    /// scrollable and dismissed with Esc/Enter/q.
    notes_popup: Option<NotesPopup>,
    /// Completed-task reports waiting to be fed back to the model as a
    /// [task notification] turn (fires on the next idle tick).
    task_notes: Vec<String>,
    /// Clipboard images staged by /paste (data URLs) — attached to the next
    /// prompt that gets sent.
    pending_paste: Vec<String>,
    /// /btw side-question exchanges (question, answer) this session — sent
    /// as context for follow-up side questions, never into the main history.
    btw_exchanges: Vec<(String, String)>,
    /// A side question is in flight (one at a time keeps them coherent).
    btw_busy: bool,
}

/// Running per-session counters surfaced by the `/stats` command.
#[derive(Default)]
struct SessionStats {
    turns: u64,
    model_calls: u64,
    output_tokens: u64,
    prompt_tokens: u64,
    /// Prompt tokens summed across every call — the billable input.
    billed_prompt_tokens: u64,
    tool_calls: u64,
    compactions: u64,
    duration_ms: u128,
    /// Summed hardening interventions across turns (see TurnStats::failures).
    failures: rift_core::FailureCounters,
}

impl App {
    fn new(model: String, skills: Vec<Skill>, theme: theme::Theme, cwd: PathBuf) -> Self {
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
            status: "Enter send · /help commands · drag to select+copy · Ctrl+L log · Esc cancel · /quit exit".into(),
            cancel: None,
            quit: false,
            restart: false,
            goal: None,
            loop_state: None,
            pending_auto: None,
            turn_reply: String::new(),
            palette_idx: 0,
            palette_off: false,
            mouse_capture: true,
            drag: None,
            turn_started: None,
            picker: None,
            answering: None,
            plan: vec![],
            skills,
            last_prompt: None,
            session_stats: SessionStats::default(),
            bg_running: vec![],
            ctx_gauge: None,
            editing_file: None,
            notes_popup: None,
            task_notes: vec![],
            pending_paste: vec![],
            btw_exchanges: vec![],
            btw_busy: false,
        }
    }

    /// Entries currently shown in the palette popup (empty = hidden):
    /// (completion text, argument hint, description). Built-in commands plus
    /// discovered skills as /skill:<name>. Live while the user is typing the
    /// command word itself (whitespace = they've moved on to arguments).
    fn palette(&self) -> Vec<(String, String, String)> {
        if self.palette_off || self.picker.is_some() || self.answering.is_some() || self.notes_popup.is_some() {
            return vec![];
        }
        // Argument completion for commands with a small enum of arguments:
        // once the command word is typed, offer its argument values (still
        // prefix-filtered), e.g. "/copy l" → "/copy log".
        if let Some(rest) = self.input.strip_prefix("/copy ") {
            let want = rest.trim_start();
            return [("all", "copy the whole transcript"), ("log", "copy the activity log")]
                .iter()
                .filter(|(arg, _)| arg.starts_with(want) && *arg != want)
                .map(|(arg, d)| (format!("/copy {arg}"), String::new(), d.to_string()))
                .collect();
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
        // Approval prompts carry the pending diff — render it diff-colored
        // above the question so the user reviews what they're allowing.
        for line in &req.detail {
            self.transcript.push_line(diff_kind(line), line.clone());
        }
        if !req.detail.is_empty() {
            self.transcript.push_line(Kind::Info, String::new());
        }
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

    /// RIFT banner at the top of the transcript, shown once at startup. The
    /// block is a placeholder — Pane::rebuild draws it sized to the pane, so
    /// the text here is just the tagline rendered under the art.
    fn push_logo(&mut self) {
        self.transcript.push_block(Kind::Logo, format!("v{} · {}", env!("CARGO_PKG_VERSION"), self.model));
    }

    /// One-time "Tips for getting started" note under the banner — only on a
    /// fresh session (resumed ones already have history to scroll). Points at
    /// the couple of commands a first-time user most wants.
    fn push_getting_started(&mut self) {
        self.transcript.push_block(
            Kind::Info,
            "Tips for getting started\n\
             • Run /init to generate a RIFT.md with instructions for rift\n\
             • Run /release-notes to see what's new in this version\n\
             • Run /help for all commands and keyboard shortcuts"
                .into(),
        );
    }

    /// Rebuild the transcript from a resumed session's message history.
    fn seed_from_messages(&mut self, messages: &[Message]) {
        let blocks_before = self.transcript.blocks.len();
        for msg in messages {
            match msg.role {
                Role::System => {}
                Role::User => {
                    if !msg.content.starts_with("[system]") {
                        self.transcript.push_block(Kind::User, seed_user_preview(&msg.content));
                    }
                }
                Role::Assistant => {
                    if !msg.content.is_empty() {
                        self.transcript.push_block(Kind::Assistant, msg.content.clone());
                    }
                    for tc in &msg.tool_calls {
                        let args = serde_json::to_string(&tc.function.arguments).unwrap_or_default();
                        self.log.push_block(Kind::Tool, tool_call_summary(&tc.function.name, &args));
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
        if self.transcript.blocks.len() > blocks_before {
            self.transcript.push_block(Kind::Info, "── session resumed ──".into());
        }
    }

    fn focused_pane(&mut self) -> &mut Pane {
        match self.focus {
            Focus::Transcript => &mut self.transcript,
            Focus::Log => self.log_like_pane(),
        }
    }

    /// The pane under a screen cell, if any — hidden panes render at a zero
    /// area, so at most one can match.
    fn pane_at(&self, col: u16, row: u16) -> Option<PaneId> {
        if self.log.contains(col, row) {
            Some(PaneId::Log)
        } else if self.diff.contains(col, row) {
            Some(PaneId::Diff)
        } else if self.transcript.contains(col, row) {
            Some(PaneId::Transcript)
        } else {
            None
        }
    }

    fn pane(&mut self, id: PaneId) -> &mut Pane {
        match id {
            PaneId::Transcript => &mut self.transcript,
            PaneId::Log => &mut self.log,
            PaneId::Diff => &mut self.diff,
        }
    }

    /// Drop any live text selection; true when there was one to drop.
    fn clear_selection(&mut self) -> bool {
        let had = self.transcript.sel.is_some() || self.log.sel.is_some() || self.diff.sel.is_some();
        self.transcript.sel = None;
        self.log.sel = None;
        self.diff.sel = None;
        self.drag = None;
        had
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
        if self.palette_off || self.picker.is_some() || self.answering.is_some() || self.notes_popup.is_some() {
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

    /// Switch the color theme by name (typed `/theme <name>` and the picker
    /// share this — the theme is pure UI state, never routed to the agent).
    /// Built-ins first, then custom JSON themes (user dir + plugins).
    fn apply_theme(&mut self, name: &str) {
        match theme::resolve(name, &self.cwd) {
            Some(th) => {
                self.theme = th;
                // Cached code spans hold the old syntect colors.
                self.transcript.set_syntax(th.syntax);
                self.transcript.dirty = true;
                self.log.dirty = true;
                self.diff.dirty = true;
                self.status = format!("theme: {}", th.name);
            }
            None => {
                self.transcript.push_block(
                    Kind::Warn,
                    format!("! unknown theme '{name}' — available: {}", theme::names().join(", ")),
                );
            }
        }
    }

    /// Clear the turn-in-flight state (busy flag, cancel token, spinner clock).
    fn idle(&mut self) {
        self.busy = false;
        self.cancel = None;
        self.turn_started = None;
    }

    fn handle_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Iteration(i) => {
                if i == 1 {
                    self.turn_reply.clear();
                }
                self.log.push_block(Kind::Info, format!("· step {i}"));
                self.status = format!("step {i} — waiting for {}…", self.model);
            }
            AgentEvent::Thinking(t) => self.transcript.append_stream(Kind::Thinking, &t),
            AgentEvent::Content(c) => {
                self.turn_reply.push_str(&c);
                self.transcript.append_stream(Kind::Assistant, &c);
            }
            AgentEvent::ToolStart { name, args } => {
                self.session_stats.tool_calls += 1;
                self.log.push_block(Kind::Tool, tool_call_summary(&name, &args));
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
            // The TUI's diff pane (Tab) already shows the working-tree diff;
            // the per-edit preview marks the applied change in the log, with
            // real ± coloring line by line.
            AgentEvent::EditDiff { path, diff } => {
                self.log.push_line(Kind::Tool, format!("✎ {path}"));
                for l in &diff {
                    self.log.push_line(diff_kind(l), l.clone());
                }
                self.log.push_line(Kind::Info, String::new());
            }
            AgentEvent::Info(i) => {
                if i.starts_with("compacted") {
                    self.session_stats.compactions += 1;
                }
                self.log.push_block(Kind::Info, format!("· {i}"));
            }
            // Sub-agent activity renders as the same tagged activity-log
            // lines it always has; the structure exists for --serve.
            AgentEvent::SubAgentStarted { tag, model, label } => {
                self.log.push_block(Kind::Info, format!("· ⧉ {tag} started ({model}): {label}"));
            }
            AgentEvent::SubAgentActivity { tag, text, .. } => {
                self.log.push_block(Kind::Info, format!("· [{tag}] {text}"));
            }
            AgentEvent::SubAgentFinished { tag, steps } => {
                self.log.push_block(Kind::Info, format!("· [{tag}] finished — {steps} step(s)"));
            }
            AgentEvent::Plan(items) => self.plan = items,
            AgentEvent::Context { used, limit } => {
                self.ctx_gauge = (limit > 0).then_some((used, limit));
            }
            AgentEvent::TaskStarted { id, label } => {
                self.bg_running.push((id, label.clone()));
                self.transcript.push_block(Kind::Info, format!("⚙ background task #{id} started: {label}"));
                self.log.push_block(Kind::Info, format!("⚙ bg #{id} started: {label}"));
            }
            AgentEvent::TaskFinished { id, label, ok, preview } => {
                self.bg_running.retain(|(tid, _)| *tid != id);
                let mark = if ok { '✓' } else { '✗' };
                self.transcript.push_block(
                    if ok { Kind::Info } else { Kind::Warn },
                    format!("⚙ background task #{id} {mark} finished: {label}"),
                );
                self.log.push_block(Kind::Info, format!("⚙ bg #{id} {mark} finished: {label}"));
                // Queue the model-facing notification; the idle tick turns
                // pending notes into one auto turn (never mid-turn).
                let outcome = if ok { "completed successfully" } else { "FAILED" };
                let tail = if preview.trim().is_empty() {
                    String::new()
                } else {
                    format!("\noutput tail:\n{}", preview.trim())
                };
                self.task_notes.push(format!("Task #{id} ({label}) {outcome}.{tail}"));
            }
            AgentEvent::Warning(w) => {
                self.log.push_block(Kind::Warn, format!("! {w}"));
                self.transcript.push_block(Kind::Warn, format!("! {w}"));
            }
            AgentEvent::Done(stats) => {
                // idle() clears the cancel token — read it first.
                let cancelled = self.cancel.as_ref().is_some_and(|c| c.is_cancelled());
                self.idle();
                // Catch changes made via bash (git checkout, formatters…) too.
                self.diff_stale = true;
                if stats.iterations > 0 {
                    self.session_stats.turns += 1;
                    self.session_stats.model_calls += stats.iterations as u64;
                    self.session_stats.output_tokens += stats.output_tokens;
                    self.session_stats.prompt_tokens += stats.prompt_tokens;
                    self.session_stats.billed_prompt_tokens += stats.billed_prompt_tokens;
                    self.session_stats.duration_ms += stats.duration_ms;
                    self.session_stats.failures.add(&stats.failures);
                }
                self.status = format!(
                    "done — {} steps · {} prompt tok · {} out tok · {:.1} tok/s",
                    stats.iterations, stats.prompt_tokens, stats.output_tokens, stats.tokens_per_sec
                );
                self.after_turn(cancelled, stats.iterations > 0);
            }
        }
    }

    /// Post-turn /goal and /loop bookkeeping. `ran` is false when the turn
    /// errored out before a single model step (don't auto-continue into a
    /// tight error loop).
    fn after_turn(&mut self, cancelled: bool, ran: bool) {
        if cancelled {
            // Esc is the escape hatch from auto-continuation — always honor it.
            self.pending_auto = None;
            // Don't fire a surprise notification turn right after a cancel;
            // the completions already showed in the transcript.
            self.task_notes.clear();
            if let Some(g) = self.goal.take() {
                self.transcript
                    .push_block(Kind::Info, format!("◎ goal cancelled — /goal {} resumes it", g.condition));
            }
            if self.loop_state.take().is_some() {
                self.transcript.push_block(Kind::Info, "↻ loop stopped".into());
            }
            return;
        }
        if let Some(mut goal) = self.goal.take() {
            if goal_met(&self.turn_reply) {
                self.transcript
                    .push_block(Kind::Info, format!("◎ goal met after {} turn(s): {}", goal.runs, goal.condition));
                self.status = "◎ goal met".into();
            } else if !ran {
                self.transcript.push_block(
                    Kind::Warn,
                    format!("! goal paused — the turn failed; /goal {} resumes it", goal.condition),
                );
            } else if goal.runs >= GOAL_MAX_RUNS {
                self.transcript.push_block(
                    Kind::Warn,
                    format!(
                        "! goal not met after {GOAL_MAX_RUNS} turns — stopping; /goal {} resumes it",
                        goal.condition
                    ),
                );
            } else {
                goal.runs += 1;
                self.transcript.push_block(
                    Kind::Info,
                    format!("◎ goal not met — continuing (turn {}/{GOAL_MAX_RUNS}) · Esc or /goal clear stops", goal.runs),
                );
                self.pending_auto = Some(goal_continuation(&goal.condition));
                self.goal = Some(goal);
            }
        }
        // A failing back-to-back loop would spin on the error; interval
        // loops ride out one bad run.
        if !ran && self.loop_state.as_ref().is_some_and(|l| l.every.is_none()) {
            self.loop_state = None;
            self.transcript.push_block(Kind::Warn, "! loop stopped — the last run failed".into());
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
            UiEffect::Restart => {
                self.restart = true;
                self.quit = true;
            }
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
            UiEffect::Pasted(url, kb) => {
                self.pending_paste.push(url);
                self.transcript.push_block(
                    Kind::Info,
                    format!(
                        "📋 clipboard image attached ({kb} KB, {} staged) — it goes with your next message (vision models)",
                        self.pending_paste.len()
                    ),
                );
                self.status = "image staged — type your message".into();
            }
            UiEffect::Btw { question, reply, ok } => {
                self.btw_busy = false;
                if ok {
                    self.transcript.push_block(Kind::Thinking, format!("(btw) {}", reply.trim()));
                    self.btw_exchanges.push((question, reply));
                    // Keep the side thread bounded — it rides along on every
                    // follow-up side question.
                    const BTW_KEEP: usize = 10;
                    if self.btw_exchanges.len() > BTW_KEEP {
                        let excess = self.btw_exchanges.len() - BTW_KEEP;
                        self.btw_exchanges.drain(..excess);
                    }
                    if !self.busy {
                        self.status = "btw: answered — /btw continues the side thread".into();
                    }
                } else {
                    self.transcript.push_block(Kind::Warn, format!("! (btw) {reply}"));
                }
            }
            // Handled by the event loop (need stdout/terminal/restart state);
            // never reach here.
            UiEffect::Osc52(_) => {}
            UiEffect::EditFile(..) => {}
            UiEffect::Host(_) => {}
            UiEffect::Status(status) => self.status = status,
            UiEffect::Done(status) => {
                self.idle();
                self.status = status;
            }
        }
    }
}

/// Max input rows shown at once before the box stops growing and scrolls to
/// follow the cursor — keeps a long paste from eating the whole screen.
const MAX_INPUT_ROWS: usize = 8;

/// Columns available for input text on one row: the terminal width minus the
/// block borders (2) and the "❯ "/"… " prefix (2). At least 1.
fn input_content_width(term_width: u16) -> usize {
    (term_width as usize).saturating_sub(4).max(1)
}

/// Wrap the input to `content_w` columns, returning the display rows (prefix
/// excluded) plus the cursor's (row, column) among them. Every logical line
/// (split on '\n') hard-wraps into chunks of `content_w`; a cursor sitting
/// exactly at the end of a full-width line gets its own trailing row, the way
/// a terminal wraps it. This is what lets the input box scale with the window
/// instead of running text off the right edge.
fn wrap_input(input: &str, cursor_byte: usize, content_w: usize) -> (Vec<String>, usize, usize) {
    let w = content_w.max(1);
    let cursor = cursor_byte.min(input.len());
    let cur_line = input[..cursor].matches('\n').count();
    let line_start = input[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let cursor_char_col = input[line_start..cursor].chars().count();

    let mut rows: Vec<String> = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_col = 0;
    for (li, line) in input.split('\n').enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let base = rows.len();
        let mut off = 0;
        while off < chars.len() {
            let end = (off + w).min(chars.len());
            rows.push(chars[off..end].iter().collect());
            off = end;
        }
        if chars.is_empty() {
            rows.push(String::new());
        }
        if li == cur_line {
            let seg = cursor_char_col / w;
            if seg >= rows.len() - base {
                // Cursor one past the last full-width segment — its own row.
                rows.push(String::new());
                cursor_row = rows.len() - 1;
                cursor_col = 0;
            } else {
                cursor_row = base + seg;
                cursor_col = cursor_char_col % w;
            }
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    (rows, cursor_row, cursor_col)
}

/// Height of the input pane (including borders) for the current text, cursor,
/// and terminal width — grows with wrapped rows up to [`MAX_INPUT_ROWS`].
fn input_height(input: &str, cursor: usize, term_width: u16) -> u16 {
    let (rows, _, _) = wrap_input(input, cursor, input_content_width(term_width));
    (rows.len().clamp(1, MAX_INPUT_ROWS) as u16) + 2
}

/// `"editor"` from the user config; beats $EDITOR/$VISUAL. A global because
/// the resolver runs in the UI event loop, far from where the config lives;
/// set at startup and again on /config reload so edits take effect live.
static CONFIG_EDITOR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub(crate) fn set_config_editor(editor: Option<String>) {
    if let Ok(mut e) = CONFIG_EDITOR.lock() {
        *e = editor.filter(|s| !s.trim().is_empty());
    }
}

/// The configured editor and where it came from — config `"editor"`,
/// `$EDITOR`, `$VISUAL`, else the platform default (a terminal editor — see
/// `default_editor`). May carry flags, e.g. "code -w". The source lets
/// `/config edit` say what it picked and how to change it.
pub(crate) fn resolve_editor_with_source() -> (String, &'static str) {
    if let Some(e) = CONFIG_EDITOR.lock().ok().and_then(|e| e.clone()) {
        return (e, "config \"editor\"");
    }
    for (var, source) in [("EDITOR", "$EDITOR"), ("VISUAL", "$VISUAL")] {
        if let Ok(e) = std::env::var(var) {
            if !e.trim().is_empty() {
                return (e, source);
            }
        }
    }
    (default_editor(), "default")
}

fn resolve_editor() -> String {
    resolve_editor_with_source().0
}

/// The fallback when neither `$EDITOR` nor `$VISUAL` is set: an editor that
/// opens *in* the terminal, so `/config edit` stays in the console instead
/// of popping the file open in a notepad window. Unix always has vi. On
/// Windows nothing terminal-based is guaranteed, so probe PATH — `edit`
/// (Microsoft's terminal editor, in-box on Windows 11) first, then common
/// installs — and only fall back to notepad when none is found.
fn default_editor() -> String {
    if cfg!(windows) {
        pick_terminal_editor(on_path).unwrap_or_else(|| "notepad".into())
    } else {
        "vi".into()
    }
}

/// First of the known terminal editors that `available` reports present,
/// in preference order.
fn pick_terminal_editor(available: impl Fn(&str) -> bool) -> Option<String> {
    ["edit", "nano", "vim", "nvim", "vi", "hx", "micro"]
        .into_iter()
        .find(|name| available(name))
        .map(str::to_string)
}

/// Is `name` resolvable as a program on PATH? On Windows each `PATHEXT`
/// extension is tried, matching what CreateProcess would launch.
fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    std::env::split_paths(&path).any(|dir| {
        if dir.as_os_str().is_empty() {
            return false;
        }
        if exts.is_empty() {
            dir.join(name).is_file()
        } else {
            exts.iter().any(|ext| dir.join(format!("{name}{ext}")).is_file())
        }
    })
}

/// Does this editor open in its own window (so the TUI should stay up and
/// wait) rather than needing the terminal handed to it? Recognized GUI
/// editors get the dimmed "close the file" modal; everything else — vim,
/// nano, emacs -nw, or anything unknown — keeps the safe TTY handover, so a
/// terminal editor is never left fighting the TUI for the same console.
fn editor_is_gui(editor: &str) -> bool {
    let stem = |s: &str| {
        std::path::Path::new(s)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(s)
            .to_ascii_lowercase()
    };
    // Two candidates cover both shapes an EDITOR takes: the whole string
    // (an unquoted spaced path like `C:\Program Files\…\notepad++.exe` →
    // "notepad++") and its first token (a program with trailing flags like
    // `code -w` → "code"). Either matching the GUI set is enough.
    let editor = editor.trim().trim_matches(['"', '\'']);
    let whole = stem(editor);
    let first = stem(editor.split_whitespace().next().unwrap_or(editor));
    let is_gui = |base: &str| {
        matches!(
            base,
            "notepad"
            | "notepad++"
            | "notepad2"
            | "wordpad"
            | "write"
            | "code"
            | "code-insiders"
            | "codium"
            | "vscodium"
            | "cursor"
            | "windsurf"
            | "zed"
            | "subl"
            | "sublime_text"
            | "atom"
            | "gedit"
            | "kate"
            | "gvim"
            | "mousepad"
            | "notepadqq"
            | "textedit"
        )
    };
    is_gui(&whole) || is_gui(&first)
}

/// Launch a GUI editor on `path` without blocking, returning its handle so
/// the event loop can watch for the window closing. Direct spawn first (std
/// handles spaced paths and .cmd/.bat on Windows); fall back to a shell only
/// when that fails, e.g. `EDITOR` embeds flags like "code -w".
fn spawn_gui_editor(editor: &str, path: &std::path::Path) -> std::io::Result<std::process::Child> {
    std::process::Command::new(editor).arg(path).spawn().or_else(|_| {
        #[cfg(windows)]
        {
            let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
            std::process::Command::new(shell)
                .arg("/C")
                .arg(format!("{editor} \"{}\"", path.display()))
                .spawn()
        }
        #[cfg(not(windows))]
        {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{editor} '{}'", path.display()))
                .spawn()
        }
    })
}

/// " 13k/32k" — compact token counts for the status-bar context gauge.
fn fmt_ctx_detail(used: u64, limit: u64) -> String {
    format!(" {}/{}", fmt_k(used), fmt_k(limit))
}

fn fmt_k(n: u64) -> String {
    if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    // Themes with their own background/text paint the whole frame first;
    // widgets then draw over it (unstyled cells keep this fg/bg, styled
    // spans override fg only). Terminal-native themes skip this entirely.
    {
        let t = app.theme;
        if t.bg.is_some() || t.fg.is_some() {
            let mut style = Style::default();
            if let Some(bg) = t.bg {
                style = style.bg(bg);
            }
            if let Some(fg) = t.fg {
                style = style.fg(fg);
            }
            frame.render_widget(Block::new().style(style), frame.area());
        }
    }
    let [main_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(input_height(&app.input, app.cursor, frame.area().width)),
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
    let unfocused_style = Style::default().fg(t.border);

    // Transcript pane.
    {
        let focused = app.focus == Focus::Transcript || log_area.is_none();
        let block = Block::bordered()
            .title(" Rift ")
            .border_style(if focused { focused_style } else { unfocused_style });
        let inner = block.inner(transcript_area);
        app.transcript.area = inner;
        app.transcript.rebuild(inner.width, &t);
        app.transcript.view_height = inner.height as usize;
        let lines = app.transcript.visible_lines(&t);
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
        pane.rebuild(log_inner.width, &t);
        pane.view_height = log_inner.height as usize;
        let lines = pane.visible_lines(&t);
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
    let bg_note = match app.bg_running.len() {
        0 => String::new(),
        1 => " · 1 bg task running".into(),
        n => format!(" · {n} bg tasks running"),
    };
    let bg_note = if app.btw_busy { format!("{bg_note} · btw pending") } else { bg_note };
    // Context gauge: estimated fill of the working num_ctx, colored by how
    // close the next request is to the window (compaction kicks in near the
    // top, so red is "compaction imminent", not "about to fail").
    let (ctx_note, ctx_color) = match app.ctx_gauge {
        Some((used, limit)) => {
            let pct = (used.saturating_mul(100) / limit).min(999);
            let color = match pct {
                0..=59 => t.ok,
                60..=84 => t.warn,
                _ => t.error,
            };
            (format!("ctx {pct}%{} · ", fmt_ctx_detail(used, limit)), color)
        }
        None => (String::new(), t.muted),
    };
    let status = Line::from(vec![
        Span::styled(format!(" {} ", app.model), Style::default().fg(t.sel_fg).bg(t.accent)),
        Span::styled(busy_marker, Style::default().fg(if app.busy { t.warn } else { t.ok })),
        Span::styled(ctx_note, Style::default().fg(ctx_color)),
        Span::styled(format!("{}{elapsed_note}", app.status), Style::default().fg(t.muted)),
        Span::styled(bg_note, Style::default().fg(t.warn)),
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
    app.cursor = app.cursor.min(app.input.len());
    // Wrap to the pane width so long lines fold instead of running off the
    // right edge; the visible window scrolls to keep the cursor in view.
    let content_w = (input_inner.width as usize).saturating_sub(2).max(1);
    let (rows, cursor_row, cursor_col) = wrap_input(&app.input, app.cursor, content_w);
    let visible = (input_inner.height as usize).max(1);
    let start = if cursor_row >= visible { cursor_row + 1 - visible } else { 0 };
    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in rows.iter().enumerate().skip(start).take(visible) {
        // ❯ marks the very first row; every wrap/continuation row gets ….
        let prefix = if i == 0 { "❯ " } else { "… " };
        let mut spans = vec![Span::styled(prefix, Style::default().fg(t.accent))];
        if i == cursor_row {
            let chars: Vec<char> = row.chars().collect();
            let split = cursor_col.min(chars.len());
            let before: String = chars[..split].iter().collect();
            let (under, after) = if split < chars.len() {
                (chars[split].to_string(), chars[split + 1..].iter().collect::<String>())
            } else {
                (" ".to_string(), String::new())
            };
            spans.push(Span::raw(before));
            spans.push(Span::styled(under, Style::default().add_modifier(Modifier::REVERSED)));
            spans.push(Span::raw(after));
        } else {
            spans.push(Span::raw(row.clone()));
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

    // Config-editing modal: a GUI editor holds the file open in its own
    // window. Dim the whole frame and show a centered "close the file"
    // notice so the TUI stays visibly present (not blanked) while it waits.
    if let Some((editor, path)) = &app.editing_file {
        // Dim everything behind the modal without restyling each span: a
        // full-frame DIM overlay reads as "greyed out / inactive".
        frame.render_widget(
            Block::new().style(Style::default().add_modifier(Modifier::DIM)),
            frame.area(),
        );
        let body = vec![
            Line::from(Span::styled(
                "⚙  Editing config",
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(Span::styled(path.clone(), Style::default().fg(t.muted))),
            Line::raw(""),
            Line::from(Span::styled(
                format!("Open in {editor}. Save and close the file to reload."),
                Style::default().fg(t.fg.unwrap_or(Color::Reset)),
            )),
            Line::from(Span::styled(
                "The rift window is waiting — it resumes on its own.",
                Style::default().fg(t.muted),
            )),
        ];
        let width = frame.area().width.saturating_sub(8).clamp(40, 76);
        let height = (body.len() as u16 + 2).min(frame.area().height);
        let area = Rect {
            x: (frame.area().width.saturating_sub(width)) / 2,
            y: (frame.area().height.saturating_sub(height)) / 2,
            width,
            height,
        };
        let block = Block::bordered()
            .title(" close the file to continue ")
            .border_style(Style::default().fg(t.warn));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(body).alignment(ratatui::layout::Alignment::Center), inner);
    }

    // Release-notes popup: dimmed background + a centered, scrollable box.
    // Drawn last so it sits above every pane and the picker.
    if let Some(n) = &mut app.notes_popup {
        frame.render_widget(
            Block::new().style(Style::default().add_modifier(Modifier::DIM)),
            frame.area(),
        );
        let fw = frame.area().width;
        let fh = frame.area().height;
        let width = fw.saturating_sub(6).clamp(40, 92);
        let height = fh.saturating_sub(4).clamp(6, 30);
        let area = Rect {
            x: (fw.saturating_sub(width)) / 2,
            y: (fh.saturating_sub(height)) / 2,
            width,
            height,
        };
        let inner_w = width.saturating_sub(2) as usize;
        let inner_h = height.saturating_sub(2) as usize;
        let all = render_notes_lines(&n.body, inner_w, &t);
        // Clamp scroll so the last page can't scroll off into blank space,
        // and write the clamped value back (End/`G` sets usize::MAX).
        let max_scroll = all.len().saturating_sub(inner_h);
        n.scroll = n.scroll.min(max_scroll);
        let more = if n.scroll < max_scroll { " ↓ more " } else { "" };
        let shown: Vec<Line> = all.into_iter().skip(n.scroll).take(inner_h).collect();
        let title: String = n.title.chars().take(inner_w.saturating_sub(26)).collect();
        let block = Block::bordered()
            .title(format!(" What's new — {title} "))
            .title_bottom(format!(" ↑↓ scroll · Esc close{more}"))
            .border_style(Style::default().fg(t.accent));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(shown), inner);
    }
}

/// Style + word-wrap the raw changelog body for the popup: `**bold**` runs
/// render in the accent color, `- ` list items get a `•` and a hanging
/// indent, everything wrapped to `width`. Kept deliberately small — this is
/// one embedded document, not a general markdown renderer.
fn render_notes_lines(body: &[String], width: usize, t: &theme::Theme) -> Vec<Line<'static>> {
    let width = width.max(8);
    let base = Style::default().fg(t.fg.unwrap_or(Color::Reset));
    let bold = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
    let mut out: Vec<Line> = Vec::new();
    for raw in body {
        let trimmed = raw.trim_start();
        let (prefix, indent, content) = if let Some(rest) = trimmed.strip_prefix("- ") {
            ("• ", "  ", rest)
        } else if let Some(rest) = trimmed.strip_prefix("* ") {
            ("• ", "  ", rest)
        } else {
            ("", "", trimmed)
        };
        if content.is_empty() {
            out.push(Line::raw(""));
            continue;
        }
        // Split the content into styled runs on `**` boundaries, then greedily
        // word-wrap those runs into lines of at most `width`.
        let runs = split_bold(content, base, bold);
        let mut line: Vec<Span> = vec![Span::styled(prefix.to_string(), bold)];
        let mut col = prefix.chars().count();
        let mut first = true;
        for (style, text) in runs {
            for word in text.split_inclusive(' ') {
                let wlen = word.chars().count();
                if !first && col + wlen > width {
                    out.push(Line::from(std::mem::take(&mut line)));
                    line.push(Span::raw(indent.to_string()));
                    col = indent.chars().count();
                }
                line.push(Span::styled(word.to_string(), style));
                col += wlen;
                first = false;
            }
        }
        if !line.is_empty() {
            out.push(Line::from(line));
        }
    }
    out
}

/// Split a string on `**…**` into (style, text) runs — bold inside the
/// markers, base outside. Unbalanced markers fall back to base text.
fn split_bold(s: &str, base: Style, bold: Style) -> Vec<(Style, String)> {
    let mut runs = vec![];
    let mut rest = s;
    while let Some(open) = rest.find("**") {
        if open > 0 {
            runs.push((base, rest[..open].to_string()));
        }
        let after = &rest[open + 2..];
        match after.find("**") {
            Some(close) => {
                runs.push((bold, after[..close].to_string()));
                rest = &after[close + 2..];
            }
            None => {
                // No closing marker — emit the rest verbatim (with the `**`).
                runs.push((base, rest[open..].to_string()));
                rest = "";
                break;
            }
        }
    }
    if !rest.is_empty() {
        runs.push((base, rest.to_string()));
    }
    runs
}

/// Expand `@path` tokens: for each existing file, append an outline (or a
/// capped head for unsupported types) to the prompt. Token-stingy on purpose
/// — the model can still `read` for exact lines. Returns the expanded prompt
/// and activity-log notes about what was attached.
fn expand_mentions(input: &str, cwd: &std::path::Path) -> (String, Vec<String>, Vec<String>) {
    let mut expanded = input.to_string();
    let mut notes = Vec::new();
    let mut images = Vec::new();
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
        // Images attach as base64 for vision models; text keeps the
        // token-stingy outline treatment.
        if let Some(mime) = image_media_type(&path) {
            match read_image_data_url(&path, mime) {
                Ok((url, kb)) => {
                    images.push(url);
                    expanded.push_str(&format!("\n\n[attached image {rel} — mentioned as @{rel}]"));
                    notes.push(format!("attached image {rel} ({kb} KB — needs a vision-capable model)"));
                }
                Err(e) => notes.push(format!("@{rel}: {e} — sent as plain text")),
            }
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
    (expanded, notes, images)
}

/// Grab an image from the system clipboard into a temp PNG and return it as
/// a data URL (for /paste). Best-effort per platform: PowerShell on Windows,
/// pngpaste on macOS, wl-paste/xclip on Linux.
pub(crate) fn clipboard_image_data_url() -> anyhow::Result<(String, u64)> {
    let out = std::env::temp_dir().join(format!("rift-paste-{}.png", std::process::id()));
    let path_str = out.display().to_string();
    #[cfg(windows)]
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-STA",
            "-Command",
            &format!(
                "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
                 $img = [System.Windows.Forms.Clipboard]::GetImage(); \
                 if ($img -eq $null) {{ exit 2 }}; \
                 $img.Save('{path_str}', [System.Drawing.Imaging.ImageFormat]::Png)"
            ),
        ])
        .status();
    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("pngpaste").arg(&path_str).status();
    #[cfg(all(unix, not(target_os = "macos")))]
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "wl-paste --type image/png > '{path_str}' 2>/dev/null || xclip -selection clipboard -t image/png -o > '{path_str}'"
        ))
        .status();
    match status {
        Ok(s) if s.success() && out.is_file() && std::fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false) => {}
        Ok(_) => anyhow::bail!("no image on the clipboard (copy a screenshot first)"),
        Err(e) => anyhow::bail!("clipboard tool unavailable: {e}"),
    }
    let result = read_image_data_url(&out, "image/png");
    let _ = std::fs::remove_file(&out);
    result
}

/// Image media type by extension; None = not an image we attach.
pub(crate) fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// Read an image file into a data URL, capped so a stray screenshot dump
/// can't blow up the request. Returns (data URL, size in KB).
pub(crate) fn read_image_data_url(path: &std::path::Path, mime: &str) -> anyhow::Result<(String, u64)> {
    use base64::Engine;
    const IMAGE_MAX_BYTES: u64 = 10 * 1024 * 1024;
    let meta = std::fs::metadata(path)?;
    if meta.len() > IMAGE_MAX_BYTES {
        anyhow::bail!("image is {} MB (max 10 MB)", meta.len() / (1024 * 1024));
    }
    let bytes = std::fs::read(path)?;
    let kb = (bytes.len() as u64) / 1024;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((format!("data:{mime};base64,{encoded}"), kb))
}

/// Compact display form of a seeded (resumed) user message. Expanded command
/// prompts (/mcp new, /init, goal continuations…) run pages long and would
/// wall off the resumed transcript — the user never saw the expansion live
/// either, only the line they typed. Bookkeeping prefixes (the Esc-interrupt
/// note) are display noise and get stripped.
fn seed_user_preview(content: &str) -> String {
    let content = match content.strip_prefix("[note: the user pressed Esc") {
        Some(rest) => rest.split_once("]\n\n").map(|(_, real)| real).unwrap_or(content),
        None => content,
    };
    const KEEP_LINES: usize = 8;
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= KEEP_LINES {
        content.to_string()
    } else {
        format!("{}\n… [{} more lines]", lines[..KEEP_LINES].join("\n"), lines.len() - KEEP_LINES)
    }
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

/// The /deep-research command body: a canned orchestration prompt through
/// the normal agent loop — fan out searches, delegate source-reading to
/// concurrent sub-agents, cross-check, synthesize a cited report.
const DEEP_RESEARCH_PROMPT: &str = "Run a deep-research workflow on this question:

{q}

Work in phases:
1. PLAN: decompose the question into 3-6 distinct search angles (definitions, comparisons, recent developments, criticisms, data). Use the plan tool to track them.
2. SEARCH: run web_search for each angle (vary phrasing; prefer specific queries over broad ones). Collect the most promising 6-12 source URLs across all angles.
3. READ (delegate!): use the agent tool with up to 4 concurrent tasks per call, as many calls as needed, to read sources in parallel. Each task's prompt must name 2-3 URLs and instruct: 'fetch each URL, extract the claims relevant to <question>, quote key passages verbatim with their URL, note publication dates and obvious bias'. Sub-agents have fetch and web_search tools.
4. CROSS-CHECK: compare the sub-agents' findings — flag claims sources disagree on, note which are corroborated by 2+ independent sources, and mark anything single-sourced as such.
5. REPORT: write the final answer in markdown — a 2-3 sentence executive summary, the findings organized by theme with inline [n] citation markers, a disagreements/caveats section when sources conflict, and a numbered Sources list mapping [n] to URLs. Cite honestly: every non-obvious claim gets a citation; say clearly when evidence is thin.

If web_search reports it is not configured, stop and tell the user to run /search <searxng-url>.";

/// The `/init` command body — a normal agent turn with a canned prompt.
const INIT_PROMPT: &str = "Explore this repository (use repo_map and outline to stay cheap; read key files only as needed) and write a RIFT.md file at the project root: a concise guide for AI coding agents working here. Cover: what the project does, how the code is laid out, how to build/test/run it, and any conventions or gotchas you noticed. Keep it under 60 lines.";

/// /goal auto-continuation cap — the backstop against a condition the model
/// can never verify. /goal again resumes where it stopped.
const GOAL_MAX_RUNS: u32 = 25;
/// /loop run cap — a week of half-hourly runs; /loop again re-arms.
const LOOP_MAX_RUNS: u32 = 336;

struct GoalState {
    condition: String,
    runs: u32,
}

struct LoopState {
    /// The prompt (or /command) to fire each round.
    body: String,
    /// None = back-to-back: the next run fires as soon as one finishes.
    every: Option<Duration>,
    next_at: Instant,
    runs: u32,
}

fn goal_initial(condition: &str) -> String {
    format!(
        "GOAL: {condition}\n\nWork toward this goal now. When — and only when — the goal is \
         fully met and you have VERIFIED it, include a line starting with GOAL MET in your \
         reply plus a one-line summary. If you cannot verify it yet, keep working instead of \
         claiming it."
    )
}

fn goal_continuation(condition: &str) -> String {
    format!(
        "[goal check] The session goal is not yet confirmed met:\n{condition}\n\nContinue \
         working toward it. When it is fully met and VERIFIED, include a line starting with \
         GOAL MET in your reply. Never write GOAL MET without having verified the condition \
         this turn."
    )
}

/// Did the model declare (line-anchored, to dodge instruction echoes) that
/// the goal is met?
fn goal_met(reply: &str) -> bool {
    reply.lines().any(|l| l.trim_start().starts_with("GOAL MET"))
}

/// Run one /btw side question on a UI-side task (modeled on Claude Code's
/// /btw): it sees the whole conversation but has no tools, its exchange never
/// enters the main history, and — because it bypasses the agent task entirely
/// — it works while the agent is mid-turn. Conversation context comes from
/// the session autosave (history up to the last completed turn).
fn spawn_btw(
    fx: mpsc::UnboundedSender<UiEffect>,
    ctx: rift_core::ToolCtx,
    session: PathBuf,
    prior: Vec<(String, String)>,
    question: String,
) {
    tokio::spawn(async move {
        let Some(handle) = ctx.subagent_handle() else {
            let _ = fx.send(UiEffect::Btw {
                question,
                reply: "no provider is ready yet — send one normal message first".into(),
                ok: false,
            });
            return;
        };
        // Snapshot of the main conversation. Empty (fresh session, no turn
        // saved yet) still works — the question just has no history behind it.
        let mut messages: Vec<Message> = std::fs::read_to_string(&session)
            .ok()
            .and_then(|s| serde_json::from_str::<rift_core::SavedSession>(&s).ok())
            .map(|s| s.messages)
            .unwrap_or_default();
        if messages.first().map(|m| m.role) != Some(Role::System) {
            messages.insert(0, Message::system("You are a helpful, concise assistant."));
        }
        // Old reasoning isn't needed to answer an aside; drop it from the
        // request (same trim the agent applies to its own requests).
        for m in &mut messages {
            m.thinking = None;
        }
        // Prior side exchanges make /btw a small conversation of its own.
        for (q, a) in &prior {
            messages.push(Message::user(format!("[side question] {q}")));
            messages.push(Message {
                role: Role::Assistant,
                content: a.clone(),
                thinking: None,
                tool_calls: vec![],
                tool_name: None,
                tool_call_id: None,
                provider_data: None,
                images: vec![],
            });
        }
        messages.push(Message::user(format!(
            "[side question — an aside from the user, NOT part of the main task; this exchange \
             is not kept in the conversation] {question}\n\nAnswer directly and concisely from \
             what you already know in this conversation. You have NO tools for this answer — \
             never emit tool calls; if you would need to look something up, say what you'd check \
             instead. If the question is unrelated to the project, simply answer it."
        )));
        let req = ChatRequest {
            model: handle.cfg.model.clone(),
            messages,
            tools: vec![],
            stream: true,
            think: handle.cfg.think,
            effort: handle.cfg.effort.clone(),
            keep_alive: Some("10m".into()),
            options: Some(ChatOptions {
                num_ctx: Some(handle.cfg.num_ctx),
                temperature: handle.cfg.temperature,
                num_predict: None,
            }),
        };
        // Buffered, not streamed into the transcript: the main turn may be
        // streaming there at the same time, and two interleaved streams would
        // garble both. The answer lands as one block.
        let mut streamed = String::new();
        let mut on_delta = |d: StreamDelta| {
            if let StreamDelta::Content(c) = d {
                streamed.push_str(&c);
            }
        };
        match handle.client.chat_stream(&req, &mut on_delta).await {
            Ok(out) => {
                let reply = if out.message.content.trim().is_empty() { streamed } else { out.message.content };
                let reply =
                    if reply.trim().is_empty() { "(the model sent no answer)".to_string() } else { reply };
                let _ = fx.send(UiEffect::Btw { question, reply, ok: true });
            }
            Err(e) => {
                let _ = fx.send(UiEffect::Btw { question, reply: format!("side question failed: {e:#}"), ok: false });
            }
        }
    });
}

/// `30s` / `5m` / `2h` / bare seconds → Duration.
fn parse_interval(tok: &str) -> Option<Duration> {
    let (num, unit) = match tok.chars().last()? {
        c @ ('s' | 'm' | 'h') => (&tok[..tok.len() - 1], c),
        c if c.is_ascii_digit() => (tok, 's'),
        _ => return None,
    };
    let n: u64 = num.parse().ok().filter(|n| *n > 0)?;
    Some(Duration::from_secs(match unit {
        'm' => n * 60,
        'h' => n * 3600,
        _ => n,
    }))
}

fn fmt_interval(d: Duration) -> String {
    let s = d.as_secs();
    if s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// `--global` (or `-g`) as the first word of a generator command targets the
/// user-wide config; default is project scope. Returns (global, description).
fn parse_scope_flag(rest: &str) -> (bool, String) {
    let rest = rest.trim();
    for flag in ["--global", "-g", "--user"] {
        if let Some(desc) = rest.strip_prefix(flag) {
            // Only when it's a whole word, not a prefix of the description.
            if desc.is_empty() || desc.starts_with(char::is_whitespace) {
                return (true, desc.trim().to_string());
            }
        }
    }
    (false, rest.to_string())
}

/// Where a generated skill lands: (directory for the prompt, scope note).
fn skill_target(global: bool) -> (String, &'static str) {
    if global {
        let home = rift_core::paths::config_dir().unwrap_or_else(|| std::path::PathBuf::from(".config"));
        (
            home.join("rift/skills").display().to_string(),
            "user-wide: available in every project",
        )
    } else {
        (".rift/skills".into(), "project-scoped: sessions in this repo only")
    }
}

struct McpPlacement {
    /// Directory the server file goes in.
    server_dir: String,
    /// Config file to merge the mcp entry into.
    config_path: String,
    /// The args[0] path to register — MUST be absolute for global scope
    /// (relative paths resolve against whatever cwd rift runs from).
    args_path: String,
    trust_note: &'static str,
}

fn mcp_placement(global: bool) -> McpPlacement {
    if global {
        let cfg = rift_core::paths::config_dir().unwrap_or_else(|| std::path::PathBuf::from(".config"));
        let dir = cfg.join("rift/mcp");
        McpPlacement {
            server_dir: dir.display().to_string(),
            config_path: cfg.join("rift/config.json").display().to_string(),
            args_path: dir.join("<name>.py").display().to_string(),
            trust_note: "it is registered user-wide, so it loads in every project without a trust prompt (user config is the user's own machine) — mention they can review the server file before restarting",
        }
    } else {
        McpPlacement {
            server_dir: ".rift/mcp".into(),
            config_path: ".rift.json".into(),
            args_path: ".rift/mcp/<name>.py".into(),
            trust_note: "rift will ask once to trust it (project-config MCP entries always are)",
        }
    }
}

/// /skills new — the agent writes its own skill file (the /init pattern:
/// a canned prompt through the normal agent loop, using the write tool).
const SKILL_NEW_PROMPT: &str = "Create a new rift skill from this description:\n\n{desc}\n\nSkills are markdown instruction files the agent loads on demand. Write ONE file at {dir}/<name>.md ({scope}) — pick a short kebab-case <name> — in exactly this format:\n\n---\nname: <name>\ndescription: <one line saying when to use this skill — this line is shown to the model every session, so make it a good trigger>\n---\n<skill body>\n\nBody guidelines: imperative instructions for a coding agent, not prose for humans; concrete steps and commands; include a verification step; keep it under 80 lines; include only what an agent could NOT infer by exploring the repo. If the description is ambiguous and an ask_user tool is available, ask ONE clarifying question before writing. After writing, read the file back to confirm the frontmatter parses (--- fences, name and description lines). Finish by telling the user: the skill loads at startup — run /restart now to use it as /skill:<name> without losing this chat.";

/// /mcp new — the agent writes AND self-tests a stdio MCP server matching
/// the exact protocol subset rift's client speaks, then registers it in the
/// project config (which is trust-gated at startup, so the user reviews it).
const MCP_NEW_PROMPT: &str = "Build a local MCP (Model Context Protocol) server from this description:\n\n{desc}\n\nWrite ONE self-contained Python 3 file at {server_dir}/<name>.py (pick a short kebab-case <name>) using ONLY the Python standard library.\n\nProtocol contract (JSON-RPC 2.0 over stdio; one JSON object per line; read requests line-by-line from stdin; write responses to stdout; anything you log goes to stderr — NEVER print non-JSON to stdout):\n- request method \"initialize\" -> respond {\"jsonrpc\":\"2.0\",\"id\":<same id>,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"<name>\",\"version\":\"0.1.0\"}}}\n- notification \"notifications/initialized\" (no id) -> send nothing back\n- request \"tools/list\" -> result {\"tools\":[{\"name\":\"...\",\"description\":\"...\",\"inputSchema\":{<JSON Schema object with type/properties/required>}}]}\n- request \"tools/call\" with params {\"name\":...,\"arguments\":{...}} -> the result MUST be an OBJECT of exactly this shape: {\"content\":[{\"type\":\"text\",\"text\":\"<output>\"}]} — never a bare array; on tool failure return {\"content\":[{\"type\":\"text\",\"text\":\"<error message>\"}],\"isError\":true} (not a JSON-RPC error)\n- any other request with an id -> JSON-RPC error response; ignore unknown notifications\n\nDesign 1-3 focused tools that implement the description, each with a clear description and a strict inputSchema. If the description truly requires external packages or credentials, say so and stop instead of writing a broken server.\n\nWork economically — you have a limited step budget: write the COMPLETE server in a single write call, run one test pipeline, fix only what fails, then register.\n\nMANDATORY self-test before you finish: use bash to pipe a full session through the server —\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}' '{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}' '{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}' '{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"<tool>\",\"arguments\":{...}}}' | python3 {server_dir}/<name>.py\nand verify every response line is valid JSON with the right id and shape. The tools/call tests MUST include: (a) each tool called once with typical arguments, and (b) at least one call with ALL optional arguments omitted — every parameter your schema does not list in \"required\" must actually default in the implementation, not KeyError. Fix and re-test until it passes.\n\nThen register it: read the config file {config_path} if it exists (create it if not) and merge in {\"mcp\":{\"<name>\":{\"command\":\"python3\",\"args\":[\"{args_path}\"]}}} — use exactly that args path with <name> substituted — without losing any existing keys. After writing, READ {config_path} BACK and confirm the mcp entry is present — only report registration you have verified in the file, never from memory of having written it. Finish by telling the user: run /restart to load the server — {trust_note}; /mcp lists it afterwards.";

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
    /// The host/model came from a CLI flag or env var (not config), so a
    /// /restart must pin it again — config alone must not override it.
    pub host_pinned: bool,
    pub model_pinned: bool,
    pub providers: std::collections::HashMap<String, rift_core::ProviderConfig>,
    /// Config `pricing` map for the cost display (built-ins live in
    /// crate::pricing).
    pub pricing: std::collections::HashMap<String, rift_core::config::Pricing>,
    pub theme: theme::Theme,
}

/// How to relaunch after /restart: the exact session file to resume, plus
/// the host/model to pin via CLI flags — but ONLY when they were pinned at
/// launch (a --host/--model flag) or switched mid-session (/host, /model).
/// None = omit the flag so the relaunch re-reads config: "edit config, then
/// /restart" must pick up the edit instead of silently re-pinning the old
/// value (the flag outranks the file).
pub struct RestartSpec {
    pub model: Option<String>,
    pub host: Option<String>,
    pub session: std::path::PathBuf,
}

pub async fn run_tui(agent: Agent, opts: TuiOptions) -> Result<Option<RestartSpec>> {
    let TuiOptions { model, store, resumed, mcp, config_path, mut ask_rx, skills, host, host_pinned, model_pinned, providers, pricing, theme } = opts;
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
    // Captured for /restart before the store/host move into the agent task.
    // The host copy is UPDATED by UiEffect::Host on /host switches, so a
    // restart relaunches against the current server, not the startup one.
    let restart_session = store.path().to_path_buf();
    let mut restart_host = host.clone();
    // Startup values: a /restart re-pins host/model only when they differ
    // from these (mid-session /host, /model switch) or were CLI-pinned.
    let startup_host = host.clone();
    let startup_model = model.clone();
    // UI-side effect sender: for commands the UI handles itself (/copy log).
    let fx_ui = fx_tx.clone();
    // Background-task events must reach the UI outside any turn — give the
    // registry its own clone of the event channel before the agent moves.
    agent.ctx().bg().set_notify(ev_tx.clone());
    // UI-side ToolCtx handle: /btw side questions read the current provider
    // and model from it (kept fresh by run_turn) without touching the agent.
    let ui_ctx = agent.ctx().clone();
    // The addressable model name rides into the command context for /fork.
    let model_addr = model.clone();
    let agent_task = tokio::spawn(async move {
        let mut agent = agent;
        let mut cx =
            CmdCx { store, cwd: cwd.clone(), mcp, config_path, host, providers, model_addr };
        let cwd_str = cwd.display().to_string();
        // Seed the context gauge so a resumed session shows its fill level
        // before the first turn.
        let (used, limit) = agent.context_usage();
        let _ = ev_tx.send(AgentEvent::Context { used, limit });
        while let Some(msg) = prompt_rx.recv().await {
            match msg {
                UiMsg::Prompt(prompt, images, cancel) => {
                    if !images.is_empty() {
                        agent.attach_images(images);
                    }
                    if let Err(e) = agent.run_turn(&prompt, &ev_tx, &cancel).await {
                        let _ = ev_tx.send(AgentEvent::Warning(format!("error: {e:#}")));
                        let _ = ev_tx.send(AgentEvent::Done(TurnStats::default()));
                    }
                    if let Err(e) = cx.store.save(&agent.cfg.model, &cwd_str, &agent.messages) {
                        let _ = ev_tx.send(AgentEvent::Warning(format!("session save failed: {e:#}")));
                    }
                    // Compact during idle time (user is reading the reply),
                    // not mid-turn when they're waiting on the next answer.
                    agent.idle_compact(&ev_tx).await;
                }
                UiMsg::Command(line, cancel) => {
                    commands::run_command(&line, &mut agent, &mut cx, &fx_tx, &cancel).await;
                }
            }
            // Refresh the context gauge after every turn AND every command —
            // /clear, /compact, /rewind, /sessions and /model all move it.
            let (used, limit) = agent.context_usage();
            let _ = ev_tx.send(AgentEvent::Context { used, limit });
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
    app.push_logo();
    if resumed.is_empty() {
        app.push_getting_started();
    }
    app.seed_from_messages(&resumed);

    let result = (|| -> Result<()> {
        let mut needs_redraw = true;
        let mut last_tick = Instant::now();
        // A GUI editor we're waiting on (see UiEffect::EditFile), paired
        // with the command to dispatch when it closes cleanly. While Some,
        // the modal is up and input is frozen.
        let mut editing_child: Option<(std::process::Child, String)> = None;
        loop {
            // Watch a GUI editor: when its window closes, drop the modal and
            // dispatch the edit's reload command, exactly as the
            // terminal-editor path does.
            if let Some((child, after)) = editing_child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let after = after.clone();
                        editing_child = None;
                        app.editing_file = None;
                        if status.success() {
                            let _ = prompt_tx.send(UiMsg::Command(
                                after,
                                CancellationToken::new(),
                            ));
                        } else {
                            app.transcript.push_block(
                                Kind::Warn,
                                "! editor exited with an error; changes not reloaded".into(),
                            );
                        }
                        needs_redraw = true;
                    }
                    Err(e) => {
                        editing_child = None;
                        app.editing_file = None;
                        app.transcript
                            .push_block(Kind::Warn, format!("! lost track of the editor process: {e}"));
                        needs_redraw = true;
                    }
                    Ok(None) => {} // still open
                }
            }
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
                } else if let UiEffect::EditFile(path, after) = fx {
                    let editor = resolve_editor();
                    if editor_is_gui(&editor) {
                        // GUI editor: it opens in its own window, so keep the
                        // TUI up (dimmed behind a modal) and watch the process
                        // instead of blanking the terminal and blocking on it.
                        match spawn_gui_editor(&editor, &path) {
                            Ok(child) => {
                                let label =
                                    editor.split_whitespace().next().unwrap_or(&editor).to_string();
                                app.editing_file = Some((label, path.display().to_string()));
                                editing_child = Some((child, after));
                            }
                            Err(e) => app.transcript.push_block(
                                Kind::Warn,
                                format!("! could not launch editor '{editor}': {e}"),
                            ),
                        }
                    } else {
                        // Terminal editor (vim/nano/…): hand it the whole TTY,
                        // then take it back — it can't share the console.
                        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
                        ratatui::restore();
                        // Direct spawn first: std handles spaced paths (and
                        // .cmd/.bat on Windows) without shell-quoting pitfalls.
                        // Fall back to a shell only when that fails, e.g.
                        // EDITOR embeds flags like "vim -p".
                        let status =
                            std::process::Command::new(&editor).arg(&path).status().or_else(|_| {
                                #[cfg(windows)]
                                {
                                    let shell =
                                        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
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
                            });
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
                                    .send(UiMsg::Command(after, CancellationToken::new()));
                            }
                            _ => app.transcript.push_block(
                                Kind::Warn,
                                "! editor exited with an error; changes not reloaded".into(),
                            ),
                        }
                    }
                } else if let UiEffect::Host(url) = fx {
                    // /host switched servers: keep the restart target current.
                    restart_host = url;
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
                if !app.busy
                    && app.picker.is_none()
                    && app.answering.is_none()
                    && app.editing_file.is_none()
                    && app.notes_popup.is_none()
                {
                    // Background tasks finished while the agent was busy (or
                    // idle): feed their reports back as ONE notification turn
                    // so the model can react — the Claude Code-style flow.
                    if app.pending_auto.is_none() && !app.task_notes.is_empty() {
                        let notes = app.task_notes.drain(..).collect::<Vec<_>>().join("\n\n");
                        app.transcript.push_block(Kind::Info, "⚙ reporting finished background work to the model".into());
                        app.pending_auto = Some(format!(
                            "[task notification] Background work you started has finished:\n\n{notes}\n\n\
                             If this completes or affects the ongoing work, act on it now; otherwise give \
                             the user a one-line status."
                        ));
                        needs_redraw = true;
                    }
                    // Fire a due /loop round (queued like any auto turn).
                    if app.pending_auto.is_none() {
                        if let Some(ls) = app.loop_state.as_mut() {
                            if Instant::now() >= ls.next_at {
                                if ls.runs >= LOOP_MAX_RUNS {
                                    app.loop_state = None;
                                    app.transcript
                                        .push_block(Kind::Warn, format!("! loop stopped after {LOOP_MAX_RUNS} runs"));
                                } else {
                                    ls.runs += 1;
                                    let n = ls.runs;
                                    // Reschedule from fire time — a slow run
                                    // never causes a burst of catch-up rounds.
                                    ls.next_at = Instant::now() + ls.every.unwrap_or(Duration::ZERO);
                                    let body = ls.body.clone();
                                    app.transcript
                                        .push_block(Kind::Info, format!("↻ loop run {n}: {body}"));
                                    app.pending_auto = Some(body);
                                }
                                needs_redraw = true;
                            }
                        }
                    }
                    // Submit a queued goal/loop turn now that the agent is idle.
                    if let Some(text) = app.pending_auto.take() {
                        app.busy = true;
                        app.turn_started = Some(Instant::now());
                        app.transcript.scroll_from_bottom = 0;
                        let cancel = CancellationToken::new();
                        app.cancel = Some(cancel.clone());
                        if text.starts_with('/') {
                            app.status = "running command…".into();
                            let _ = prompt_tx.send(UiMsg::Command(text, cancel));
                        } else {
                            app.status = "auto turn — sending…".into();
                            let _ = prompt_tx.send(UiMsg::Prompt(text, vec![], cancel));
                        }
                        needs_redraw = true;
                    }
                }
                continue;
            }
            // Only state-changing events trigger a redraw — mouse capture
            // floods us with Moved events, and redrawing on each pegs a core.
            let input_event = event::read()?;
            // While a GUI editor holds the config open, the TUI is frozen
            // behind the modal — swallow input, only recentering on resize.
            if app.editing_file.is_some() {
                if matches!(input_event, Event::Resize(..)) {
                    needs_redraw = true;
                }
                continue;
            }
            match input_event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    needs_redraw = true;
                    let palette = app.palette();
                    let mention = if palette.is_empty() { app.mention_palette() } else { vec![] };
                    let popup_len = palette.len().max(mention.len());
                    // An open release-notes popup owns the keyboard: scroll or
                    // dismiss, nothing reaches the input line behind it.
                    if let Some(n) = app.notes_popup.as_mut() {
                        match key.code {
                            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                                app.notes_popup = None;
                                app.status = "release notes closed".into();
                            }
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                app.notes_popup = None;
                                app.status = "release notes closed".into();
                            }
                            KeyCode::Up | KeyCode::Char('k') => n.scroll = n.scroll.saturating_sub(1),
                            KeyCode::Down | KeyCode::Char('j') => n.scroll += 1,
                            KeyCode::PageUp => n.scroll = n.scroll.saturating_sub(10),
                            KeyCode::PageDown => n.scroll += 10,
                            KeyCode::Home | KeyCode::Char('g') => n.scroll = 0,
                            KeyCode::End | KeyCode::Char('G') => n.scroll = usize::MAX,
                            _ => {}
                        }
                        continue;
                    }
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
                                                // UI-side commands are applied right here — the
                                                // agent-task dispatcher doesn't know them and
                                                // would report "unknown command".
                                                if let Some(name) = line.strip_prefix("/theme ") {
                                                    app.apply_theme(name.trim());
                                                } else {
                                                    app.busy = true;
                                                    app.turn_started = Some(Instant::now());
                                                    app.status = "running command…".into();
                                                    let cancel = CancellationToken::new();
                                                    app.cancel = Some(cancel.clone());
                                                    let _ = prompt_tx.send(UiMsg::Command(line, cancel));
                                                }
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
                            app.status = "mouse capture on — drag selects text, wheel scrolls panes".into();
                        } else {
                            let _ = execute!(stdout(), DisableMouseCapture);
                            // The terminal can't see the TUI's selection, so
                            // drop it rather than leave a frozen highlight.
                            app.clear_selection();
                            app.status =
                                "mouse capture off — the terminal's own selection is back (it spans both panes) · Ctrl+T to return".into();
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
                        // A live selection is the most local thing Esc can
                        // dismiss — but never at the cost of interrupting a
                        // running turn, so it yields to the busy path below.
                        let cleared = app.clear_selection();
                        if cleared && popup_len == 0 && app.answering.is_none() && !app.busy {
                            app.status = "selection cleared".into();
                        } else if popup_len > 0 {
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
                        } else if app.goal.is_some() || app.loop_state.is_some() {
                            // Idle between auto turns: Esc stops the automation
                            // (during a turn, the cancel above reaches the same
                            // cleanup via the Done handler).
                            app.pending_auto = None;
                            if let Some(g) = app.goal.take() {
                                app.transcript
                                    .push_block(Kind::Info, format!("◎ goal cancelled — /goal {} resumes it", g.condition));
                            }
                            if app.loop_state.take().is_some() {
                                app.transcript.push_block(Kind::Info, "↻ loop stopped".into());
                            }
                            app.status = "goal/loop stopped".into();
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
                        } else if app.input.trim() == "/paste" {
                            // Clipboard image → staged attachment. Runs off
                            // the UI thread (the clipboard helper spawns a
                            // process); works while the agent is busy.
                            app.input.clear();
                            app.cursor = 0;
                            app.status = "reading clipboard…".into();
                            let fx2 = fx_ui.clone();
                            tokio::task::spawn_blocking(move || {
                                let fx = fx2;
                                match clipboard_image_data_url() {
                                    Ok((url, kb)) => {
                                        let _ = fx.send(UiEffect::Pasted(url, kb));
                                    }
                                    Err(e) => {
                                        let _ = fx.send(UiEffect::Out(Kind::Warn, format!("! /paste: {e:#}")));
                                        let _ = fx.send(UiEffect::Status("paste failed".into()));
                                    }
                                }
                            });
                        } else if app.input.trim() == "/btw" || app.input.trim().starts_with("/btw ") {
                            // Side question (Claude Code's /btw): sees the
                            // conversation, has no tools, never joins the main
                            // history — and is deliberately handled BEFORE the
                            // busy gate, so it works while the agent is working.
                            let raw = std::mem::take(&mut app.input);
                            app.cursor = 0;
                            app.history.push(raw.clone());
                            app.history_idx = None;
                            let arg = raw.trim().strip_prefix("/btw").map(str::trim).unwrap_or_default().to_string();
                            if arg.is_empty() {
                                app.transcript.push_block(
                                    Kind::Info,
                                    "usage: /btw <question> — quick side question: sees the conversation, no tools, \
                                     never enters the history, works while the agent is busy · /btw clear resets the side thread"
                                        .into(),
                                );
                            } else if matches!(arg.as_str(), "clear" | "x") {
                                let n = app.btw_exchanges.len();
                                app.btw_exchanges.clear();
                                app.transcript
                                    .push_block(Kind::Info, format!("(btw) side thread cleared ({n} exchange(s))"));
                            } else if app.btw_busy {
                                app.transcript
                                    .push_block(Kind::Warn, "! a side question is already pending — wait for its answer".into());
                            } else {
                                app.btw_busy = true;
                                app.transcript.push_block(Kind::Thinking, format!("(btw) you: {arg}"));
                                if !app.busy {
                                    app.status = "btw: asking…".into();
                                }
                                spawn_btw(
                                    fx_ui.clone(),
                                    ui_ctx.clone(),
                                    restart_session.clone(),
                                    app.btw_exchanges.clone(),
                                    arg,
                                );
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
                            if let Some(q) = trimmed.strip_prefix("/deep-research") {
                                // Canned research orchestration through the
                                // normal agent loop (the /init pattern).
                                let q = q.trim();
                                if q.is_empty() {
                                    app.idle();
                                    app.transcript.push_block(
                                        Kind::Warn,
                                        "! usage: /deep-research <question>".into(),
                                    );
                                } else {
                                    app.status = "deep research underway…".into();
                                    let _ = prompt_tx.send(UiMsg::Prompt(
                                        DEEP_RESEARCH_PROMPT.replace("{q}", q),
                                        vec![],
                                        cancel,
                                    ));
                                }
                            } else if trimmed == "/release-notes" {
                                // Pure UI: the changelog is embedded, no agent
                                // turn — open the closable popup and undo the
                                // busy flag set for a normal submission.
                                app.idle();
                                let (title, body) = crate::release_notes::latest();
                                app.notes_popup = Some(NotesPopup { title, body, scroll: 0 });
                                app.status = "release notes — Esc to close".into();
                            } else if trimmed == "/init" {
                                // Syntactic sugar: a canned prompt through the normal agent loop.
                                app.status = "generating RIFT.md…".into();
                                let _ = prompt_tx.send(UiMsg::Prompt(INIT_PROMPT.to_string(), vec![], cancel));
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
                                        let _ = prompt_tx.send(UiMsg::Prompt(prompt, vec![], cancel));
                                    }
                                    None => {
                                        app.idle();
                                        app.transcript.push_block(
                                            Kind::Warn,
                                            format!("! unknown skill '{name}' — /skills lists what's available"),
                                        );
                                    }
                                }
                            } else if let Some(rest) = trimmed.strip_prefix("/skills new") {
                                // Self-extension: the agent writes the skill
                                // file with its own tools (the /init pattern).
                                // --global targets ~/.config/rift/skills/
                                // (every project); default is .rift/skills/
                                // (this project only).
                                let (global, desc) = parse_scope_flag(rest);
                                if desc.is_empty() {
                                    app.idle();
                                    app.transcript.push_block(
                                        Kind::Warn,
                                        "! usage: /skills new [--global] <what the skill should do>".into(),
                                    );
                                } else {
                                    app.status = "generating skill…".into();
                                    let (dir, scope) = skill_target(global);
                                    let prompt = SKILL_NEW_PROMPT
                                        .replace("{dir}", &dir)
                                        .replace("{scope}", scope)
                                        .replace("{desc}", &desc);
                                    let _ = prompt_tx.send(UiMsg::Prompt(prompt, vec![], cancel));
                                }
                            } else if let Some(rest) = trimmed.strip_prefix("/mcp new") {
                                // Self-extension: the agent writes AND tests a
                                // stdio MCP server, then registers it. --global
                                // registers user-wide (absolute paths, loads
                                // everywhere, no trust prompt — user config is
                                // the user's own); default is the project
                                // .rift.json (relative paths, trust-gated).
                                let (global, desc) = parse_scope_flag(rest);
                                if desc.is_empty() {
                                    app.idle();
                                    app.transcript.push_block(
                                        Kind::Warn,
                                        "! usage: /mcp new [--global] <what the server's tools should do>".into(),
                                    );
                                } else {
                                    app.status = "generating MCP server…".into();
                                    let p = mcp_placement(global);
                                    let prompt = MCP_NEW_PROMPT
                                        .replace("{server_dir}", &p.server_dir)
                                        .replace("{config_path}", &p.config_path)
                                        .replace("{args_path}", &p.args_path)
                                        .replace("{trust_note}", p.trust_note)
                                        .replace("{desc}", &desc);
                                    let _ = prompt_tx.send(UiMsg::Prompt(prompt, vec![], cancel));
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
                                    // Browse with the picker (like /model);
                                    // Enter re-enters as "/theme <name>".
                                    app.picker = Some(PickerState {
                                        title: format!("select theme — current: {}", app.theme.name),
                                        items: theme::THEMES
                                            .iter()
                                            .map(|th| PickerItem {
                                                value: th.name.to_string(),
                                                label: th.name.to_string(),
                                                detail: if th.name == app.theme.name {
                                                    "current".into()
                                                } else if th.bg.is_some() {
                                                    "truecolor".into()
                                                } else {
                                                    "terminal-native".into()
                                                },
                                            })
                                            .collect(),
                                        selected: 0,
                                        kind: PickerKind::Command { template: "/theme {}".into() },
                                    });
                                    app.status =
                                        "themes — ↑↓ select, Enter switch, Esc cancel · persist with \"theme\" in config".into();
                                } else {
                                    app.apply_theme(arg);
                                }
                            } else if trimmed == "/goal" || trimmed.starts_with("/goal ") {
                                // Completion-condition mode: the turn starts now
                                // and auto-continues until the model verifies
                                // the goal (GOAL MET line) or a stop.
                                let arg = trimmed.strip_prefix("/goal").unwrap_or_default().trim().to_string();
                                match arg.as_str() {
                                    "" => {
                                        app.idle();
                                        match &app.goal {
                                            Some(g) => app.transcript.push_block(
                                                Kind::Info,
                                                format!(
                                                    "◎ goal active (turn {}/{GOAL_MAX_RUNS}): {}\nEsc or /goal clear stops it",
                                                    g.runs, g.condition
                                                ),
                                            ),
                                            None => app.transcript.push_block(
                                                Kind::Info,
                                                "no active goal — /goal <condition> keeps the agent working until the model verifies it's met".into(),
                                            ),
                                        }
                                    }
                                    "clear" | "stop" | "off" => {
                                        app.idle();
                                        app.pending_auto = None;
                                        match app.goal.take() {
                                            Some(g) => app
                                                .transcript
                                                .push_block(Kind::Info, format!("◎ goal cleared: {}", g.condition)),
                                            None => app.transcript.push_block(Kind::Info, "no active goal".into()),
                                        }
                                    }
                                    _ => {
                                        app.goal = Some(GoalState { condition: arg.clone(), runs: 1 });
                                        app.status = format!(
                                            "◎ goal set — auto-continues up to {GOAL_MAX_RUNS} turns · Esc or /goal clear stops"
                                        );
                                        let _ = prompt_tx.send(UiMsg::Prompt(goal_initial(&arg), vec![], cancel));
                                    }
                                }
                            } else if trimmed == "/loop" || trimmed.starts_with("/loop ") {
                                // Recurring prompt: /loop [30s|5m|2h] <prompt or /command>.
                                // No interval = back-to-back. Fires from the
                                // idle tick; /loop stop or Esc ends it.
                                app.idle();
                                let arg = trimmed.strip_prefix("/loop").unwrap_or_default().trim();
                                if arg.is_empty() {
                                    match &app.loop_state {
                                        Some(l) => app.transcript.push_block(
                                            Kind::Info,
                                            format!(
                                                "↻ loop active ({}, run {}/{LOOP_MAX_RUNS}): {}\nEsc or /loop stop ends it",
                                                l.every.map(|d| format!("every {}", fmt_interval(d)))
                                                    .unwrap_or_else(|| "back-to-back".into()),
                                                l.runs,
                                                l.body
                                            ),
                                        ),
                                        None => app.transcript.push_block(
                                            Kind::Info,
                                            "no active loop — usage: /loop [30s|5m|2h] <prompt or /command>".into(),
                                        ),
                                    }
                                } else if matches!(arg, "stop" | "clear" | "off") {
                                    app.pending_auto = None;
                                    match app.loop_state.take() {
                                        Some(l) => app
                                            .transcript
                                            .push_block(Kind::Info, format!("↻ loop stopped after {} run(s)", l.runs)),
                                        None => app.transcript.push_block(Kind::Info, "no active loop".into()),
                                    }
                                } else if parse_interval(arg).is_some() {
                                    // A bare interval with nothing to run.
                                    app.transcript.push_block(
                                        Kind::Warn,
                                        "! usage: /loop [30s|5m|2h] <prompt or /command>".into(),
                                    );
                                } else {
                                    let (every, body) = match arg.split_once(char::is_whitespace) {
                                        Some((first, rest)) if parse_interval(first).is_some() => {
                                            (parse_interval(first), rest.trim().to_string())
                                        }
                                        _ => (None, arg.to_string()),
                                    };
                                    app.loop_state = Some(LoopState {
                                        body,
                                        every,
                                        next_at: Instant::now(),
                                        runs: 0,
                                    });
                                    app.transcript.push_block(
                                        Kind::Info,
                                        format!(
                                            "↻ loop armed ({}) — first run starts now · Esc or /loop stop ends it",
                                            every
                                                .map(|d| format!("every {}", fmt_interval(d)))
                                                .unwrap_or_else(|| "back-to-back".into())
                                        ),
                                    );
                                }
                            } else if trimmed == "/quit" {
                                app.quit = true;
                            } else if trimmed == "/restart" {
                                // Relaunch the binary and resume this exact
                                // session — the post-/update path that keeps
                                // the chat. Sessions save after every turn, so
                                // nothing is lost. (A truly running turn never
                                // reaches this chain — submissions are blocked
                                // while busy — so no guard is needed; busy was
                                // just set for THIS submission by the shared
                                // path above.)
                                app.idle();
                                app.restart = true;
                                app.quit = true;
                            } else if trimmed == "/retry" {
                                if let Some(p) = app.last_prompt.clone() {
                                    app.status = "retrying…".into();
                                    app.transcript.push_block(
                                        Kind::Info,
                                        format!("↻ {}", p.chars().take(80).collect::<String>()),
                                    );
                                    let _ = prompt_tx.send(UiMsg::Prompt(p, vec![], cancel));
                                } else {
                                    app.idle();
                                    app.transcript.push_block(Kind::Warn, "! nothing to retry yet".into());
                                }
                            } else if trimmed == "/stats" {
                                app.idle();
                                let f = &app.session_stats.failures;
                                let mut out = format!(
                                    "session stats:\n  turns:         {}\n  model calls:   {}\n  tool calls:    {}\n  compactions:   {}\n  output tokens: {}\n  prompt tokens: {} (last-of-turn, summed)\n  model time:    {:.1}s\n  recoveries:    {}",
                                    app.session_stats.turns,
                                    app.session_stats.model_calls,
                                    app.session_stats.tool_calls,
                                    app.session_stats.compactions,
                                    app.session_stats.output_tokens,
                                    app.session_stats.prompt_tokens,
                                    app.session_stats.duration_ms as f64 / 1000.0,
                                    f.model_failures(),
                                );
                                // Metered providers: estimated $ at the current
                                // model's rates (billed input = per-call sums).
                                if let Some(rates) = crate::pricing::lookup(&app.model, &pricing) {
                                    let s = &app.session_stats;
                                    let usd = crate::pricing::cost(s.billed_prompt_tokens, s.output_tokens, rates);
                                    out.push_str(&format!(
                                        "\n  est. cost:     {} ({} billed in @ ${}/MTok, {} out @ ${}/MTok)",
                                        crate::pricing::format_cost(usd),
                                        s.billed_prompt_tokens,
                                        rates.0,
                                        s.output_tokens,
                                        rates.1,
                                    ));
                                }
                                if f.model_failures() > 0 {
                                    let detail: Vec<String> = [
                                        ("textual tool calls", f.textual_recoveries),
                                        ("tool aliases", f.alias_hits),
                                        ("doom loops", f.doom_loop_trips),
                                        ("unknown tools", f.unknown_tools),
                                        ("tool errors", f.tool_errors),
                                        ("apply nudges", f.apply_nudges),
                                        ("greedy retries", f.greedy_retries),
                                        ("truncations", f.truncations),
                                        ("template strips", f.template_strips),
                                        ("budget wrap-ups", f.wrap_ups),
                                    ]
                                    .iter()
                                    .filter(|(_, n)| *n > 0)
                                    .map(|(k, n)| format!("{k} {n}"))
                                    .collect();
                                    out.push_str(&format!(" ({})", detail.join(", ")));
                                }
                                app.transcript.push_block(Kind::Info, out);
                            } else if trimmed.starts_with('/') {
                                app.status = "running command…".into();
                                let _ = prompt_tx.send(UiMsg::Command(trimmed, cancel));
                            } else {
                                app.status = "sending…".into();
                                // Expand @-mentions: text files attach as
                                // outlines (structure without full-read
                                // tokens); images attach as base64 for
                                // vision-capable models.
                                let (expanded, notes, mut images) = expand_mentions(&raw, &app.cwd);
                                for n in notes {
                                    app.log.push_block(Kind::Info, format!("· {n}"));
                                }
                                // Staged /paste images ride along with this
                                // message.
                                if !app.pending_paste.is_empty() {
                                    app.log.push_block(
                                        Kind::Info,
                                        format!("· attaching {} pasted image(s)", app.pending_paste.len()),
                                    );
                                    images.append(&mut app.pending_paste);
                                }
                                app.last_prompt = Some(expanded.clone());
                                let _ = prompt_tx.send(UiMsg::Prompt(expanded, images, cancel));
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
                    // Wheel over an open palette/mention popup moves its
                    // selection instead of scrolling the pane beneath it —
                    // computed before the pane borrow.
                    let popup_len = {
                        let palette = app.palette();
                        if palette.is_empty() { app.mention_palette().len() } else { palette.len() }
                    };
                    match mouse.kind {
                        MouseEventKind::ScrollUp if popup_len > 0 => {
                            app.palette_idx = app.palette_idx.saturating_sub(1);
                            needs_redraw = true;
                        }
                        MouseEventKind::ScrollDown if popup_len > 0 => {
                            app.palette_idx = (app.palette_idx + 1).min(popup_len - 1);
                            needs_redraw = true;
                        }
                        kind @ (MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) => {
                            let pane = if app.log.contains(mouse.column, mouse.row) {
                                &mut app.log
                            } else if app.diff.contains(mouse.column, mouse.row) {
                                &mut app.diff
                            } else if app.transcript.contains(mouse.column, mouse.row) {
                                &mut app.transcript
                            } else {
                                app.focused_pane()
                            };
                            if kind == MouseEventKind::ScrollUp {
                                pane.scroll_up(3);
                            } else {
                                pane.scroll_down(3);
                            }
                            needs_redraw = true;
                        }
                        // Drag to select text inside a pane, copied on release
                        // — the panes sit side by side, so the terminal's own
                        // selection would splice both columns of every row
                        // together. Selecting one pane's text needs the TUI's
                        // help; Ctrl+T still hands selection back to the
                        // terminal for anyone who prefers it.
                        MouseEventKind::Down(MouseButton::Left) => {
                            app.clear_selection();
                            if let Some(id) = app.pane_at(mouse.column, mouse.row) {
                                // Clicking a pane focuses it, so the keyboard
                                // (PageUp, End) follows the mouse.
                                app.focus = match id {
                                    PaneId::Transcript => Focus::Transcript,
                                    PaneId::Log | PaneId::Diff => Focus::Log,
                                };
                                app.pane(id).select_from(mouse.column, mouse.row);
                                app.drag = Some(id);
                            }
                            needs_redraw = true;
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            if let Some(id) = app.drag {
                                let pane = app.pane(id);
                                // Dragging past an edge scrolls the pane, so a
                                // selection can run past one screenful.
                                // `.max(top)` keeps the clamp below well-formed
                                // if the layout collapsed the pane mid-drag
                                // (Ctrl+L, a resize) — an inverted range panics.
                                let top = pane.area.y;
                                let bottom = pane.area.bottom().saturating_sub(1).max(top);
                                if mouse.row < top {
                                    pane.scroll_up(1);
                                } else if mouse.row > bottom {
                                    pane.scroll_down(1);
                                }
                                pane.select_to(mouse.column, mouse.row.clamp(top, bottom));
                                needs_redraw = true;
                            }
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            if let Some(id) = app.drag.take() {
                                if let Some(text) = app.pane(id).selected_text() {
                                    let fx2 = fx_ui.clone();
                                    tokio::spawn(async move {
                                        let chars = text.chars().count();
                                        let msg = match crate::clipboard::copy_via_tool(&text).await {
                                            Some(tool) => format!("copied {chars} chars (via {tool}) — Esc clears the selection"),
                                            None => {
                                                let _ = fx2.send(UiEffect::Osc52(text));
                                                format!("sent {chars} chars to the terminal clipboard (OSC 52)")
                                            }
                                        };
                                        // Status only: a Done here would reset
                                        // the busy state of a turn running
                                        // while the user selects text.
                                        let _ = fx2.send(UiEffect::Status(msg));
                                    });
                                }
                                needs_redraw = true;
                            }
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
    result.map(|()| {
        app.restart.then(|| RestartSpec {
            model: (model_pinned || app.model != startup_model).then(|| app.model.clone()),
            host: (host_pinned || restart_host != startup_host).then_some(restart_host),
            session: restart_session,
        })
    })
}
