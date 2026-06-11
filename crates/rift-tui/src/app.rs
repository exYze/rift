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
use std::time::Duration;

use anyhow::Result;
use rift_core::{Agent, AgentEvent, SessionStore, TurnStats};
use rift_ollama::{Message, Role};

use crate::commands::{self, CmdCx, UiEffect};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
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

pub(crate) fn style_for(kind: Kind) -> Style {
    match kind {
        Kind::User => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        Kind::Assistant => Style::default(),
        Kind::Code => Style::default().fg(Color::Green),
        Kind::Thinking => Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        Kind::Tool => Style::default().fg(Color::White),
        Kind::ToolErr => Style::default().fg(Color::Red),
        Kind::Warn => Style::default().fg(Color::Yellow),
        Kind::Info => Style::default().fg(Color::DarkGray),
        Kind::DiffAdd => Style::default().fg(Color::Green),
        Kind::DiffDel => Style::default().fg(Color::Red),
        Kind::DiffHunk => Style::default().fg(Color::Cyan),
        Kind::DiffMeta => Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    }
}

struct BlockEntry {
    kind: Kind,
    text: String,
    /// Whether a blank separator line follows this block when rendered.
    gap: bool,
}

/// A scrollable region with its own content and bottom-anchored scroll state.
pub(crate) struct Pane {
    blocks: Vec<BlockEntry>,
    wrapped: Vec<(Kind, String)>,
    wrap_width: u16,
    pub(crate) dirty: bool,
    pub(crate) scroll_from_bottom: usize,
    pub(crate) view_height: usize,
    /// Last rendered screen area, for mouse-wheel routing.
    pub(crate) area: Rect,
}

impl Pane {
    pub(crate) fn new() -> Self {
        Self {
            blocks: vec![],
            wrapped: vec![],
            wrap_width: 0,
            dirty: true,
            scroll_from_bottom: 0,
            view_height: 0,
            area: Rect::default(),
        }
    }

    pub(crate) fn push_block(&mut self, kind: Kind, text: String) {
        self.blocks.push(BlockEntry { kind, text, gap: true });
        self.dirty = true;
    }

    /// Dense single line, no blank separator after it (tool logs, diffs).
    pub(crate) fn push_line(&mut self, kind: Kind, text: String) {
        self.blocks.push(BlockEntry { kind, text, gap: false });
        self.dirty = true;
    }

    pub(crate) fn append_stream(&mut self, kind: Kind, text: &str) {
        match self.blocks.last_mut() {
            Some(last) if last.kind == kind => last.text.push_str(text),
            _ => self.blocks.push(BlockEntry { kind, text: text.to_string(), gap: true }),
        }
        self.dirty = true;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub(crate) fn rebuild(&mut self, width: u16) {
        if !self.dirty && width == self.wrap_width {
            return;
        }
        let width_changed = width != self.wrap_width;
        let old_total = self.wrapped.len();
        self.wrap_width = width;
        self.dirty = false;
        let w = width.max(10) as usize;
        self.wrapped.clear();
        for block in &self.blocks {
            let prefixed = match block.kind {
                Kind::User => format!("❯ {}", block.text),
                _ => block.text.clone(),
            };
            // Fenced code blocks inside assistant text get their own style.
            let mut in_fence = false;
            for raw_line in prefixed.lines() {
                let mut kind = block.kind;
                if block.kind == Kind::Assistant {
                    if raw_line.trim_start().starts_with("```") {
                        in_fence = !in_fence;
                        kind = Kind::Code;
                    } else if in_fence {
                        kind = Kind::Code;
                    }
                }
                if raw_line.is_empty() {
                    self.wrapped.push((kind, String::new()));
                    continue;
                }
                if kind.hard_cut() {
                    // Don't re-wrap code/diffs: hard-cut so alignment stays exact.
                    let indent = if kind == Kind::Code { "  " } else { "" };
                    let mut rest = raw_line;
                    loop {
                        let cut = floor_boundary(rest, w.saturating_sub(indent.len()).max(1));
                        self.wrapped.push((kind, format!("{indent}{}", &rest[..cut])));
                        rest = &rest[cut..];
                        if rest.is_empty() {
                            break;
                        }
                    }
                } else {
                    for piece in textwrap::wrap(raw_line, w) {
                        self.wrapped.push((kind, piece.into_owned()));
                    }
                }
            }
            if block.gap {
                self.wrapped.push((block.kind, String::new()));
            }
        }
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

    pub(crate) fn visible_lines(&mut self) -> Vec<Line<'static>> {
        self.scroll_from_bottom = self.scroll_from_bottom.min(self.max_scroll());
        let total = self.wrapped.len();
        let end = total - self.scroll_from_bottom.min(total);
        let start = end.saturating_sub(self.view_height);
        self.wrapped[start..end]
            .iter()
            .map(|(kind, text)| Line::from(Span::styled(text.clone(), style_for(*kind))))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Transcript,
    Log,
}

struct App {
    model: String,
    transcript: Pane,
    log: Pane,
    show_log: bool,
    focus: Focus,
    input: String,
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
}

/// Indices into `commands::COMMANDS` whose names start with the input.
/// The palette is live while the user is typing the command word itself
/// (a space or newline means they've moved on to arguments).
fn palette_matches(input: &str) -> Vec<usize> {
    if !input.starts_with('/') || input.contains(char::is_whitespace) {
        return vec![];
    }
    commands::COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, (name, _, _))| name.starts_with(input))
        .map(|(i, _)| i)
        .collect()
}

impl App {
    fn new(model: String) -> Self {
        Self {
            model,
            transcript: Pane::new(),
            log: Pane::new(),
            show_log: true,
            focus: Focus::Transcript,
            input: String::new(),
            history: vec![],
            history_idx: None,
            busy: false,
            status: "Enter send · /help commands · Ctrl+J newline · Tab focus · Ctrl+L log · Esc cancel · Ctrl+C quit".into(),
            cancel: None,
            quit: false,
            palette_idx: 0,
            palette_off: false,
        }
    }

    /// Matches currently shown in the palette popup (empty = hidden).
    fn palette(&self) -> Vec<usize> {
        if self.palette_off {
            vec![]
        } else {
            palette_matches(&self.input)
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
            Focus::Log => &mut self.log,
        }
    }

    fn handle_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Iteration(i) => {
                self.log.push_block(Kind::Info, format!("· step {i}"));
                self.status = format!("step {i} — waiting for {}…", self.model);
            }
            AgentEvent::Thinking(t) => self.transcript.append_stream(Kind::Thinking, &t),
            AgentEvent::Content(c) => self.transcript.append_stream(Kind::Assistant, &c),
            AgentEvent::ToolStart { name, args } => {
                let args: String = args.chars().take(160).collect();
                self.log.push_block(Kind::Tool, format!("→ {name} {args}"));
                self.status = format!("running {name}…");
            }
            AgentEvent::ToolResult { name, ok, preview } => {
                self.log.push_block(
                    if ok { Kind::Tool } else { Kind::ToolErr },
                    format!("{} {name}: {preview}", if ok { '✓' } else { '✗' }),
                );
            }
            AgentEvent::Info(i) => {
                self.log.push_block(Kind::Info, format!("· {i}"));
            }
            AgentEvent::Warning(w) => {
                self.log.push_block(Kind::Warn, format!("! {w}"));
                self.transcript.push_block(Kind::Warn, format!("! {w}"));
            }
            AgentEvent::Done(stats) => {
                self.busy = false;
                self.cancel = None;
                self.status = format!(
                    "done — {} steps · {} prompt tok · {} out tok · {:.1} tok/s",
                    stats.iterations, stats.prompt_tokens, stats.output_tokens, stats.tokens_per_sec
                );
            }
        }
    }

    fn handle_ui_effect(&mut self, fx: UiEffect) {
        match fx {
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
                self.log = Pane::new();
            }
            UiEffect::Seed(messages) => {
                self.transcript = Pane::new();
                self.log = Pane::new();
                self.seed_from_messages(&messages);
            }
            UiEffect::Model(name) => self.model = name,
            UiEffect::Done(status) => {
                self.busy = false;
                self.cancel = None;
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

    let focused_style = Style::default().fg(Color::Cyan);
    let unfocused_style = Style::default().fg(Color::DarkGray);

    // Transcript pane.
    {
        let focused = app.focus == Focus::Transcript || log_area.is_none();
        let block = Block::bordered()
            .title(" Rift ")
            .border_style(if focused { focused_style } else { unfocused_style });
        let inner = block.inner(transcript_area);
        app.transcript.area = inner;
        app.transcript.rebuild(inner.width);
        app.transcript.view_height = inner.height as usize;
        let lines = app.transcript.visible_lines();
        frame.render_widget(block, transcript_area);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    // Tool-log pane.
    if let Some(log_area) = log_area {
        let focused = app.focus == Focus::Log;
        let block = Block::bordered()
            .title(" activity ")
            .border_style(if focused { focused_style } else { unfocused_style });
        let inner = block.inner(log_area);
        app.log.area = inner;
        app.log.rebuild(inner.width);
        app.log.view_height = inner.height as usize;
        let lines = app.log.visible_lines();
        frame.render_widget(block, log_area);
        frame.render_widget(Paragraph::new(lines), inner);
    } else {
        app.log.area = Rect::default();
    }

    // Status line.
    let pane = match app.focus {
        Focus::Transcript => &app.transcript,
        Focus::Log => &app.log,
    };
    let scroll_note = if pane.scroll_from_bottom > 0 {
        format!("  [↑{} — End to follow]", pane.scroll_from_bottom)
    } else {
        String::new()
    };
    let busy_marker = if app.busy { " ◐ " } else { " ● " };
    let status = Line::from(vec![
        Span::styled(format!(" {} ", app.model), Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::styled(busy_marker, Style::default().fg(if app.busy { Color::Yellow } else { Color::Green })),
        Span::styled(app.status.clone(), Style::default().fg(Color::DarkGray)),
        Span::styled(scroll_note, Style::default().fg(Color::Yellow)),
    ]);
    frame.render_widget(Paragraph::new(status), status_area);

    // Input pane.
    let input_title = if app.busy { " input (Esc cancels the running turn) " } else { " input " };
    let input_block = Block::bordered().title(input_title).border_style(unfocused_style);
    let input_inner = input_block.inner(input_area);
    frame.render_widget(input_block, input_area);
    let mut lines: Vec<Line> = Vec::new();
    let input_lines: Vec<&str> = if app.input.is_empty() { vec![""] } else { app.input.split('\n').collect() };
    let shown = input_lines.len().min(input_inner.height as usize).max(1);
    let start = input_lines.len() - shown;
    for (i, l) in input_lines[start..].iter().enumerate() {
        let prefix = if start + i == 0 { "❯ " } else { "… " };
        let is_last = start + i == input_lines.len() - 1;
        let mut spans = vec![Span::styled(prefix, Style::default().fg(Color::Cyan)), Span::raw((*l).to_string())];
        if is_last {
            spans.push(Span::styled("█", Style::default().fg(Color::DarkGray)));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), input_inner);

    // Slash-command palette: overlay anchored above the input pane, filtered
    // live by what's typed; Up/Down select, Tab completes, Enter runs.
    let palette = app.palette();
    if !palette.is_empty() {
        app.palette_idx = app.palette_idx.min(palette.len() - 1);
        let rows = palette.len().min(8);
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
        for (row, &ci) in palette.iter().enumerate().skip(start).take(rows) {
            let (name, args, desc) = commands::COMMANDS[ci];
            let selected = row == app.palette_idx;
            let left = if args.is_empty() { name.to_string() } else { format!("{name} {args}") };
            let base = if selected {
                Style::default().bg(Color::Cyan).fg(Color::Black)
            } else {
                Style::default()
            };
            let pad = (width as usize).saturating_sub(2 + 30 + desc.len());
            lines.push(Line::from(vec![
                Span::styled(format!(" {left:<29} "), base.add_modifier(Modifier::BOLD)),
                Span::styled(desc.to_string(), if selected { base } else { Style::default().fg(Color::DarkGray) }),
                Span::styled(" ".repeat(pad), base),
            ]));
        }
        let block = Block::bordered()
            .title(" commands — ↑↓ select · Tab complete · Enter run · Esc dismiss ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// The `/init` command body — a normal agent turn with a canned prompt.
const INIT_PROMPT: &str = "Explore this repository (use repo_map and outline to stay cheap; read key files only as needed) and write a RIFT.md file at the project root: a concise guide for AI coding agents working here. Cover: what the project does, how the code is laid out, how to build/test/run it, and any conventions or gotchas you noticed. Keep it under 60 lines.";

pub async fn run_tui(
    agent: Agent,
    model: String,
    store: SessionStore,
    resumed: Vec<Message>,
    mcp: Vec<(String, usize)>,
    config_path: Option<PathBuf>,
) -> Result<()> {
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
    let agent_task = tokio::spawn(async move {
        let mut agent = agent;
        let mut cx = CmdCx { store, cwd: cwd.clone(), mcp, config_path };
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
    let _ = execute!(stdout(), EnableMouseCapture);

    let mut app = App::new(model);
    app.seed_from_messages(&resumed);

    let result = (|| -> Result<()> {
        let mut needs_redraw = true;
        loop {
            while let Ok(ev) = ev_rx.try_recv() {
                app.handle_agent_event(ev);
                needs_redraw = true;
            }
            while let Ok(fx) = fx_rx.try_recv() {
                app.handle_ui_effect(fx);
                needs_redraw = true;
            }
            if needs_redraw {
                terminal.draw(|f| draw(f, &mut app))?;
                needs_redraw = false;
            }
            if app.quit {
                return Ok(());
            }

            if !event::poll(Duration::from_millis(16))? {
                continue;
            }
            needs_redraw = true;
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let palette = app.palette();
                    match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.quit = true;
                    }
                    KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.show_log = !app.show_log;
                        if !app.show_log {
                            app.focus = Focus::Transcript;
                        }
                    }
                    KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.input.push('\n');
                    }
                    KeyCode::Tab => {
                        if let Some(&ci) = palette.get(app.palette_idx.min(palette.len().saturating_sub(1))) {
                            // Complete to the selected command; trailing space
                            // if it takes arguments (which also hides the popup).
                            let (name, args, _) = commands::COMMANDS[ci];
                            app.input = if args.is_empty() { name.to_string() } else { format!("{name} ") };
                        } else {
                            app.focus = match app.focus {
                                Focus::Transcript if app.show_log => Focus::Log,
                                _ => Focus::Transcript,
                            };
                        }
                    }
                    KeyCode::Esc => {
                        if !palette.is_empty() {
                            app.palette_off = true;
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
                        app.input.push('\n');
                    }
                    KeyCode::Enter => {
                        if app.busy {
                            app.status = "agent is running — Esc to cancel first".into();
                        } else if !app.input.trim().is_empty() {
                            // With the palette open, Enter runs the selected
                            // command (completing any partial prefix first).
                            if let Some(&ci) = palette.get(app.palette_idx.min(palette.len().saturating_sub(1))) {
                                app.input = commands::COMMANDS[ci].0.to_string();
                            }
                            let raw = std::mem::take(&mut app.input);
                            app.history.push(raw.clone());
                            app.history_idx = None;
                            app.transcript.push_block(Kind::User, raw.clone());
                            app.busy = true;
                            app.transcript.scroll_from_bottom = 0;
                            app.log.scroll_from_bottom = 0;
                            let cancel = CancellationToken::new();
                            app.cancel = Some(cancel.clone());
                            let trimmed = raw.trim().to_string();
                            if trimmed == "/init" {
                                // Syntactic sugar: a canned prompt through the normal agent loop.
                                app.status = "generating RIFT.md…".into();
                                let _ = prompt_tx.send(UiMsg::Prompt(INIT_PROMPT.to_string(), cancel));
                            } else if trimmed.starts_with('/') {
                                app.status = "running command…".into();
                                let _ = prompt_tx.send(UiMsg::Command(trimmed, cancel));
                            } else {
                                app.status = "sending…".into();
                                let _ = prompt_tx.send(UiMsg::Prompt(raw, cancel));
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        app.input.pop();
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
                            app.palette_off = false;
                            app.palette_idx = 0;
                        }
                    }
                    KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                        match app.history_idx {
                            Some(i) if i + 1 < app.history.len() => {
                                app.history_idx = Some(i + 1);
                                app.input = app.history[i + 1].clone();
                            }
                            Some(_) => {
                                app.history_idx = None;
                                app.input.clear();
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
                        if palette.is_empty() {
                            app.focused_pane().scroll_up(1);
                        } else {
                            app.palette_idx = app.palette_idx.min(palette.len() - 1).saturating_sub(1);
                        }
                    }
                    KeyCode::Down => {
                        if palette.is_empty() {
                            app.focused_pane().scroll_down(1);
                        } else {
                            app.palette_idx = (app.palette_idx + 1).min(palette.len() - 1);
                        }
                    }
                    KeyCode::Home => {
                        let max = app.focused_pane().max_scroll();
                        app.focused_pane().scroll_from_bottom = max;
                    }
                    KeyCode::End => app.focused_pane().scroll_from_bottom = 0,
                    KeyCode::Char(c) => {
                        app.input.push(c);
                        app.palette_off = false;
                        app.palette_idx = 0;
                    }
                    _ => {}
                    }
                }
                Event::Mouse(mouse) => {
                    // Route the wheel to the pane under the cursor.
                    let pane = if app.log.contains(mouse.column, mouse.row) {
                        &mut app.log
                    } else if app.transcript.contains(mouse.column, mouse.row) {
                        &mut app.transcript
                    } else {
                        app.focused_pane()
                    };
                    match mouse.kind {
                        MouseEventKind::ScrollUp => pane.scroll_up(3),
                        MouseEventKind::ScrollDown => pane.scroll_down(3),
                        _ => {}
                    }
                }
                Event::Resize(_, _) => {
                    app.transcript.dirty = true;
                    app.log.dirty = true;
                }
                _ => {}
            }
        }
    })();

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    agent_task.abort();
    result
}
