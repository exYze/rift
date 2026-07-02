//! WarpDrive: parallel agent exploration in isolated git worktrees.
//!
//! Each candidate (model + temperature) gets a detached worktree under
//! `<repo>/.rift/worktrees/<name>` — the user's working tree is never touched.
//! Every candidate's changes are captured as a patch in `.rift/patches/`, ready
//! to apply with one command. `.rift/` is kept out of git status via
//! `.git/info/exclude` (repo-local, nothing committed).

use std::path::{Path, PathBuf};

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rift_provider::Provider;
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentConfig, AgentEvent, TurnStats};
use crate::tools::{ToolCtx, ToolRegistry};

async fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).current_dir(dir).output().await?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub struct Swarm {
    root: PathBuf,
}

impl Swarm {
    /// Locate the enclosing git repo and verify it has a HEAD commit
    /// (worktrees are created detached at HEAD).
    pub async fn discover(cwd: &Path) -> Result<Self> {
        let Ok(top) = git_in(cwd, &["rev-parse", "--show-toplevel"]).await else {
            bail!("swarm mode needs a git repository (run `git init` and commit first)")
        };
        let root = PathBuf::from(top.trim());
        if git_in(&root, &["rev-parse", "--verify", "HEAD"]).await.is_err() {
            bail!("repository has no commits yet; make an initial commit first");
        }
        let swarm = Self { root };
        swarm.ensure_excluded().await?;
        Ok(swarm)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Names of worktree directories currently under `.rift/worktrees/`.
    pub fn list_worktrees(&self) -> Vec<String> {
        let mut out = vec![];
        if let Ok(entries) = std::fs::read_dir(self.root.join(".rift/worktrees")) {
            for e in entries.flatten() {
                if e.path().is_dir() {
                    if let Some(n) = e.file_name().to_str() {
                        out.push(n.to_string());
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Captured patches as `(candidate name, path)` under `.rift/patches/`.
    pub fn list_patches(&self) -> Vec<(String, PathBuf)> {
        let mut out = vec![];
        if let Ok(entries) = std::fs::read_dir(self.root.join(".rift/patches")) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("patch") {
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        out.push((stem.to_string(), p));
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Keep swarm scratch space out of `git status` without touching the
    /// repo's .gitignore. Only worktrees/ and patches/ — the rest of `.rift/`
    /// (skills, prompts) is meant to be committed.
    async fn ensure_excluded(&self) -> Result<()> {
        let git_dir = git_in(&self.root, &["rev-parse", "--git-common-dir"]).await?;
        let exclude = self.root.join(git_dir.trim()).join("info/exclude");
        let current = tokio::fs::read_to_string(&exclude).await.unwrap_or_default();
        // Drop the old blanket `.rift/` exclusion from earlier versions.
        let mut lines: Vec<&str> = current.lines().filter(|l| l.trim() != ".rift/").collect();
        let mut changed = lines.len() != current.lines().count();
        for needed in [".rift/worktrees/", ".rift/patches/"] {
            if !lines.iter().any(|l| l.trim() == needed) {
                lines.push(needed);
                changed = true;
            }
        }
        if changed {
            if let Some(parent) = exclude.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&exclude, format!("{}\n", lines.join("\n"))).await?;
        }
        Ok(())
    }

    pub async fn create_worktree(&self, name: &str) -> Result<PathBuf> {
        let path = self.root.join(".rift/worktrees").join(name);
        if path.exists() {
            // stale leftovers from a previous run
            let _ = git_in(&self.root, &["worktree", "remove", "--force", &path.display().to_string()]).await;
            let _ = tokio::fs::remove_dir_all(&path).await;
            let _ = git_in(&self.root, &["worktree", "prune"]).await;
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        git_in(
            &self.root,
            &["worktree", "add", "--detach", &path.display().to_string(), "HEAD"],
        )
        .await
        .context("creating worktree")?;
        Ok(path)
    }

    /// Full patch of everything the candidate changed (stages first so new
    /// files are included).
    pub async fn capture_diff(&self, worktree: &Path) -> Result<(String, String)> {
        git_in(worktree, &["add", "-A"]).await?;
        let patch = git_in(worktree, &["diff", "--cached", "--binary"]).await?;
        let stat = git_in(worktree, &["diff", "--cached", "--stat"]).await?;
        Ok((patch, stat))
    }

    pub async fn save_patch(&self, name: &str, patch: &str) -> Result<PathBuf> {
        let dir = self.root.join(".rift/patches");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{name}.patch"));
        tokio::fs::write(&path, patch).await?;
        Ok(path)
    }

    /// Apply a saved candidate patch to the user's working tree.
    pub async fn apply_patch(&self, name: &str) -> Result<()> {
        let path = self.root.join(".rift/patches").join(format!("{name}.patch"));
        if !path.exists() {
            bail!("no patch named '{name}' — expected {}", path.display());
        }
        git_in(&self.root, &["apply", "--3way", &path.display().to_string()])
            .await
            .context("applying patch (working tree may have conflicting local changes)")?;
        Ok(())
    }

    pub async fn remove_worktree(&self, worktree: &Path) -> Result<()> {
        let _ = git_in(&self.root, &["worktree", "remove", "--force", &worktree.display().to_string()]).await;
        let _ = git_in(&self.root, &["worktree", "prune"]).await;
        Ok(())
    }

    pub async fn cleanup_all(&self) -> Result<usize> {
        let dir = self.root.join(".rift/worktrees");
        let mut removed = 0;
        if dir.exists() {
            let mut rd = tokio::fs::read_dir(&dir).await?;
            while let Some(e) = rd.next_entry().await? {
                self.remove_worktree(&e.path()).await?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    /// Directory-safe label, also the patch name.
    pub name: String,
    pub model: String,
    pub temperature: Option<f64>,
}

impl Candidate {
    pub fn from_model(model: &str, ordinal: usize) -> Self {
        let safe: String = model
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' })
            .collect();
        Self { name: format!("{ordinal}-{safe}"), model: model.to_string(), temperature: None }
    }
}

#[derive(Debug)]
pub struct CandidateOutcome {
    pub candidate: Candidate,
    pub worktree: PathBuf,
    pub patch_path: Option<PathBuf>,
    pub diff_stat: String,
    /// The candidate's final answer text.
    pub summary: String,
    pub stats: TurnStats,
    pub error: Option<String>,
}

/// Run the same task with every candidate concurrently, each in its own
/// worktree. Events stream out tagged with the candidate index. Failures are
/// per-candidate, never collective.
pub async fn run_swarm(
    client: &Arc<dyn Provider>,
    base_cfg: &AgentConfig,
    swarm: &Swarm,
    candidates: Vec<Candidate>,
    task: &str,
    tx: UnboundedSender<(usize, AgentEvent)>,
    cancel: &CancellationToken,
) -> Vec<CandidateOutcome> {
    let futures = candidates.into_iter().enumerate().map(|(idx, cand)| {
        let client = client.clone();
        let mut cfg = base_cfg.clone();
        let tx = tx.clone();
        let task = task.to_string();
        async move {
            let worktree = match swarm.create_worktree(&cand.name).await {
                Ok(p) => p,
                Err(e) => {
                    return CandidateOutcome {
                        candidate: cand,
                        worktree: PathBuf::new(),
                        patch_path: None,
                        diff_stat: String::new(),
                        summary: String::new(),
                        stats: TurnStats::default(),
                        error: Some(format!("worktree: {e:#}")),
                    }
                }
            };

            cfg.model = cand.model.clone();
            cfg.temperature = cand.temperature;
            cfg.always_task = true;
            // Per-model capability check: never send think to a non-thinking
            // model, clamp num_ctx to the model's max.
            match client.show(&cand.model).await {
                Ok(show) => {
                    cfg.think = if show.supports("thinking") { None } else { Some(false) };
                    if let Some(max) = show.context_length() {
                        cfg.num_ctx = cfg.num_ctx.min(max);
                    }
                }
                Err(e) => {
                    return CandidateOutcome {
                        candidate: cand,
                        worktree,
                        patch_path: None,
                        diff_stat: String::new(),
                        summary: String::new(),
                        stats: TurnStats::default(),
                        error: Some(format!("model preflight: {e}")),
                    }
                }
            }

            // Per-candidate prompt: cross-provider races pick each model's
            // family target, not one generic prompt for the whole swarm.
            let system_prompt = crate::system_prompt_with_guide(&cfg.model, &worktree).0;
            let mut agent = Agent::new(
                client,
                cfg,
                ToolRegistry::standard(),
                ToolCtx::new(&worktree),
                system_prompt,
            );

            // Forward this candidate's events tagged with its index.
            let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            let tx_fwd = tx.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(ev) = erx.recv().await {
                    let _ = tx_fwd.send((idx, ev));
                }
            });

            let run = agent.run_turn(&task, &etx, cancel).await;
            drop(etx);
            let _ = forwarder.await;

            let (stats, error) = match run {
                Ok(s) => (s, None),
                Err(e) => (TurnStats::default(), Some(format!("{e:#}"))),
            };
            let summary = agent
                .messages
                .iter()
                .rev()
                .find(|m| m.role == rift_provider::Role::Assistant && !m.content.is_empty())
                .map(|m| m.content.clone())
                .unwrap_or_default();

            let (patch_path, diff_stat) = match swarm.capture_diff(&worktree).await {
                Ok((patch, stat)) if !patch.trim().is_empty() => {
                    match swarm.save_patch(&cand.name, &patch).await {
                        Ok(p) => (Some(p), stat),
                        Err(e) => (None, format!("patch save failed: {e:#}")),
                    }
                }
                Ok(_) => (None, "(no changes)".into()),
                Err(e) => (None, format!("diff failed: {e:#}")),
            };

            CandidateOutcome { candidate: cand, worktree, patch_path, diff_stat, summary, stats, error }
        }
    });

    futures_util::future::join_all(futures).await
}
