mod app;
mod clipboard;
mod commands;
mod swarm_ui;
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
use rift_ollama::{OllamaClient, Provider};
use rift_openai::OpenAiClient;

/// Build a provider (and the actual model name) from a possibly-prefixed model
/// string. `openrouter/qwen3` routes through the configured `openrouter`
/// provider with model `qwen3`; any other model uses the default Ollama server.
pub(crate) fn build_provider(
    model: &str,
    host: &str,
    providers: &HashMap<String, ProviderConfig>,
) -> (Arc<dyn Provider>, String) {
    if let Some((name, rest)) = model.split_once('/') {
        if let Some(pc) = providers.get(name) {
            return (Arc::new(OpenAiClient::new(&pc.base_url, pc.resolve_key())), rest.to_string());
        }
    }
    (Arc::new(OllamaClient::new(host)), model.to_string())
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
    let (config, config_path) = Config::load(&cwd)?;
    if let Some(p) = &config_path {
        eprintln!("config: {}", p.display());
    }
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
    let num_ctx = cli.num_ctx.or(config.num_ctx).unwrap_or(32_768);
    let temp = cli.temp.or(config.temperature).unwrap_or(0.2);
    let max_iterations = cli.max_iterations.or(config.max_iterations).unwrap_or(25);

    // A `provider/model` string routes through a configured provider; otherwise
    // the default Ollama server at `host`. `model` becomes the bare model name.
    let (client, model) = build_provider(&model, &host, &config.providers);

    // Subcommands bypass the single-model preflight (swarm preflights each
    // candidate itself; merge is git-only).
    match &cli.cmd {
        Some(Cmd::Swarm { task, models, explore, no_tui }) => {
            let cfg_base = AgentConfig {
                model: String::new(), // set per candidate
                num_ctx,
                temperature: Some(temp),
                max_iterations,
                think: None,
                always_task: true,
            };
            return run_swarm_cli(client, cfg_base, task, models, *explore, *no_tui).await;
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
    let num_ctx = match show.as_ref().and_then(|s| s.context_length()) {
        Some(max_ctx) => {
            if num_ctx > max_ctx {
                eprintln!("warning: num_ctx {num_ctx} exceeds model max {max_ctx}; using {max_ctx}");
            }
            num_ctx.min(max_ctx)
        }
        None => num_ctx,
    };

    let cfg = AgentConfig {
        model: model.clone(),
        num_ctx,
        temperature: Some(temp),
        max_iterations,
        think,
        always_task: cli.prompt.is_some(),
    };

    // MCP servers + permission policy come from the config loaded up top.
    let mut registry = ToolRegistry::standard();
    let mut mcp_status: Vec<(String, usize)> = vec![];
    for (name, server_cfg) in &config.mcp {
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
    let approve = cli.approve || config.permissions.approve;
    let mut ctx = ToolCtx::with_extra_deny(&cwd, &config.permissions.bash_deny).with_approval(approve);
    let (ask_tx, ask_rx) = mpsc::unbounded_channel::<AskRequest>();
    if interactive {
        ctx = ctx.with_interaction(ask_tx);
        registry.register(Box::new(AskUserTool));
        if approve {
            eprintln!("approval mode: write/edit/bash will ask before running");
        }
    } else if approve {
        eprintln!("note: approval mode needs the interactive TUI; running headless without it");
    }

    let (mut prompt_text, guide_files) = rift_core::system_prompt_with_guide(&cwd);
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

    let mut agent = Agent::new(client, cfg, registry, ctx, prompt_text);

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
            let saved = SessionStore::load(&path)?;
            // Keep the freshly composed system prompt (cwd may have changed).
            let mut messages = saved.messages;
            if messages.first().is_some_and(|m| m.role == rift_ollama::Role::System) {
                messages[0] = agent.messages[0].clone();
            }
            agent.messages = messages.clone();
            eprintln!("resumed session {} ({} messages)", path.display(), messages.len());
            (SessionStore::at(path), messages)
        }
        None => (SessionStore::create()?, vec![]),
    };

    match cli.prompt {
        Some(prompt) => run_headless(agent, prompt, store).await,
        None => {
            app::run_tui(
                agent,
                app::TuiOptions {
                    model,
                    store,
                    resumed: resumed_messages,
                    mcp: mcp_status,
                    config_path,
                    ask_rx,
                    skills,
                    host: host.clone(),
                    providers: config.providers.clone(),
                },
            )
            .await
        }
    }
}

/// `rift swarm`: race N models on one task in parallel worktrees. Interactive
/// TUI by default on a terminal; plain streaming with --no-tui (or piped).
async fn run_swarm_cli(
    client: Arc<dyn Provider>,
    cfg_base: AgentConfig,
    task: &str,
    models: &str,
    explore: bool,
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
        return swarm_ui::run_swarm_tui(client, cfg_base, swarm, candidates, task.to_string()).await;
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
    let outcomes = run_swarm(&client, &cfg_base, &swarm, candidates, task, tx, &cancel).await;
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
    println!("\napply a winner with: rift merge <name> [--cleanup]   (worktrees kept under .rift/worktrees/)");
    Ok(())
}

fn indent(s: &str, pad: &str) -> String {
    s.lines().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n")
}

/// One-shot mode: prints the event stream to stdout. Used for scripting and
/// as the harness entry point for benchmarks.
async fn run_headless(mut agent: Agent, prompt: String, store: SessionStore) -> Result<()> {
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
                AgentEvent::Done(stats) => {
                    if in_thinking {
                        eprintln!("\x1b[0m");
                        in_thinking = false;
                    }
                    println!(
                        "\n\x1b[2m[{} iterations, {} prompt tok, {} out tok, {:.1} tok/s, {:.1}s]\x1b[0m",
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

    let cancel = CancellationToken::new();
    agent.run_turn(&prompt, &tx, &cancel).await?;
    let cwd = std::env::current_dir()?.display().to_string();
    store.save(&agent.cfg.model, &cwd, &agent.messages)?;
    drop(tx);
    let _ = printer.await;
    Ok(())
}
