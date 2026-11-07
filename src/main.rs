//! Entry point: launches the TUI by default, or one of the machine-facing subcommands (A2A server, MCP bridge, custom agent).
mod a2a;
mod acp_client;
mod app;
mod config;
mod custom_agent;
mod event;
mod keys;
mod llm;
mod mcp_server;
mod pty_pane;
mod rpc;
mod terminal_agents;
mod tui;
mod ui;
mod worker;

use acp_client::{Client, reject_all};
use anyhow::{Context, Result, bail};
use app::{App, Env};
use clap::{Parser, Subcommand, ValueEnum};
use config::{AgentSpec, Config};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};

#[derive(Parser)]
#[command(about = "TUI for running coding agents over ACP; agents delegate to each other over A2A")]
struct Cli {
    /// Config file with custom agents [default: ~/.config/central/config.json]
    #[arg(long, global = true, env = "CENTRAL_CONFIG")]
    config: Option<PathBuf>,
    /// Start the TUI with this agent's past sessions listed, like `claude --resume`.
    #[arg(long, value_name = "AGENT")]
    resume: Option<String>,
    /// Without a subcommand the TUI starts.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List available agents.
    Agents,
    /// List an agent's past sessions for the current directory.
    Sessions { agent: String },
    /// Start an agent and open a session without prompting, to verify it works.
    Check { agent: String },
    /// Register a custom agent backed by an Ollama or OpenAI-compatible model.
    Add {
        name: String,
        #[arg(long, value_enum)]
        kind: CustomKind,
        #[arg(long)]
        model: String,
        /// Endpoint root [default: http://localhost:11434 for ollama, https://api.openai.com/v1 for openai]
        #[arg(long)]
        base_url: Option<String>,
        /// Environment variable holding the API key (openai only)
        #[arg(long)]
        api_key_env: Option<String>,
        #[arg(long)]
        system: Option<String>,
    },
    /// Serve every agent over A2A without the TUI.
    A2aServe {
        /// Grant every agent permission request automatically (there is no UI to ask).
        #[arg(long)]
        auto_approve: bool,
        /// Bearer token clients must send [default: random, printed to stderr]
        #[arg(long, env = mcp_server::TOKEN_ENV)]
        token: Option<String>,
    },
    /// Run a custom agent as an ACP server on stdio (spawned by clients, not usually by hand).
    Serve { name: String },
    /// Run the MCP server that exposes delegate/generate_image (spawned by agents).
    Mcp {
        #[arg(long)]
        a2a_url: String,
        /// Name of the agent that launched this server, reported to the A2A server.
        #[arg(long)]
        caller: String,
        /// Agent that `generate_image` delegates to.
        #[arg(long, default_value = "codex")]
        image_agent: String,
        #[arg(long, env = mcp_server::TOKEN_ENV, hide_env_values = true)]
        token: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CustomKind {
    Ollama,
    Openai,
}

/// Parses arguments, loads the config and runs the chosen command.
fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load(cli.config)?;
    match cli.command {
        None => run_tui(config, cli.resume),
        Some(Command::Agents) => {
            for name in config.agent_names() {
                println!("{name}");
            }
            Ok(())
        }
        Some(Command::Sessions { agent }) => sessions(&config, &agent),
        Some(Command::Check { agent }) => check(&config, &agent),
        Some(Command::Add {
            name,
            kind,
            model,
            base_url,
            api_key_env,
            system,
        }) => {
            let spec = match kind {
                CustomKind::Ollama => AgentSpec::Ollama {
                    model,
                    base_url: base_url.unwrap_or_else(|| "http://localhost:11434".to_owned()),
                    system,
                },
                CustomKind::Openai => AgentSpec::Openai {
                    model,
                    base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_owned()),
                    api_key_env,
                    system,
                },
            };
            config.agents.insert(name.clone(), spec);
            config.save()?;
            println!("saved '{name}' to {}", config.path.display());
            Ok(())
        }
        Some(Command::A2aServe {
            auto_approve,
            token,
        }) => {
            let token = match token {
                Some(token) => token,
                None => {
                    let token = a2a::new_token()?;
                    eprintln!("bearer token: {token}");
                    token
                }
            };
            let url = a2a::start(
                Arc::new(config),
                None,
                Arc::new(AtomicBool::new(auto_approve)),
                token,
            )?;
            println!("listening on {url}");
            loop {
                std::thread::park();
            }
        }
        Some(Command::Serve { name }) => match config.resolve(&name)? {
            AgentSpec::Acp { .. } => bail!("'{name}' is an external ACP agent, not a custom one"),
            spec => custom_agent::serve(&spec),
        },
        Some(Command::Mcp {
            a2a_url,
            caller,
            image_agent,
            token,
        }) => {
            let target = a2a::Target {
                base_url: a2a_url,
                token,
                caller,
            };
            mcp_server::serve(&target, &image_agent)
        }
    }
}

/// Starts the A2A server in this process, then hands the terminal to the TUI.
/// With `resume`, that agent's past-session list opens immediately.
fn run_tui(config: Config, resume: Option<String>) -> Result<()> {
    let config = Arc::new(config);
    let (ui, events) = mpsc::channel();
    let auto_allow = Arc::new(AtomicBool::new(false));
    let token = a2a::new_token()?;
    let a2a_url = a2a::start(
        config.clone(),
        Some(ui.clone()),
        auto_allow.clone(),
        token.clone(),
    )?;
    let env = Env {
        config,
        exe: std::env::current_exe()?,
        cwd: std::env::current_dir()?,
        a2a_url,
        token,
        ui,
    };
    let mut app = App::new(env, auto_allow);
    if let Some(agent) = resume.filter(|agent| !app.start_resume(agent)) {
        bail!("unknown agent '{agent}'; run `central agents` to list them");
    }
    tui::run(&mut app, events)
}

/// Implements `central check`: spawns the agent and opens a session, then exits.
fn check(config: &Config, agent: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let cwd = std::env::current_dir()?;
    let launch = config.resolve(agent)?.launch(agent, &exe, &config.path);
    Client::start(
        &launch,
        &cwd,
        vec![],
        reject_all(),
        cwd.join("generated-images"),
        Box::new(|_| {}),
    )
    .with_context(|| format!("{agent}: ACP handshake failed"))?;
    println!("{agent}: ACP handshake ok");
    Ok(())
}

/// Implements `central sessions`: prints one line per past session (id, last update, title).
fn sessions(config: &Config, agent: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let cwd = std::env::current_dir()?;
    let launch = config.resolve(agent)?.launch(agent, &exe, &config.path);
    let mut client = Client::connect(
        &launch,
        &cwd,
        reject_all(),
        cwd.join("generated-images"),
        Box::new(|_| {}),
    )
    .with_context(|| format!("{agent}: ACP handshake failed"))?;
    for session in client.list_sessions()? {
        println!(
            "{}  {}  {}",
            session.id,
            session.updated_at.as_deref().unwrap_or("-"),
            session.title.as_deref().unwrap_or("(untitled)")
        );
    }
    Ok(())
}
