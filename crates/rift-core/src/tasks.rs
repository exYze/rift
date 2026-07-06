//! Background task registry: shell commands and sub-agents that keep running
//! while the user (and the model) continue the conversation. One registry per
//! session, shared through ToolCtx; the frontend subscribes via `set_notify`
//! so TaskStarted/TaskFinished events surface even between turns.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use anyhow::{bail, Result};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;

/// Concurrent-running cap — a runaway model can't fork-bomb the machine.
pub const BG_MAX_RUNNING: usize = 8;
/// Per-task output buffer cap; the FRONT is dropped once exceeded — the tail
/// is where a build/test run's verdict lives.
const BG_OUTPUT_CAP: usize = 262_144;
/// Output/report preview length carried in the TaskFinished notification.
const FINISH_PREVIEW: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Agent,
}

impl TaskKind {
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::Shell => "shell",
            TaskKind::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    /// Finished on its own: the exit code for shells (None = unknown),
    /// 0/1 for ok/failed sub-agents.
    Done(Option<i32>),
    /// Terminated via the task tool or /tasks kill — kills are silent
    /// (no TaskFinished notification; the killer already knows).
    Killed,
}

impl TaskStatus {
    pub fn describe(&self) -> String {
        match self {
            TaskStatus::Running => "running".into(),
            TaskStatus::Done(Some(0)) => "done".into(),
            TaskStatus::Done(Some(c)) => format!("failed (exit {c})"),
            TaskStatus::Done(None) => "done (exit unknown)".into(),
            TaskStatus::Killed => "killed".into(),
        }
    }
}

struct BgTask {
    id: u64,
    kind: TaskKind,
    label: String,
    started: Instant,
    status: TaskStatus,
    output: String,
    pid: Option<u32>,
    cancel: CancellationToken,
}

/// A cloneable listing row (the lock never leaves this module).
#[derive(Debug, Clone)]
pub struct TaskView {
    pub id: u64,
    pub kind: TaskKind,
    pub label: String,
    pub status: TaskStatus,
    pub elapsed_secs: u64,
    pub output_bytes: usize,
}

#[derive(Clone, Default)]
pub struct BgTasks {
    inner: Arc<Mutex<Vec<BgTask>>>,
    next_id: Arc<AtomicU64>,
    notify: Arc<RwLock<Option<UnboundedSender<AgentEvent>>>>,
}

impl BgTasks {
    /// Attach the frontend's event channel. Task events flow through it for
    /// the whole session, independent of any turn — that's what lets a
    /// completion surface while the user is typing.
    pub fn set_notify(&self, tx: UnboundedSender<AgentEvent>) {
        if let Ok(mut n) = self.notify.write() {
            *n = Some(tx);
        }
    }

    /// Send an event to the attached frontend (silently dropped when none).
    pub fn emit(&self, ev: AgentEvent) {
        if let Ok(n) = self.notify.read() {
            if let Some(tx) = n.as_ref() {
                let _ = tx.send(ev);
            }
        }
    }

    pub fn running_count(&self) -> usize {
        self.inner
            .lock()
            .map(|t| t.iter().filter(|t| t.status == TaskStatus::Running).count())
            .unwrap_or(0)
    }

    /// Register a new running task. Fails when the concurrent-running cap is
    /// reached — the model is told to wait for or kill an existing task.
    pub fn register(&self, kind: TaskKind, label: &str, pid: Option<u32>) -> Result<(u64, CancellationToken)> {
        if self.running_count() >= BG_MAX_RUNNING {
            bail!(
                "background task limit reached ({BG_MAX_RUNNING} running). Wait for one to finish \
                 or kill one with the task tool before starting another."
            );
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let cancel = CancellationToken::new();
        if let Ok(mut tasks) = self.inner.lock() {
            tasks.push(BgTask {
                id,
                kind,
                label: label.to_string(),
                started: Instant::now(),
                status: TaskStatus::Running,
                output: String::new(),
                pid,
                cancel: cancel.clone(),
            });
        }
        self.emit(AgentEvent::TaskStarted { id, label: label.to_string() });
        Ok((id, cancel))
    }

    pub fn append_output(&self, id: u64, chunk: &str) {
        if let Ok(mut tasks) = self.inner.lock() {
            if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                t.output.push_str(chunk);
                if t.output.len() > BG_OUTPUT_CAP {
                    // Drop the front on a char boundary; keep the tail.
                    let cut = t.output.len() - BG_OUTPUT_CAP;
                    let mut i = cut;
                    while i < t.output.len() && !t.output.is_char_boundary(i) {
                        i += 1;
                    }
                    t.output.replace_range(..i, "[…output trimmed…]\n");
                }
            }
        }
    }

    /// Mark a task finished on its own and notify the frontend. A task the
    /// user/model already killed stays Killed and stays silent.
    pub fn finish(&self, id: u64, exit: Option<i32>) {
        let ev = {
            let Ok(mut tasks) = self.inner.lock() else { return };
            let Some(t) = tasks.iter_mut().find(|t| t.id == id) else { return };
            if t.status != TaskStatus::Running {
                return;
            }
            t.status = TaskStatus::Done(exit);
            // The preview feeds straight into a model-facing notification —
            // strip terminal escapes (cargo/npm colors) before it travels.
            let clean = crate::tools::strip_ansi(&t.output);
            let tail = clean.trim();
            let start = floor_char_boundary(tail, tail.len().saturating_sub(FINISH_PREVIEW));
            AgentEvent::TaskFinished {
                id,
                label: t.label.clone(),
                ok: matches!(exit, Some(0)),
                preview: tail[ceil_char_boundary(tail, start)..].to_string(),
            }
        };
        self.emit(ev);
    }

    /// Kill a running task: cancel its token (background agents stop at the
    /// next await point) and terminate its process tree (shells).
    pub fn kill(&self, id: u64) -> Result<TaskView> {
        let (view, pid, cancel) = {
            let Ok(mut tasks) = self.inner.lock() else { bail!("task registry lock poisoned") };
            let Some(t) = tasks.iter_mut().find(|t| t.id == id) else {
                bail!("no background task #{id} — the task tool with no arguments lists them");
            };
            if t.status != TaskStatus::Running {
                bail!("task #{id} is not running (status: {})", t.status.describe());
            }
            t.status = TaskStatus::Killed;
            (view_of(t), t.pid, t.cancel.clone())
        };
        cancel.cancel();
        if let Some(pid) = pid {
            kill_tree(pid);
        }
        Ok(view)
    }

    pub fn list(&self) -> Vec<TaskView> {
        self.inner.lock().map(|tasks| tasks.iter().map(view_of).collect()).unwrap_or_default()
    }

    /// Status row + full accumulated output of one task.
    pub fn output_of(&self, id: u64) -> Option<(TaskView, String)> {
        self.inner
            .lock()
            .ok()?
            .iter()
            .find(|t| t.id == id)
            .map(|t| (view_of(t), t.output.clone()))
    }
}

fn view_of(t: &BgTask) -> TaskView {
    TaskView {
        id: t.id,
        kind: t.kind,
        label: t.label.clone(),
        status: t.status,
        elapsed_secs: t.started.elapsed().as_secs(),
        output_bytes: t.output.len(),
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Terminate a whole process tree (shell → npm → node …), fire-and-forget.
/// Mirrors the bash tool's timeout kill; here it backs the task tool's kill.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -9 -{pid} 2>/dev/null || kill -9 {pid} 2>/dev/null"))
            .spawn();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_register_append_finish() {
        let reg = BgTasks::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.set_notify(tx);

        let (id, _cancel) = reg.register(TaskKind::Shell, "cargo build", Some(1234)).unwrap();
        assert_eq!(reg.running_count(), 1);
        matches!(rx.try_recv().unwrap(), AgentEvent::TaskStarted { .. });

        reg.append_output(id, "compiling…\n");
        reg.append_output(id, "finished\n");
        reg.finish(id, Some(0));
        assert_eq!(reg.running_count(), 0);
        match rx.try_recv().unwrap() {
            AgentEvent::TaskFinished { id: fid, ok, preview, .. } => {
                assert_eq!(fid, id);
                assert!(ok);
                assert!(preview.contains("finished"));
            }
            other => panic!("expected TaskFinished, got {other:?}"),
        }

        let (view, out) = reg.output_of(id).unwrap();
        assert_eq!(view.status, TaskStatus::Done(Some(0)));
        assert!(out.contains("compiling"));
        // Double-finish is a no-op (no second notification).
        reg.finish(id, Some(1));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn kill_is_silent_and_finish_keeps_killed() {
        let reg = BgTasks::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.set_notify(tx);
        let (id, cancel) = reg.register(TaskKind::Agent, "audit the tests", None).unwrap();
        let _ = rx.try_recv(); // TaskStarted
        let view = reg.kill(id).unwrap();
        assert_eq!(view.status, TaskStatus::Killed);
        assert!(cancel.is_cancelled());
        // The monitor calling finish() afterwards must not resurrect or notify.
        reg.finish(id, Some(0));
        assert!(rx.try_recv().is_err());
        assert_eq!(reg.output_of(id).unwrap().0.status, TaskStatus::Killed);
        // Killing again fails cleanly.
        assert!(reg.kill(id).is_err());
    }

    #[test]
    fn running_cap_enforced() {
        let reg = BgTasks::default();
        for i in 0..BG_MAX_RUNNING {
            reg.register(TaskKind::Shell, &format!("job {i}"), None).unwrap();
        }
        assert!(reg.register(TaskKind::Shell, "one too many", None).is_err());
        // Finishing one frees a slot.
        reg.finish(1, Some(0));
        assert!(reg.register(TaskKind::Shell, "fits again", None).is_ok());
    }

    #[test]
    fn output_cap_keeps_tail() {
        let reg = BgTasks::default();
        let (id, _) = reg.register(TaskKind::Shell, "spammy", None).unwrap();
        let chunk = "x".repeat(100_000);
        for _ in 0..4 {
            reg.append_output(id, &chunk);
        }
        reg.append_output(id, "THE END");
        let (_, out) = reg.output_of(id).unwrap();
        assert!(out.len() <= BG_OUTPUT_CAP + 64);
        assert!(out.ends_with("THE END"));
    }
}
