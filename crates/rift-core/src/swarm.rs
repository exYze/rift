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
    /// Calibrated certainty in `winner`, 0..1 — only a System One judge
    /// reports one. `None` from the chat judge, which has no calibrated
    /// notion of how sure it is.
    pub confidence: Option<f64>,
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
        effort: None,
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
    Ok(JudgeVerdict { winner, text, confidence: None })
}

// ---- System One (Jev) judge ----------------------------------------------

/// Question ids. Both are asked in ONE call: `state` (every diff) is the
/// expensive part of the request and is billed once per call, not per
/// question — the documented fan-out pattern.
const Q_WINNER: &str = "winner";
const Q_ANY_CORRECT: &str = "any_correct";

/// Below this probability that *any* candidate is correct, the race has no
/// winner. This is the check the chat judge can only be asked to make for
/// itself — and then has to be re-verified in code, because it sometimes
/// picks anyway.
const ANY_CORRECT_MIN: f64 = 0.5;
/// Below this confidence the pick is still reported, but flagged for review
/// rather than presented as a clean recommendation.
const LOW_CONFIDENCE: f64 = 0.5;

/// Does this judge spec name a System One model rather than a chat model?
/// Accepts `jev`, a pinned `jev-1.13.0`, or the provider-prefixed
/// `typesafe/<model>` form rift already uses for cloud routing.
pub fn is_system_one_judge(judge_model: &str) -> bool {
    let m = judge_model.trim().to_lowercase();
    m == "jev" || m.starts_with("jev-") || m.starts_with("typesafe/")
}

/// The System One model id inside a judge spec (`typesafe/jev-1.13.0` ->
/// `jev-1.13.0`, bare `jev` -> the default alias).
pub fn system_one_model(judge_model: &str) -> String {
    let m = judge_model.trim();
    let m = m.strip_prefix("typesafe/").or_else(|| m.strip_prefix("TypeSafe/")).unwrap_or(m);
    if m.eq_ignore_ascii_case("jev") || m.is_empty() {
        rift_typesafe::DEFAULT_MODEL.to_string()
    } else {
        m.to_string()
    }
}

/// Only candidates that actually produced a change may win — the same rule
/// the chat judge is told in prose. Here it is enforced structurally: an
/// ineligible candidate is never offered as an option, so the model cannot
/// pick one.
fn eligible(outcomes: &[CandidateOutcome]) -> Vec<&CandidateOutcome> {
    outcomes.iter().filter(|o| o.patch_path.is_some() && o.error.is_none()).collect()
}

/// The race as structured state. Jev takes program state directly, so the
/// diffs go in as data rather than being flattened into a prompt.
fn jev_state(task: &str, outcomes: &[CandidateOutcome]) -> serde_json::Value {
    let candidates: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|o| {
            let mut m = serde_json::json!({
                "name": o.candidate.name,
                "model": o.candidate.model,
            });
            if let Some(e) = &o.error {
                m["status"] = "errored".into();
                m["error"] = e.clone().into();
            } else if let Some(path) = &o.patch_path {
                let patch = std::fs::read_to_string(path)
                    .unwrap_or_else(|e| format!("(patch unreadable: {e})"));
                m["status"] = "changed".into();
                m["diff_stat"] = o.diff_stat.trim().into();
                m["patch"] = cap(&patch, JUDGE_PATCH_MAX_CHARS, "patch").into();
            } else {
                m["status"] = "no changes".into();
            }
            if !o.summary.is_empty() {
                // Labelled as a claim: the model must weigh the diff, not the
                // candidate's own account of it.
                m["self_reported_summary"] = cap(&o.summary, JUDGE_SUMMARY_MAX_CHARS, "summary").into();
            }
            m
        })
        .collect();
    serde_json::json!({ "task": task, "candidates": candidates })
}

/// Render a typed verdict for humans: the decision first, then the
/// distribution it came from.
fn render_jev_verdict(
    model: &str,
    winner: &Option<String>,
    confidence: Option<f64>,
    any_correct: f64,
    ranked: &[(String, f64)],
    outcomes: &[CandidateOutcome],
) -> String {
    let mut t = format!("judge: {model} (System One — typed decision, no prose)
");
    let verdict = if any_correct >= ANY_CORRECT_MIN { "yes" } else { "no" };
    t.push_str(&format!("any candidate correct? {verdict} (p={any_correct:.2})
"));
    match winner {
        Some(w) => {
            let c = confidence.unwrap_or(0.0);
            t.push_str(&format!("winner: {w} (confidence {c:.2})
"));
            if c < LOW_CONFIDENCE {
                t.push_str("  low confidence — read the diff before merging
");
            }
        }
        None if any_correct < ANY_CORRECT_MIN => {
            t.push_str("winner: none — no candidate is likely to have solved the task
");
        }
        None => t.push_str("winner: none
"),
    }
    if !ranked.is_empty() {
        t.push_str("scores:
");
        for (name, p) in ranked {
            t.push_str(&format!("  {name}: {p:.2}
"));
        }
    }
    let skipped: Vec<String> = outcomes
        .iter()
        .filter(|o| o.patch_path.is_none() || o.error.is_some())
        .map(|o| {
            let why = if o.error.is_some() { "errored" } else { "no changes" };
            format!("{} ({why})", o.candidate.name)
        })
        .collect();
    if !skipped.is_empty() {
        t.push_str(&format!("not eligible: {}
", skipped.join(", ")));
    }
    t
}

/// Score a finished race with a System One model.
///
/// The chat judge has to be *asked* for a `WINNER:` line and then parsed,
/// with all the failure modes that implies — a missing line, a hallucinated
/// name, a pick that breaks the rules it was given. Here the winner is a
/// `choice` over the eligible candidates, so it comes back as a name that
/// exists, plus a probability per candidate and a calibrated confidence.
pub async fn judge_swarm_system_one(
    client: &rift_typesafe::TypeSafeClient,
    task: &str,
    outcomes: &[CandidateOutcome],
) -> Result<JudgeVerdict> {
    let runners = eligible(outcomes);
    if runners.is_empty() {
        // Nothing to choose between — don't spend a call to be told so.
        return Ok(JudgeVerdict {
            winner: None,
            text: "judge: no candidate produced changes — nothing to judge
".into(),
            confidence: None,
        });
    }

    let options: Vec<(String, Option<String>)> = runners
        .iter()
        .map(|o| {
            let stat = o.diff_stat.trim().replace('\n', "; ");
            (
                o.candidate.name.clone(),
                Some(format!("produced by {} — {}", o.candidate.model, stat)),
            )
        })
        .collect();

    let questions = vec![
        (
            Q_WINNER.to_string(),
            rift_typesafe::Question::choice(
                "Every candidate was given the same task in an identical copy of the repository. Which candidate's diff best accomplishes it? Judge the diffs ONLY: correctness first, then prefer the smaller, cleaner change. A self_reported_summary is the candidate's own claim, not evidence.",
                options,
            ),
        ),
        (
            Q_ANY_CORRECT.to_string(),
            rift_typesafe::Question::noul_with(
                "Does at least one candidate's diff correctly and completely accomplish the task?",
                "At least one diff is a correct and complete solution to the task.",
                "No diff correctly and completely solves the task — all are wrong, partial, or off-target.",
            ),
        ),
    ];

    let state = jev_state(task, outcomes);
    let ev = client.evaluate(&state, &questions).await.context("system one judge call")?;

    let winner_answer = ev.require(Q_WINNER)?;
    let any_correct = ev
        .get(Q_ANY_CORRECT)
        .and_then(|a| a.as_noul())
        // A missing noul must not silently promote a winner; treat it as
        // "not established".
        .unwrap_or(0.0);

    let picked = winner_answer.as_choice().map(str::to_string);
    let confidence = winner_answer.confidence();
    let ranked = winner_answer.ranked();

    // Gate on correctness, then re-verify eligibility. The option list made
    // an ineligible pick impossible, but a future API change must not turn
    // that into a silently wrong merge recommendation.
    let mut winner = if any_correct >= ANY_CORRECT_MIN { picked } else { None };
    if let Some(w) = &winner {
        if !runners.iter().any(|o| &o.candidate.name == w) {
            winner = None;
        }
    }

    let text = render_jev_verdict(&ev.model, &winner, confidence, any_correct, &ranked, outcomes);
    Ok(JudgeVerdict { winner, text, confidence })
}

/// Judge a race with whichever referee `judge_model` names: a System One
/// model (`jev`, `typesafe/jev-latest`) or an ordinary chat model.
///
/// `typesafe` is the configured client, or `None` when no API key is set —
/// in which case naming a System One judge is a clear error rather than a
/// silent downgrade to a different referee than the one that was asked for.
pub async fn judge_race(
    typesafe: Option<&rift_typesafe::TypeSafeClient>,
    provider_for: &ProviderFactory,
    judge_model: &str,
    num_ctx: u64,
    task: &str,
    outcomes: &[CandidateOutcome],
) -> Result<JudgeVerdict> {
    if is_system_one_judge(judge_model) {
        let Some(client) = typesafe else {
            bail!(
                "judge {judge_model} is a TypeSafe System One model but no API key is configured \
                 — set {} or a \"typesafe\" entry in the rift config, or name a chat model as judge",
                rift_typesafe::API_KEY_ENV
            )
        };
        let client = client.with_model(system_one_model(judge_model));
        return judge_swarm_system_one(&client, task, outcomes).await;
    }
    judge_swarm(provider_for, judge_model, num_ctx, task, outcomes).await
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
    fn system_one_judge_specs_are_recognized_and_normalized() {
        for spec in ["jev", "JEV", "jev-1.13.0", "typesafe/jev-latest", " typesafe/jev "] {
            assert!(is_system_one_judge(spec), "{spec:?} should route to a System One judge");
        }
        // Chat judges must keep going to the chat path.
        for spec in ["gemma4:26b", "anthropic/claude-opus-5", "openrouter/qwen3", "jevons"] {
            assert!(!is_system_one_judge(spec), "{spec:?} must not route to System One");
        }
        assert_eq!(system_one_model("jev"), rift_typesafe::DEFAULT_MODEL);
        assert_eq!(system_one_model("typesafe/jev"), rift_typesafe::DEFAULT_MODEL);
        assert_eq!(system_one_model("typesafe/jev-1.13.0"), "jev-1.13.0");
        assert_eq!(system_one_model("jev-1.13.0"), "jev-1.13.0");
    }

    // Only candidates that changed something may be offered as options —
    // this is what makes an illegal pick unrepresentable rather than merely
    // discouraged.
    #[test]
    fn only_changed_candidates_are_eligible() {
        let mut errored = outcome("c", true);
        errored.error = Some("boom".into());
        let outs = vec![outcome("a", true), outcome("b", false), errored];
        let names: Vec<&str> = eligible(&outs).iter().map(|o| o.candidate.name.as_str()).collect();
        assert_eq!(names, ["a"]);
    }

    #[test]
    fn jev_state_carries_task_status_and_claims() {
        let mut outs = vec![outcome("a", true), outcome("b", false)];
        outs[0].summary = "fixed the parser".into();
        outs[0].diff_stat = "1 file changed".into();
        let st = jev_state("fix the bug", &outs);
        assert_eq!(st["task"], "fix the bug");
        assert_eq!(st["candidates"][0]["name"], "a");
        assert_eq!(st["candidates"][0]["status"], "changed");
        // A self-summary must travel labelled as a claim, not as evidence.
        assert_eq!(st["candidates"][0]["self_reported_summary"], "fixed the parser");
        assert_eq!(st["candidates"][1]["status"], "no changes");
        // An unreadable patch degrades to a note instead of panicking.
        assert!(st["candidates"][0]["patch"].as_str().unwrap().contains("unreadable"));
    }

    #[test]
    fn errored_candidate_state_reports_the_error() {
        let mut e = outcome("a", false);
        e.error = Some("worktree exploded".into());
        let st = jev_state("t", &[e]);
        assert_eq!(st["candidates"][0]["status"], "errored");
        assert_eq!(st["candidates"][0]["error"], "worktree exploded");
    }

    #[test]
    fn verdict_text_reports_decision_distribution_and_skips() {
        let outs = vec![outcome("a", true), outcome("b", false)];
        let ranked = vec![("a".to_string(), 0.8), ("b".to_string(), 0.2)];
        let t = render_jev_verdict(
            "jev-1.13.0",
            &Some("a".into()),
            Some(0.77),
            0.9,
            &ranked,
            &outs,
        );
        assert!(t.contains("jev-1.13.0"));
        assert!(t.contains("any candidate correct? yes (p=0.90)"));
        assert!(t.contains("winner: a (confidence 0.77)"));
        assert!(t.contains("a: 0.80"));
        assert!(t.contains("not eligible: b (no changes)"));
        assert!(!t.contains("low confidence"));
    }

    #[test]
    fn low_confidence_pick_is_flagged_for_review() {
        let outs = vec![outcome("a", true)];
        let t = render_jev_verdict("jev-1.13.0", &Some("a".into()), Some(0.31), 0.9, &[], &outs);
        assert!(t.contains("low confidence — read the diff before merging"));
    }

    #[test]
    fn no_winner_text_explains_why_when_nothing_was_correct() {
        let outs = vec![outcome("a", true)];
        let t = render_jev_verdict("jev-1.13.0", &None, None, 0.12, &[], &outs);
        assert!(t.contains("no candidate is likely to have solved the task"));
    }

    // With nothing mergeable there is no decision to buy — the judge must
    // not spend an API call to be told the race was empty.
    #[tokio::test]
    async fn empty_race_is_judged_without_a_request() {
        let outs = vec![outcome("a", false)];
        // An unroutable base URL: if this tried to call out, it would error.
        let client = rift_typesafe::TypeSafeClient::new("https://127.0.0.1:1", "k", "jev-latest");
        let v = judge_swarm_system_one(&client, "t", &outs).await.unwrap();
        assert!(v.winner.is_none());
        assert!(v.text.contains("nothing to judge"));
    }

    // Asking for a System One judge with no key must say so plainly, not
    // quietly referee the race with some other model.
    #[tokio::test]
    async fn system_one_judge_without_a_key_is_a_clear_error() {
        let factory: ProviderFactory =
            std::sync::Arc::new(|_: &str| bail!("the chat judge must not be used here"));
        let outs = vec![outcome("a", true)];
        let e = judge_race(None, &factory, "jev", 8192, "t", &outs).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("TYPESAFE_API_KEY"), "{msg}");
        assert!(msg.contains("no API key is configured"), "{msg}");
    }

    // A chat judge spec must still reach the chat path, key or no key.
    #[tokio::test]
    async fn chat_judge_spec_still_routes_to_the_chat_judge() {
        let factory: ProviderFactory =
            std::sync::Arc::new(|_: &str| bail!("chat judge reached"));
        let outs = vec![outcome("a", true)];
        let e = judge_race(None, &factory, "gemma4:26b", 8192, "t", &outs).await.unwrap_err();
        assert!(format!("{e:#}").contains("chat judge reached"));
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
