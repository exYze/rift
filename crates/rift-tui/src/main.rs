mod app;
mod clipboard;
mod commands;
mod highlight;
mod pricing;
mod swarm_ui;
mod theme;
mod update;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use rift_core::{
    run_swarm, Agent, AgentConfig, AgentEvent, AskRequest, AskUserTool, Candidate, Config,
    McpClient, McpTool, ProviderConfig, SessionStore, Swarm, ToolCtx, ToolRegistry,
};
use rift_anthropic::AnthropicClient;
use rift_ollama::{OllamaClient, Provider};
use rift_openai::OpenAiClient;

/// Build a provider (and the actual model name) from a possibly-prefixed model
/// string. `openrouter/qwen3` routes through the configured `openrouter`
/// provider with model `qwen3`. `anthropic/<model>` and `openai/<model>` work
/// with no config entry at all — just ANTHROPIC_API_KEY / OPENAI_API_KEY in
/// the environment (cloud is an option, never a requirement). Any other model
/// uses the default Ollama server.
pub(crate) fn build_provider(
    model: &str,
    host: &str,
    providers: &HashMap<String, ProviderConfig>,
) -> (Arc<dyn Provider>, String) {
    if let Some((name, rest)) = model.split_once('/') {
        if let Some(pc) = providers.get(name) {
            let client: Arc<dyn Provider> = match pc.kind.as_deref() {
                Some("anthropic") => Arc::new(AnthropicClient::new(&pc.base_url, pc.resolve_key())),
                _ => Arc::new(OpenAiClient::new(&pc.base_url, pc.resolve_key())),
            };
            return (client, rest.to_string());
        }
        // Built-in cloud providers (config entries with the same name override).
        match name {
            "anthropic" => {
                let key = std::env::var("ANTHROPIC_API_KEY").ok();
                return (Arc::new(AnthropicClient::new("https://api.anthropic.com", key)), rest.to_string());
            }
            "openai" => {
                let key = std::env::var("OPENAI_API_KEY").ok();
                return (Arc::new(OpenAiClient::new("https://api.openai.com/v1", key)), rest.to_string());
            }
            _ => {}
        }
    }
    // An OpenAI-style default host (…/v1 — vLLM, LM Studio, llama.cpp
    // server, set via config `host` or /host autodetection) serves bare
    // model names through the OpenAI-compat client; anything else is a
    // native Ollama server.
    if host.trim_end_matches('/').ends_with("/v1") {
        return (Arc::new(OpenAiClient::new(host, None)), model.to_string());
    }
    (Arc::new(OllamaClient::new(host)), model.to_string())
}

#[cfg(test)]
mod provider_tests {
    use super::*;

    #[test]
    fn bare_models_follow_the_host_kind() {
        let providers = HashMap::new();
        // Ollama host: bare names go native (no /v1 on the base URL).
        let (client, name) = build_provider("gemma4:26b", "http://box:11434", &providers);
        assert!(!client.base_url().ends_with("/v1"), "got {}", client.base_url());
        assert_eq!(name, "gemma4:26b");
        // OpenAI-style host: bare names ride the OpenAI-compat client.
        let (client, name) = build_provider("some-model", "http://box:8000/v1", &providers);
        assert!(client.base_url().ends_with("/v1"), "got {}", client.base_url());
        assert_eq!(name, "some-model");
        // Trailing slash doesn't confuse the detection.
        let (client, _) = build_provider("m", "http://box:8000/v1/", &providers);
        assert!(client.base_url().ends_with("/v1"), "got {}", client.base_url());
        // Provider prefixes still win over the host kind.
        let (_, name) = build_provider("openai/gpt-5", "http://box:8000/v1", &providers);
        assert_eq!(name, "gpt-5");
    }
}

/// A [`rift_core::ProviderFactory`] over `build_provider`: lets the swarm
/// (and its judge) resolve each candidate's model string independently, so
/// one race can mix local and cloud providers.
pub(crate) fn provider_factory(
    host: &str,
    providers: &HashMap<String, ProviderConfig>,
) -> rift_core::ProviderFactory {
    let host = host.to_string();
    let providers = providers.clone();
    Arc::new(move |model: &str| Ok(build_provider(model, &host, &providers)))
}
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(name = "rift", version, about = "Rift — a fast terminal coding agent for local Ollama models")]
struct Cli {
    /// Ollama server URL [default: http://localhost:11434, or `host` in config]
    #[arg(long, env = "RIFT_HOST")]
    host: Option<String>,
    /// Model to use, needs the "tools" capability [default: gemma4:26b, or `model` in config]
    #[arg(long, short, env = "RIFT_MODEL")]
    model: Option<String>,
    /// Context window to request per call [default: 32768, or `num_ctx` in config]
    #[arg(long)]
    num_ctx: Option<u64>,
    /// Run a single prompt headless (no TUI) and print the transcript
    #[arg(long, short)]
    prompt: Option<String>,
    /// Max agent-loop iterations per turn [default: 25, or `max_iterations` in config]
    #[arg(long)]
    max_iterations: Option<usize>,
    /// Resume the most recent session
    #[arg(long = "continue", short = 'c')]
    cont: bool,
    /// Resume a specific session file
    #[arg(long)]
    resume: Option<PathBuf>,
    /// Ask before every write/edit/bash action (also: permissions.approve in config)
    #[arg(long)]
    approve: bool,
    /// Sampling temperature, low = reliable tool calling [default: 0.2, or `temperature` in config]
    #[arg(long)]
    temp: Option<f64>,
    /// Reasoning effort for thinking models: minimal|low|medium|high|xhigh|max
    /// [default: the model's own, or `effort` in config]
    #[arg(long)]
    effort: Option<String>,
    /// Append one JSON line per turn (model, tokens, tool calls, failure
    /// counters, outcome) to this file. Local file only; off by default
    #[arg(long, env = "RIFT_TRACE")]
    trace: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// WarpDrive: run the same task with multiple models in parallel,
    /// each in an isolated git worktree (your working tree is untouched)
    Swarm {
        /// The task to give every candidate
        task: String,
        /// Comma-separated models to race
        #[arg(long, default_value = "gemma4:26b,gemma4:12b")]
        models: String,
        /// Also run each model at temperature 0.8 as an extra candidate
        #[arg(long)]
        explore: bool,
        /// Referee model that scores the candidates' diffs and recommends a
        /// winner after the race (e.g. qwen3.6:35b or anthropic/claude-sonnet-5)
        #[arg(long)]
        judge: Option<String>,
        /// Plain streaming output instead of the interactive TUI
        #[arg(long)]
        no_tui: bool,
    },
    /// Apply a swarm candidate's patch to your working tree
    Merge {
        /// Candidate name as shown by swarm (e.g. 0-gemma4-26b)
        name: String,
        /// Remove all swarm worktrees afterwards
        #[arg(long)]
        cleanup: bool,
    },
    /// Update rift to the latest release
    Update,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load config up front so it can supply defaults for host/model. Precedence
    // for each: CLI flag > env var (clap folds both into `cli`) > config file >
    // built-in default — so `rift` with no flags uses your configured server.
    let cwd = std::env::current_dir()?;
    let loaded = Config::load(&cwd)?;
    for p in &loaded.paths {
        eprintln!("config: {}", p.display());
    }
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }
    let config = loaded.config;
    // The file `/config edit` opens: highest-precedence existing one.
    let config_path = loaded.paths.last().cloned();
    let host = cli
        .host
        .clone()
        .or_else(|| config.host.clone())
        .unwrap_or_else(|| "http://localhost:11434".to_string());
    let model = cli
        .model
        .clone()
        .or_else(|| config.model.clone())
        .unwrap_or_else(|| "gemma4:26b".to_string());
    let explicit_ctx = cli.num_ctx.or(config.num_ctx);
    let num_ctx = explicit_ctx.unwrap_or(32_768);
    let temp = cli.temp.or(config.temperature).unwrap_or(0.2);
    let effort = cli.effort.clone().or_else(|| config.effort.clone());
    if let Some(e) = &effort {
        if !rift_core::EFFORT_LEVELS.contains(&e.as_str()) {
            bail!("unknown effort '{e}' — use one of: {}", rift_core::EFFORT_LEVELS.join(", "));
        }
    }
    let max_iterations = cli.max_iterations.or(config.max_iterations).unwrap_or(25);

    // A `provider/model` string routes through a configured provider; otherwise
    // the default Ollama server at `host`. `model` becomes the bare model name;
    // the addressable form (prefix intact) is what /model, /restart and the
    // status line carry.
    let model_addr = model.clone();
    let (client, model) = build_provider(&model, &host, &config.providers);
    // Routed through a non-Ollama provider (prefix or an OpenAI-style /v1
    // host): num_ctx is rift's internal budget only (never sent to the
    // server), so adopting a larger server-reported context is free. On
    // Ollama, num_ctx sizes the server's KV cache — there the conservative
    // default stands unless the user raises it.
    let provider_routed = model != model_addr || host.trim_end_matches('/').ends_with("/v1");

    // Subcommands bypass the single-model preflight (swarm preflights each
    // candidate itself; merge is git-only).
    match &cli.cmd {
        Some(Cmd::Swarm { task, models, explore, judge, no_tui }) => {
            let cfg_base = AgentConfig {
                model: String::new(), // set per candidate
                num_ctx,
                temperature: Some(temp),
                max_iterations,
                think: None,
                effort: effort.clone(),
                always_task: true,
            };
            let factory = provider_factory(&host, &config.providers);
            return run_swarm_cli(factory, cfg_base, task, models, *explore, judge.clone(), *no_tui).await;
        }
        Some(Cmd::Merge { name, cleanup }) => {
            let swarm = Swarm::discover(&std::env::current_dir()?).await?;
            swarm.apply_patch(name).await?;
            println!("applied patch '{name}' to {}", swarm.root().display());
            if *cleanup {
                let n = swarm.cleanup_all().await?;
                println!("removed {n} worktree(s)");
            }
            return Ok(());
        }
        Some(Cmd::Update) => {
            println!("{}", update::self_update(env!("CARGO_PKG_VERSION")).await?);
            return Ok(());
        }
        None => {}
    }

    // Preflight: check the server is reachable and the model supports tools.
    // In the interactive TUI this is non-fatal — open anyway and let the user
    // recover with /host and /model. Headless (-p) runs still bail, since there
    // is no interactive way to fix the server/model there.
    let interactive = cli.prompt.is_none();
    let show = match client.show(&model).await {
        Ok(s) => Some(s),
        Err(e) if interactive => {
            eprintln!("warning: cannot reach Ollama at {host} or model '{model}' missing: {e}");
            eprintln!("opening anyway — use /host to set a reachable server, then /model to pick a model");
            None
        }
        Err(e) => bail!("cannot reach Ollama at {host} or model '{model}' missing: {e}"),
    };
    if let Some(show) = &show {
        if !show.supports("tools") {
            if interactive {
                eprintln!("warning: model '{model}' lacks the 'tools' capability — switch with /model");
            } else {
                bail!("model '{model}' does not have the 'tools' capability; pick a tools-capable model");
            }
        }
    }
    // Thinking mode is known only if we reached the model; otherwise leave it on
    // auto (None) until the user selects one with /model.
    let think = match &show {
        Some(s) => (!s.supports("thinking")).then_some(false),
        None => None,
    };
    /// Auto-adopted context budget ceiling for provider-routed models. Big
    /// hosted contexts (DeepSeek 500k, Gemini 1M) would let history grow so
    /// large that every request re-sends hundreds of KB — 128k is generous
    /// without that; an explicit --num-ctx/config value overrides freely.
    const ADOPT_CTX_MAX: u64 = 131_072;
    let num_ctx = match show.as_ref().and_then(|s| s.context_length()) {
        Some(max_ctx) => {
            if num_ctx > max_ctx {
                eprintln!("warning: num_ctx {num_ctx} exceeds model max {max_ctx}; using {max_ctx}");
            }
            if provider_routed && explicit_ctx.is_none() && max_ctx > num_ctx {
                let adopted = max_ctx.min(ADOPT_CTX_MAX);
                eprintln!(
                    "context: model reports {max_ctx} tokens — using {adopted} as the working budget \
                     (--num-ctx overrides)"
                );
                adopted
            } else {
                num_ctx.min(max_ctx)
            }
        }
        None => num_ctx,
    };

    let cfg = AgentConfig {
        model: model.clone(),
        num_ctx,
        temperature: Some(temp),
        max_iterations,
        think,
        effort,
        always_task: cli.prompt.is_some(),
    };

    // MCP servers + permission policy come from the config loaded up top.
    // Entries from a *project* .rift.json spawn arbitrary commands and the
    // file may have arrived inside a cloned repo — require one-time approval
    // (remembered in the user-level trust store) before running them. User
    // config entries run without prompting.
    let mut registry = ToolRegistry::standard();
    let mut mcp_status: Vec<(String, usize)> = vec![];
    let user_mcp = config.mcp.iter().map(|(n, s)| (n, s, false));
    let project_mcp = config.project_mcp.iter().map(|(n, s)| (n, s, true));
    for (name, server_cfg, from_project) in user_mcp.chain(project_mcp) {
        if from_project && !rift_core::mcp_entry_trusted(name, server_cfg) {
            let cmdline = format!("{} {}", server_cfg.command, server_cfg.args.join(" "));
            if interactive && confirm_mcp(name, cmdline.trim()) {
                if let Err(e) = rift_core::trust_mcp_entry(name, server_cfg) {
                    eprintln!("warning: could not persist MCP approval: {e:#}");
                }
            } else {
                let hint = if interactive {
                    format!(" (/mcp trust {name} to enable it)")
                } else {
                    " (approve it in an interactive session, or move it to the user config)".into()
                };
                eprintln!("mcp '{name}' skipped: defined in project .rift.json and not yet trusted{hint}");
                continue;
            }
        }
        match McpClient::spawn(name, server_cfg).await {
            Ok(mcp) => match mcp.list_tools().await {
                Ok(tools) => {
                    let count = tools.len();
                    for info in tools {
                        registry.register(Box::new(McpTool::new(mcp.clone(), info)));
                    }
                    mcp_status.push((name.clone(), count));
                    eprintln!("mcp '{name}': {count} tool(s) registered");
                }
                Err(e) => eprintln!("warning: mcp '{name}' tools/list failed: {e:#}"),
            },
            Err(e) => eprintln!("warning: mcp '{name}' failed to start: {e:#}"),
        }
    }

    // Elicitation: in interactive mode the model gets an ask_user tool wired
    // to the TUI; headless runs stay non-interactive (no tool registered).
    // Approval defaults ON (the Claude Code model): interactive sessions ask
    // before write/edit/bash, with per-command "always allow" tracking.
    // `"approve": false` in the user config or /yolo opts out; --approve and
    // a project `"approve": true` force it on.
    let approve = cli.approve || config.permissions.approve_effective();
    let mut ctx = ToolCtx::with_extra_deny(&cwd, &config.permissions.bash_deny).with_approval(approve);
    ctx.set_allow(&config.permissions.bash_allow);
    let (ask_tx, ask_rx) = mpsc::unbounded_channel::<AskRequest>();
    if interactive {
        ctx = ctx.with_interaction(ask_tx);
        registry.register(Box::new(AskUserTool));
        if approve {
            eprintln!(
                "approval: write/edit/bash ask first ({} allowed pattern(s) skip the prompt; /yolo stops asking)",
                config.permissions.bash_allow.len()
            );
        }
    } else if cli.approve || config.permissions.approve == Some(true) {
        eprintln!("note: approval mode needs the interactive TUI; running headless without it");
    }

    let (mut prompt_text, guide_files) = rift_core::system_prompt_with_guide(&model, &cwd);
    if !guide_files.is_empty() {
        eprintln!("loaded project context: {}", guide_files.join(", "));
    }

    // Skills (Agent Skills standard): listed in the system prompt, bodies
    // loaded on demand via the skill tool or /skill:<name>.
    let skills = rift_core::load_skills(&cwd);
    if !skills.is_empty() {
        prompt_text.push_str(&rift_core::skills_prompt_section(&skills));
        registry.register(Box::new(rift_core::SkillTool::new(skills.clone())));
        eprintln!("skills: {}", skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "));
    }

    // Sub-agents: the agent tool needs a provider handle to build child
    // agents. Installed on the root ctx only — children get a bare ctx, so
    // delegation stays one level deep. run_turn refreshes client/cfg, so
    // later /model and /host switches carry over; the factory + roles let
    // individual tasks run on OTHER models (config `models` role map).
    ctx.set_subagent(rift_core::SubAgentHandle {
        client: client.clone(),
        cfg: cfg.clone(),
        factory: Some(provider_factory(&host, &config.providers)),
        roles: config.models.clone(),
    });
    registry.register(Box::new(rift_core::AgentTool));
    // Multi-model workflows are opt-in: the model only hears about roles
    // that are actually configured.
    if !config.models.is_empty() {
        let mut roles: Vec<String> = config.models.iter().map(|(k, v)| format!("{k} = {v}")).collect();
        roles.sort();
        prompt_text.push_str(&format!(
            "\n\nModel roles available for delegated tasks (the agent tool accepts model=\"<role>\" \
             per task): {}. Route mechanical work (implementing a written spec, running tests) to \
             cheaper roles; keep research, specs, and review on the stronger model.",
            roles.join(", ")
        ));
    }

    let mut agent = Agent::new(client, cfg, registry, ctx, prompt_text);

    // Opt-in turn traces (--trace / RIFT_TRACE). A bad path is a warning,
    // not a startup failure — tracing must never block actual work.
    if let Some(path) = &cli.trace {
        match rift_core::TraceWriter::new(path) {
            Ok(w) => {
                eprintln!("tracing turns to {}", path.display());
                agent.set_trace(Some(w));
            }
            Err(e) => eprintln!("warning: turn tracing disabled: {e:#}"),
        }
    }

    // Session: resume an existing file or start a new one.
    let resume_path = if let Some(p) = cli.resume {
        Some(p)
    } else if cli.cont {
        let latest = SessionStore::latest()?;
        if latest.is_none() {
            eprintln!("no previous session found; starting fresh");
        }
        latest
    } else {
        None
    };
    let (store, resumed_messages) = match resume_path {
        Some(path) => {
            // Lenient on purpose: /restart resumes a session that may not
            // have saved a turn yet (files are reserved empty at startup),
            // and a corrupt autosave must not brick startup.
            let (store, mut messages, notice) = SessionStore::resume(path);
            if let Some(n) = notice {
                eprintln!("{n}");
            }
            if messages.first().is_some_and(|m| m.role == rift_ollama::Role::System) {
                // Keep the freshly composed system prompt (cwd may have changed).
                messages[0] = agent.messages[0].clone();
            }
            if !messages.is_empty() {
                agent.messages = messages.clone();
                eprintln!("resumed session {} ({} messages)", store.path().display(), messages.len());
            }
            (store, messages)
        }
        None => (SessionStore::create()?, vec![]),
    };

    // Theme: config-selected, defaulting to dark; unknown names warn and fall
    // back rather than failing startup.
    let ui_theme = match config.theme.as_deref() {
        None => &theme::DARK,
        Some(name) => theme::find(name).unwrap_or_else(|| {
            eprintln!("warning: unknown theme '{name}' (available: {}); using dark", theme::names().join(", "));
            &theme::DARK
        }),
    };

    match cli.prompt {
        Some(prompt) => {
            let rates = pricing::lookup(&model, &config.pricing);
            run_headless(agent, prompt, store, rates).await
        }
        None => {
            let restart = app::run_tui(
                agent,
                app::TuiOptions {
                    model: model_addr,
                    store,
                    resumed: resumed_messages,
                    mcp: mcp_status,
                    config_path,
                    ask_rx,
                    skills,
                    host: host.clone(),
                    providers: config.providers.clone(),
                    pricing: config.pricing.clone(),
                    theme: ui_theme,
                },
            )
            .await?;
            match restart {
                Some(spec) => restart_process(spec),
                None => Ok(()),
            }
        }
    }
}

/// The tail end of /restart: relaunch the (possibly just-updated) binary
/// resuming the same session. A true exec on Unix (same PID and terminal);
/// spawn-and-wait elsewhere so the shell keeps normal job semantics.
fn restart_process(spec: app::RestartSpec) -> Result<()> {
    let exe = std::env::current_exe()?;
    eprintln!("restarting rift — resuming {}", spec.session.display());
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--host")
        .arg(&spec.host)
        .arg("--model")
        .arg(&spec.model)
        .arg("--resume")
        .arg(&spec.session);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec only returns on failure.
        Err(anyhow::anyhow!("restart failed: {}", cmd.exec()))
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

/// `rift swarm`: race N models on one task in parallel worktrees. Interactive
/// TUI by default on a terminal; plain streaming with --no-tui (or piped).
async fn run_swarm_cli(
    factory: rift_core::ProviderFactory,
    cfg_base: AgentConfig,
    task: &str,
    models: &str,
    explore: bool,
    judge: Option<String>,
    no_tui: bool,
) -> Result<()> {
    const COLORS: [&str; 6] = ["\x1b[36m", "\x1b[35m", "\x1b[32m", "\x1b[33m", "\x1b[34m", "\x1b[31m"];
    let swarm = Swarm::discover(&std::env::current_dir()?).await?;

    let mut candidates: Vec<Candidate> = models
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
        .map(|(i, m)| Candidate::from_model(m, i))
        .collect();
    if explore {
        let extra: Vec<Candidate> = candidates
            .iter()
            .map(|c| Candidate {
                name: format!("{}-hot", c.name),
                model: c.model.clone(),
                temperature: Some(0.8),
            })
            .collect();
        candidates.extend(extra);
    }
    if candidates.is_empty() {
        bail!("no models given");
    }

    use std::io::IsTerminal;
    if !no_tui && std::io::stdout().is_terminal() {
        return swarm_ui::run_swarm_tui(factory, cfg_base, swarm, candidates, task.to_string(), judge).await;
    }

    println!("WarpDrive swarm: {} candidate(s) on task: {task}", candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        let color = COLORS[i % COLORS.len()];
        println!("  {color}{}\x1b[0m = {} (temp {})", c.name, c.model, c.temperature.map_or("default".into(), |t| t.to_string()));
    }
    println!();

    let (tx, mut rx) = mpsc::unbounded_channel::<(usize, AgentEvent)>();
    let names: Vec<String> = candidates.iter().map(|c| c.name.clone()).collect();
    let printer = tokio::spawn(async move {
        while let Some((idx, ev)) = rx.recv().await {
            let color = COLORS[idx % COLORS.len()];
            let name = &names[idx];
            match ev {
                AgentEvent::ToolStart { name: tool, args } => {
                    let args: String = args.chars().take(100).collect();
                    println!("{color}[{name}]\x1b[0m → {tool} {args}");
                }
                AgentEvent::ToolResult { name: tool, ok, .. } => {
                    println!("{color}[{name}]\x1b[0m {} {tool}", if ok { "✓" } else { "✗" });
                }
                AgentEvent::Warning(w) => println!("{color}[{name}]\x1b[0m \x1b[33m! {w}\x1b[0m"),
                AgentEvent::Info(i) => println!("{color}[{name}]\x1b[0m \x1b[2m· {i}\x1b[0m"),
                AgentEvent::Done(stats) => {
                    println!(
                        "{color}[{name}]\x1b[0m done: {} steps, {} out tok, {:.1}s",
                        stats.iterations,
                        stats.output_tokens,
                        stats.duration_ms as f64 / 1000.0
                    );
                }
                _ => {}
            }
        }
    });

    let cancel = CancellationToken::new();
    let outcomes = run_swarm(&factory, &cfg_base, &swarm, candidates, task, tx, &cancel).await;
    let _ = printer.await;

    println!("\n=== results ===");
    for (i, o) in outcomes.iter().enumerate() {
        let color = COLORS[i % COLORS.len()];
        println!("\n{color}── {} ({})\x1b[0m", o.candidate.name, o.candidate.model);
        if let Some(err) = &o.error {
            println!("  ERROR: {err}");
        }
        if !o.summary.is_empty() {
            let s: String = o.summary.chars().take(400).collect();
            println!("  says: {s}");
        }
        match &o.patch_path {
            Some(p) => println!("  changes:\n{}\n  patch: {}", indent(&o.diff_stat, "    "), p.display()),
            None => println!("  changes: {}", o.diff_stat),
        }
    }

    if let Some(judge_model) = judge {
        println!("\n=== judge ({judge_model}) ===");
        match rift_core::judge_swarm(&factory, &judge_model, cfg_base.num_ctx, task, &outcomes).await {
            Ok(v) => {
                println!("{}", v.text.trim());
                // Machine-parseable verdict line (the judge bench keys on it).
                println!("JUDGE: winner={}", v.winner.as_deref().unwrap_or("none"));
            }
            Err(e) => println!("judge failed: {e:#}"),
        }
    }

    println!("\napply a winner with: rift merge <name> [--cleanup]   (worktrees kept under .rift/worktrees/)");
    Ok(())
}

/// Startup y/N prompt for an untrusted project-config MCP server. Runs before
/// the TUI takes over the terminal, so plain stdin is fine. Any error or
/// non-"y" answer counts as "no".
fn confirm_mcp(name: &str, cmdline: &str) -> bool {
    use std::io::Write;
    eprint!("project .rift.json defines MCP server '{name}': `{cmdline}`\nrun it now and trust it on this machine? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "YES")
}

fn indent(s: &str, pad: &str) -> String {
    s.lines().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n")
}

/// One-shot mode: prints the event stream to stdout. Used for scripting and
/// as the harness entry point for benchmarks.
async fn run_headless(
    mut agent: Agent,
    prompt: String,
    store: SessionStore,
    rates: Option<(f64, f64)>,
) -> Result<()> {
    use std::io::Write;
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let printer = tokio::spawn(async move {
        let mut in_thinking = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Iteration(i) => {
                    if i > 1 {
                        println!();
                    }
                }
                AgentEvent::Thinking(t) => {
                    if !in_thinking {
                        eprint!("\x1b[2m[thinking] ");
                        in_thinking = true;
                    }
                    eprint!("{t}");
                }
                AgentEvent::Content(c) => {
                    if in_thinking {
                        eprintln!("\x1b[0m");
                        in_thinking = false;
                    }
                    print!("{c}");
                    // print! buffers until a newline — flush so streamed
                    // paragraphs appear as they arrive instead of at the end.
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::ToolStart { name, args } => {
                    if in_thinking {
                        eprintln!("\x1b[0m");
                        in_thinking = false;
                    }
                    println!("\n\x1b[36m→ {name} {args}\x1b[0m");
                }
                AgentEvent::ToolResult { name, ok, preview } => {
                    let mark = if ok { "✓" } else { "✗" };
                    println!("\x1b[36m{mark} {name}\x1b[0m {preview}");
                }
                AgentEvent::Info(i) => eprintln!("\x1b[2m· {i}\x1b[0m"),
                AgentEvent::Plan(items) => {
                    for (i, item) in items.iter().enumerate() {
                        eprintln!("\x1b[2m  {} {}. {}\x1b[0m", if item.done { "[x]" } else { "[ ]" }, i + 1, item.text);
                    }
                }
                AgentEvent::Warning(w) => eprintln!("\n\x1b[33m! {w}\x1b[0m"),
                AgentEvent::TaskStarted { id, label } => {
                    eprintln!("\x1b[2m⚙ background task #{id} started: {label}\x1b[0m");
                }
                AgentEvent::TaskFinished { id, label, ok, .. } => {
                    let mark = if ok { "✓" } else { "✗" };
                    eprintln!("\x1b[2m⚙ background task #{id} {mark} finished: {label}\x1b[0m");
                }
                AgentEvent::Done(stats) => {
                    if in_thinking {
                        eprintln!("\x1b[0m");
                        in_thinking = false;
                    }
                    let recoveries = match stats.failures.model_failures() {
                        0 => String::new(),
                        n => format!(", {n} recoveries"),
                    };
                    let cost = rates
                        .map(|r| {
                            format!(
                                ", est. {}",
                                pricing::format_cost(pricing::cost(
                                    stats.billed_prompt_tokens,
                                    stats.output_tokens,
                                    r
                                ))
                            )
                        })
                        .unwrap_or_default();
                    println!(
                        "\n\x1b[2m[{} iterations, {} prompt tok, {} out tok, {:.1} tok/s, {:.1}s{recoveries}{cost}]\x1b[0m",
                        stats.iterations,
                        stats.prompt_tokens,
                        stats.output_tokens,
                        stats.tokens_per_sec,
                        stats.duration_ms as f64 / 1000.0
                    );
                }
            }
        }
    });

    // Background-task events (start/finish) surface through the same
    // printer channel, even between/after turns.
    agent.ctx().bg().set_notify(tx.clone());

    let cancel = CancellationToken::new();
    agent.run_turn(&prompt, &tx, &cancel).await?;
    let cwd = std::env::current_dir()?.display().to_string();
    store.save(&agent.cfg.model, &cwd, &agent.messages)?;
    let still_running = agent.ctx().bg().running_count();
    if still_running > 0 {
        eprintln!(
            "\x1b[33m! {still_running} background task(s) still running are terminated at exit \
             (headless runs end with the turn; use the TUI for work that outlives one)\x1b[0m"
        );
    }
    // The registry holds a clone of tx — release it BEFORE waiting for the
    // printer, or the channel never closes and the process hangs forever
    // after printing its final line (the v1.0.0–v1.0.3 headless zombie bug).
    agent.ctx().bg().clear_notify();
    drop(tx);
    let _ = printer.await;
    Ok(())
}
