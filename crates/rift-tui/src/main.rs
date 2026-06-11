mod app;
mod swarm_ui;

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use rift_core::{
    run_swarm, system_prompt, Agent, AgentConfig, AgentEvent, Candidate, Config, McpClient,
    McpTool, SessionStore, Swarm, ToolCtx, ToolRegistry,
};
use rift_ollama::OllamaClient;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(name = "rift", about = "Rift — a fast terminal coding agent for local Ollama models")]
struct Cli {
    /// Ollama server URL
    #[arg(long, env = "RIFT_HOST", default_value = "http://localhost:11434")]
    host: String,
    /// Model to use (must have the "tools" capability)
    #[arg(long, short, env = "RIFT_MODEL", default_value = "gemma4:26b")]
    model: String,
    /// Context window to request per call (options.num_ctx)
    #[arg(long, default_value_t = 32_768)]
    num_ctx: u64,
    /// Run a single prompt headless (no TUI) and print the transcript
    #[arg(long, short)]
    prompt: Option<String>,
    /// Max agent-loop iterations per turn
    #[arg(long, default_value_t = 25)]
    max_iterations: usize,
    /// Resume the most recent session
    #[arg(long = "continue", short = 'c')]
    cont: bool,
    /// Resume a specific session file
    #[arg(long)]
    resume: Option<PathBuf>,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = OllamaClient::new(&cli.host);

    // Subcommands bypass the single-model preflight (swarm preflights each
    // candidate itself; merge is git-only).
    match &cli.cmd {
        Some(Cmd::Swarm { task, models, explore, no_tui }) => {
            return run_swarm_cli(client, &cli, task, models, *explore, *no_tui).await;
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
        None => {}
    }

    // Preflight: verify the server is reachable and the model supports tools,
    // so failures are loud and early instead of mid-conversation.
    let show = match client.show(&cli.model).await {
        Ok(s) => s,
        Err(e) => bail!("cannot reach Ollama at {} or model '{}' missing: {e}", cli.host, cli.model),
    };
    if !show.supports("tools") {
        bail!("model '{}' does not have the 'tools' capability; pick a tools-capable model", cli.model);
    }
    let think = if show.supports("thinking") { None } else { Some(false) };
    if let Some(max_ctx) = show.context_length() {
        if cli.num_ctx > max_ctx {
            eprintln!("warning: --num-ctx {} exceeds model max {max_ctx}; using {max_ctx}", cli.num_ctx);
        }
    }
    let num_ctx = show.context_length().map_or(cli.num_ctx, |m| m.min(cli.num_ctx));

    let cwd = std::env::current_dir()?;
    let cfg = AgentConfig {
        model: cli.model.clone(),
        num_ctx,
        temperature: None,
        max_iterations: cli.max_iterations,
        think,
    };

    // Config: MCP servers + permission policy.
    let (config, config_path) = Config::load(&cwd)?;
    if let Some(p) = &config_path {
        eprintln!("config: {}", p.display());
    }
    let mut registry = ToolRegistry::standard();
    for (name, server_cfg) in &config.mcp {
        match McpClient::spawn(name, server_cfg).await {
            Ok(mcp) => match mcp.list_tools().await {
                Ok(tools) => {
                    let count = tools.len();
                    for info in tools {
                        registry.register(Box::new(McpTool::new(mcp.clone(), info)));
                    }
                    eprintln!("mcp '{name}': {count} tool(s) registered");
                }
                Err(e) => eprintln!("warning: mcp '{name}' tools/list failed: {e:#}"),
            },
            Err(e) => eprintln!("warning: mcp '{name}' failed to start: {e:#}"),
        }
    }

    let mut agent = Agent::new(
        client,
        cfg,
        registry,
        ToolCtx::with_extra_deny(&cwd, &config.permissions.bash_deny),
        system_prompt(&cwd.display().to_string()),
    );

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
        None => app::run_tui(agent, cli.model, store, resumed_messages).await,
    }
}

/// `gw swarm`: race N models on one task in parallel worktrees. Interactive
/// TUI by default on a terminal; plain streaming with --no-tui (or piped).
async fn run_swarm_cli(
    client: OllamaClient,
    cli: &Cli,
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

    let cfg_base = AgentConfig {
        model: String::new(), // set per candidate
        num_ctx: cli.num_ctx,
        temperature: None,
        max_iterations: cli.max_iterations,
        think: None,
    };

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
    println!("\napply a winner with: gw merge <name> [--cleanup]   (worktrees kept under .rift/worktrees/)");
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
