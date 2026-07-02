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
use rift_provider::{ChatOptions, ChatRequest, Message, Provider};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentConfig, AgentEvent, TurnStats};
use crate::tools::{ToolCtx, ToolRegistry};

/// Resolves a (possibly provider-prefixed) model string like
/// `anthropic/claude-sonnet-5` or `gemma4:26b` to a ready provider client
/// plus the bare model name. Lets a single swarm race candidates across
/// different providers — local vs cloud in the same run. Built by the
/// caller (rift-tui owns the provider crates and the config).
pub type ProviderFactory =
    Arc<dyn Fn(&str) -> Result<(Arc<dyn Provider>, String)> + Send + Sync>;

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
    /// files are included). Interpreter/build cache junk a candidate creates
    /// by *running* code (not writing it) is excluded — it pollutes patches
    /// and unfairly dings candidates in judged races.
    pub async fn capture_diff(&self, worktree: &Path) -> Result<(String, String)> {
        git_in(worktree, &["add", "-A"]).await?;
        const PATHSPEC: &[&str] = &[
            "--",
            ".",
            ":(exclude)__pycache__",
            ":(exclude)*.pyc",
            ":(exclude).pytest_cache",
            ":(exclude)node_modules",
        ];
        let mut patch_args = vec!["diff", "--cached", "--binary"];
        patch_args.extend_from_slice(PATHSPEC);
        let patch = git_in(worktree, &patch_args).await?;
        let mut stat_args = vec!["diff", "--cached", "--stat"];
        stat_args.extend_from_slice(PATHSPEC);
        let stat = git_in(worktree, &stat_args).await?;
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
/// worktree. Each candidate's model string resolves through `provider_for`,
/// so one race can mix providers (`gemma4:26b` vs `anthropic/claude-…`).
/// Events stream out tagged with the candidate index. Failures are
/// per-candidate, never collective.
pub async fn run_swarm(
    provider_for: &ProviderFactory,
    base_cfg: &AgentConfig,
    swarm: &Swarm,
    candidates: Vec<Candidate>,
    task: &str,
    tx: UnboundedSender<(usize, AgentEvent)>,
    cancel: &CancellationToken,
) -> Vec<CandidateOutcome> {
    let futures = candidates.into_iter().enumerate().map(|(idx, cand)| {
        let provider_for = provider_for.clone();
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

            let (client, bare_model) = match provider_for(&cand.model) {
                Ok(r) => r,
                Err(e) => {
                    return CandidateOutcome {
                        candidate: cand,
                        worktree,
                        patch_path: None,
                        diff_stat: String::new(),
                        summary: String::new(),
                        stats: TurnStats::default(),
                        error: Some(format!("provider: {e:#}")),
                    }
                }
            };
            cfg.model = bare_model;
            cfg.temperature = cand.temperature;
            cfg.always_task = true;
            // Per-model capability check: never send think to a non-thinking
            // model, clamp num_ctx to the model's max.
            match client.show(&cfg.model).await {
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

/// The referee's call on a finished race.
#[derive(Debug, Clone)]
pub struct JudgeVerdict {
    /// Candidate name the judge picked, if it picked one it was allowed to
    /// (must have produced changes). None = judge declined or answer unparsable.
    pub winner: Option<String>,
    /// The judge's full scoring text, shown to the user.
    pub text: String,
}

/// Cap per-candidate patch text shown to the judge — enough to see the whole
/// change on these tasks without blowing the judge's context on big diffs.
const JUDGE_PATCH_MAX_CHARS: usize = 6000;
const JUDGE_SUMMARY_MAX_CHARS: usize = 400;

fn cap(s: &str, max: usize, label: &str) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n[{label} truncated]")
}

/// Build the judge's single-shot prompt from the race outcomes.
fn judge_prompt(task: &str, outcomes: &[CandidateOutcome]) -> String {
    let mut p = format!(
        "You are judging a coding-agent race. Every candidate was given this task in an \
         identical copy of the repository:\n\n{task}\n\n\
         Below is each candidate's change as a unified diff, plus its own summary. Judge by \
         the changes ONLY: does the diff correctly accomplish the task? Then prefer the more \
         minimal, cleaner change. Self-summaries are claims, not evidence. A candidate with \
         no changes or an error cannot win.\n"
    );
    for o in outcomes {
        p.push_str(&format!("\n--- candidate {} (model {})\n", o.candidate.name, o.candidate.model));
        if let Some(e) = &o.error {
            p.push_str(&format!("error: {e}\n"));
            continue;
        }
        match &o.patch_path {
            Some(path) => {
                p.push_str(&format!("diff stat:\n{}\n", o.diff_stat.trim()));
                let patch = std::fs::read_to_string(path).unwrap_or_else(|e| format!("(patch unreadable: {e})"));
                p.push_str(&format!("patch:\n{}\n", cap(&patch, JUDGE_PATCH_MAX_CHARS, "patch")));
            }
            None => p.push_str("(no changes)\n"),
        }
        if !o.summary.is_empty() {
            p.push_str(&format!("summary: {}\n", cap(&o.summary, JUDGE_SUMMARY_MAX_CHARS, "summary")));
        }
    }
    p.push_str(
        "\nRespond with exactly:\n\
         - one line per candidate: SCORE <name>: <0-10> — <one-sentence reason>\n\
         - a final line: WINNER: <name>   (or WINNER: none if no candidate made correct changes)",
    );
    p
}

/// Parse `WINNER: <name>` out of the judge's reply, tolerating case and
/// decoration; the name must match a real candidate (exact, else unique
/// substring either way).
fn parse_winner(text: &str, outcomes: &[CandidateOutcome]) -> Option<String> {
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find_map(|l| {
            let lower = l.to_lowercase();
            lower.find("winner:").map(|i| l[i + "winner:".len()..].trim().to_string())
        })?;
    let pick = line.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '.').to_string();
    if pick.is_empty() || pick.eq_ignore_ascii_case("none") {
        return None;
    }
    let names: Vec<&str> = outcomes.iter().map(|o| o.candidate.name.as_str()).collect();
    if let Some(n) = names.iter().find(|n| n.eq_ignore_ascii_case(&pick)) {
        return Some(n.to_string());
    }
    let matches: Vec<&&str> = names
        .iter()
        .filter(|n| n.to_lowercase().contains(&pick.to_lowercase()) || pick.to_lowercase().contains(&n.to_lowercase()))
        .collect();
    match matches.as_slice() {
        [one] => Some(one.to_string()),
        _ => None,
    }
}

/// Score a finished race with a referee model and recommend a winner.
/// One plain chat call, no tools; the judge sees the task, every diff, and
/// every self-summary. A judge pick that names a changeless candidate is
/// discarded (judges must follow their own rules).
pub async fn judge_swarm(
    provider_for: &ProviderFactory,
    judge_model: &str,
    num_ctx: u64,
    task: &str,
    outcomes: &[CandidateOutcome],
) -> Result<JudgeVerdict> {
    let (client, model) = provider_for(judge_model)?;
    // Same capability etiquette as candidates: never send `think` to a
    // non-thinking model.
    let think = match client.show(&model).await {
        Ok(show) => {
            if show.supports("thinking") {
                None
            } else {
                Some(false)
            }
        }
        Err(e) => bail!("judge model preflight: {e}"),
    };

    let req = ChatRequest {
        model: model.clone(),
        messages: vec![Message::user(judge_prompt(task, outcomes))],
        tools: vec![],
        stream: true,
        think,
        keep_alive: Some("10m".into()),
        options: Some(ChatOptions {
            num_ctx: Some(num_ctx),
            temperature: Some(0.0),
            num_predict: None,
        }),
    };
    let mut sink = |_delta| {};
    let outcome = client.chat_stream(&req, &mut sink).await.context("judge call")?;
    let text = outcome.message.content.trim().to_string();

    let mut winner = parse_winner(&text, outcomes);
    if let Some(w) = &winner {
        let valid = outcomes
            .iter()
            .any(|o| &o.candidate.name == w && o.patch_path.is_some() && o.error.is_none());
        if !valid {
            winner = None;
        }
    }
    Ok(JudgeVerdict { winner, text })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(name: &str, with_patch: bool) -> CandidateOutcome {
        CandidateOutcome {
            candidate: Candidate { name: name.into(), model: "m".into(), temperature: None },
            worktree: PathBuf::new(),
            patch_path: with_patch.then(|| PathBuf::from("/x.patch")),
            diff_stat: String::new(),
            summary: String::new(),
            stats: TurnStats::default(),
            error: None,
        }
    }

    #[test]
    fn winner_parsing_tolerates_case_and_decoration() {
        let outs = vec![outcome("0-gemma4-26b", true), outcome("1-ornith-35b", true)];
        assert_eq!(
            parse_winner("SCORE ...\nWINNER: 1-ornith-35b", &outs).as_deref(),
            Some("1-ornith-35b")
        );
        assert_eq!(
            parse_winner("winner: **0-gemma4-26b**", &outs).as_deref(),
            Some("0-gemma4-26b")
        );
        assert_eq!(parse_winner("WINNER: none", &outs), None);
        assert_eq!(parse_winner("no verdict line at all", &outs), None);
        // Ambiguous partial that matches nothing stays None.
        assert_eq!(parse_winner("WINNER: candidate-7", &outs), None);
    }

    #[test]
    fn judge_prompt_includes_diffs_and_rules() {
        let outs = vec![outcome("a", true), outcome("b", false)];
        let p = judge_prompt("fix the bug", &outs);
        assert!(p.contains("fix the bug"));
        assert!(p.contains("candidate a"));
        assert!(p.contains("(no changes)"));
        assert!(p.contains("WINNER:"));
    }
}
