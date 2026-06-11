//! WarpDrive swarm TUI: live candidate tabs, per-candidate activity log,
//! colored diff pane, one-key merge of the winner.
//!
//! Keys: ←/→ (or 1-9) select candidate · Tab focus log/diff · m merge
//! selected · Esc cancel race · q/Ctrl+C quit. Worktrees are left in place
//! on exit so nothing is ever lost silently.

use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rift_core::{run_swarm, AgentConfig, AgentEvent, Candidate, CandidateOutcome, Swarm, TurnStats};
use rift_ollama::OllamaClient;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::app::{Kind, Pane};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Done,
    Failed,
}

struct CandView {
    name: String,
    model: String,
    status: Status,
    log: Pane,
    diff: Pane,
    has_patch: bool,
    merged: bool,
    stats: Option<TurnStats>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Focus {
    Log,
    Diff,
}

struct SwarmApp {
    cands: Vec<CandView>,
    selected: usize,
    focus: Focus,
    race_done: bool,
    message: String,
    quit: bool,
}

impl SwarmApp {
    fn handle_event(&mut self, idx: usize, ev: AgentEvent) {
        let Some(c) = self.cands.get_mut(idx) else { return };
        match ev {
            AgentEvent::Iteration(i) => c.log.push_line(Kind::Info, format!("· step {i}")),
            AgentEvent::Thinking(t) => c.log.append_stream(Kind::Thinking, &t),
            AgentEvent::Content(t) => c.log.append_stream(Kind::Assistant, &t),
            AgentEvent::ToolStart { name, args } => {
                let args: String = args.chars().take(120).collect();
                c.log.push_line(Kind::Tool, format!("→ {name} {args}"));
            }
            AgentEvent::ToolResult { name, ok, preview } => {
                let preview: String = preview.chars().take(120).collect();
                c.log.push_line(
                    if ok { Kind::Tool } else { Kind::ToolErr },
                    format!("{} {name}: {preview}", if ok { '✓' } else { '✗' }),
                );
            }
            AgentEvent::Info(i) => c.log.push_line(Kind::Info, format!("· {i}")),
            AgentEvent::Warning(w) => c.log.push_line(Kind::Warn, format!("! {w}")),
            AgentEvent::Done(stats) => {
                c.status = Status::Done;
                c.stats = Some(stats);
                c.log.push_line(Kind::Info, "· finished — capturing diff…".into());
            }
        }
    }

    fn load_outcomes(&mut self, outcomes: &[CandidateOutcome]) {
        for (i, o) in outcomes.iter().enumerate() {
            let Some(c) = self.cands.get_mut(i) else { continue };
            c.status = if o.error.is_some() { Status::Failed } else { Status::Done };
            c.has_patch = o.patch_path.is_some();
            if let Some(err) = &o.error {
                c.log.push_block(Kind::Warn, format!("! {err}"));
            }
            if !o.summary.is_empty() {
                c.log.push_block(Kind::Assistant, o.summary.clone());
            }
            if o.patch_path.is_some() {
                // Show the stat header, then the patch body with diff colors.
                for line in o.diff_stat.lines() {
                    c.diff.push_line(Kind::DiffMeta, line.to_string());
                }
                c.diff.push_line(Kind::Info, String::new());
                if let Some(p) = &o.patch_path {
                    if let Ok(patch) = std::fs::read_to_string(p) {
                        for line in patch.lines() {
                            let kind = diff_kind(line);
                            c.diff.push_line(kind, line.to_string());
                        }
                    }
                }
                // Diffs read top-down: start scrolled to the top.
                c.diff.scroll_from_bottom = usize::MAX;
            } else {
                c.diff.push_line(Kind::Info, o.diff_stat.clone());
            }
        }
        self.race_done = true;
        self.message = "race finished — m merges the selected candidate".into();
    }
}

fn diff_kind(line: &str) -> Kind {
    if line.starts_with("diff ") || line.starts_with("index ") || line.starts_with("+++") || line.starts_with("---") || line.starts_with("new file") || line.starts_with("deleted file") {
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

fn draw(frame: &mut Frame, app: &mut SwarmApp, task: &str) {
    let [tabs_area, main_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Tab bar.
    let mut spans: Vec<Span> = vec![Span::styled(" WarpDrive ", Style::default().fg(Color::Black).bg(Color::Magenta))];
    for (i, c) in app.cands.iter().enumerate() {
        let icon = match (c.status, c.merged) {
            (_, true) => "⇣",
            (Status::Running, _) => "◐",
            (Status::Done, _) => "✓",
            (Status::Failed, _) => "✗",
        };
        let label = format!("  {} {} {}  ", i + 1, c.name, icon);
        let style = if i == app.selected {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), tabs_area);

    // Main: activity log | diff.
    let [log_area, diff_area] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(main_area);
    let focused = Style::default().fg(Color::Cyan);
    let unfocused = Style::default().fg(Color::DarkGray);
    let c = &mut app.cands[app.selected];

    let log_block = Block::bordered()
        .title(format!(" activity — {} ", c.model))
        .border_style(if app.focus == Focus::Log { focused } else { unfocused });
    let inner = log_block.inner(log_area);
    c.log.area = inner;
    c.log.rebuild(inner.width);
    c.log.view_height = inner.height as usize;
    let lines = c.log.visible_lines();
    frame.render_widget(log_block, log_area);
    frame.render_widget(Paragraph::new(lines), inner);

    let diff_title = if c.merged {
        " diff (merged ⇣) "
    } else if c.has_patch {
        " diff — m to merge "
    } else {
        " diff "
    };
    let diff_block = Block::bordered()
        .title(diff_title)
        .border_style(if app.focus == Focus::Diff { focused } else { unfocused });
    let inner = diff_block.inner(diff_area);
    c.diff.area = inner;
    c.diff.rebuild(inner.width);
    c.diff.view_height = inner.height as usize;
    let lines = if c.diff.is_empty() {
        vec![Line::from(Span::styled(
            if c.status == Status::Running { "…running…" } else { "(no diff)" },
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        c.diff.visible_lines()
    };
    frame.render_widget(diff_block, diff_area);
    frame.render_widget(Paragraph::new(lines), inner);

    // Status line.
    let task_short: String = task.chars().take(60).collect();
    let status = Line::from(vec![
        Span::styled(format!(" {task_short} "), Style::default().fg(Color::DarkGray)),
        Span::styled(
            " ←/→ candidate · Tab focus · ↑/↓ scroll · m merge · Esc cancel · q quit ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(app.message.clone(), Style::default().fg(Color::Yellow)),
    ]);
    frame.render_widget(Paragraph::new(status), status_area);
}

pub async fn run_swarm_tui(
    client: OllamaClient,
    cfg: AgentConfig,
    swarm: Swarm,
    candidates: Vec<Candidate>,
    task: String,
) -> Result<()> {
    let swarm = Arc::new(swarm);
    let (tx, mut rx) = mpsc::unbounded_channel::<(usize, AgentEvent)>();
    let cancel = CancellationToken::new();

    let mut app = SwarmApp {
        cands: candidates
            .iter()
            .map(|c| CandView {
                name: c.name.clone(),
                model: c.model.clone(),
                status: Status::Running,
                log: Pane::new(),
                diff: Pane::new(),
                has_patch: false,
                merged: false,
                stats: None,
            })
            .collect(),
        selected: 0,
        focus: Focus::Diff,
        race_done: false,
        message: String::new(),
        quit: false,
    };

    let race = {
        let client = client.clone();
        let swarm = swarm.clone();
        let cancel = cancel.clone();
        let task = task.clone();
        tokio::spawn(async move { run_swarm(&client, &cfg, &swarm, candidates, &task, tx, &cancel).await })
    };
    let mut race_handle = Some(race);

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);

    let result: Result<()> = loop {
        // Drain agent events.
        while let Ok((idx, ev)) = rx.try_recv() {
            app.handle_event(idx, ev);
        }
        // Race finished? Load outcomes exactly once.
        if let Some(handle) = &race_handle {
            if handle.is_finished() {
                let handle = race_handle.take().unwrap();
                match handle.await {
                    Ok(outcomes) => app.load_outcomes(&outcomes),
                    Err(e) => app.message = format!("race task failed: {e}"),
                }
            }
        }

        terminal.draw(|f| draw(f, &mut app, &task))?;
        if app.quit {
            break Ok(());
        }

        if !event::poll(Duration::from_millis(33))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') => app.quit = true,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    cancel.cancel();
                    app.quit = true;
                }
                KeyCode::Esc => {
                    if !app.race_done {
                        cancel.cancel();
                        app.message = "cancelling all candidates…".into();
                    }
                }
                KeyCode::Left => app.selected = app.selected.saturating_sub(1),
                KeyCode::Right => app.selected = (app.selected + 1).min(app.cands.len() - 1),
                KeyCode::Char(d @ '1'..='9') => {
                    let i = (d as usize - '1' as usize).min(app.cands.len() - 1);
                    app.selected = i;
                }
                KeyCode::Tab => {
                    app.focus = if app.focus == Focus::Log { Focus::Diff } else { Focus::Log };
                }
                KeyCode::Char('m') => {
                    let c = &mut app.cands[app.selected];
                    if !app.race_done {
                        app.message = "wait for the race to finish".into();
                    } else if c.merged {
                        app.message = format!("{} already merged", c.name);
                    } else if !c.has_patch {
                        app.message = format!("{} has no changes to merge", c.name);
                    } else {
                        match swarm.apply_patch(&c.name).await {
                            Ok(()) => {
                                c.merged = true;
                                app.message = format!("merged {} into your working tree ⇣", c.name);
                            }
                            Err(e) => app.message = format!("merge failed: {e:#}"),
                        }
                    }
                }
                KeyCode::PageUp | KeyCode::PageDown | KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End => {
                    let c = &mut app.cands[app.selected];
                    let pane = if app.focus == Focus::Log { &mut c.log } else { &mut c.diff };
                    let page = pane.view_height.saturating_sub(1).max(1);
                    match key.code {
                        KeyCode::PageUp => pane.scroll_up(page),
                        KeyCode::PageDown => pane.scroll_down(page),
                        KeyCode::Up => pane.scroll_up(1),
                        KeyCode::Down => pane.scroll_down(1),
                        KeyCode::Home => pane.scroll_from_bottom = pane.max_scroll(),
                        KeyCode::End => pane.scroll_from_bottom = 0,
                        _ => {}
                    }
                }
                _ => {}
            },
            Event::Mouse(mouse) => {
                let c = &mut app.cands[app.selected];
                let over_log = c.log.contains(mouse.column, mouse.row);
                let over_diff = c.diff.contains(mouse.column, mouse.row);
                let pane = if over_diff || (!over_log && app.focus == Focus::Diff) {
                    &mut c.diff
                } else {
                    &mut c.log
                };
                match mouse.kind {
                    MouseEventKind::ScrollUp => pane.scroll_up(3),
                    MouseEventKind::ScrollDown => pane.scroll_down(3),
                    _ => {}
                }
            }
            Event::Resize(_, _) => {
                for c in &mut app.cands {
                    c.log.dirty = true;
                    c.diff.dirty = true;
                }
            }
            _ => {}
        }
    };

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    cancel.cancel();
    if let Some(handle) = race_handle {
        handle.abort();
    }

    // Post-exit summary on the plain terminal.
    println!("WarpDrive: worktrees kept under .rift/worktrees/ — `rift merge <name> [--cleanup]` still works.");
    for c in &app.cands {
        let state = if c.merged {
            "merged"
        } else if c.has_patch {
            "patch saved"
        } else {
            "no changes"
        };
        let stats = c
            .stats
            .as_ref()
            .map(|s| format!(" ({} steps, {} out tok)", s.iterations, s.output_tokens))
            .unwrap_or_default();
        println!("  {} [{}] — {state}{stats}", c.name, c.model);
    }
    result
}
