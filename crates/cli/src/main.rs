//! magpie-lite: one local gateway for every coding agent.

mod agents;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use magpie_gateway::{Config, Protocol, ProviderConfig, config_path};

#[derive(Parser)]
#[command(
    name = "magpie-lite",
    version,
    about = "Every agent's model, one gateway — lite"
)]
struct Cli {
    /// Use this config file instead of the platform default.
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the gateway.
    Serve {
        /// Port to listen on (overrides the config).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Manage upstream providers.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Point Claude Code at the gateway, on a model.
    Claude {
        /// Model as the gateway names it: `deepseek/deepseek-chat`.
        model: String,
    },
    /// Point Codex at the gateway, on a model.
    Codex {
        /// Model as the gateway names it: `deepseek/deepseek-chat`.
        model: String,
    },
    /// List the agents this build can wire, and whether they are installed.
    Ls,
    /// List the models the configured providers serve.
    Models,
}

#[derive(Subcommand)]
enum ProviderCommand {
    /// Add or update a provider.
    Add {
        /// Short id models route by: `deepseek/deepseek-chat`.
        id: String,
        /// Wire protocol the upstream speaks.
        #[arg(long, default_value = "chat")]
        protocol: Protocol,
        /// Base URL: chat/responses bases end in `/v1`; Anthropic bases are the root; Gemini bases end in `/v1beta`.
        #[arg(long)]
        base_url: String,
        /// API key.
        #[arg(long)]
        api_key: String,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
        /// Known model ids, for bare-name routing and `models`.
        #[arg(long, num_args = 1..)]
        models: Vec<String>,
        /// Fallback routes, tried in order when this provider is rate limited, out of quota, or down.
        #[arg(long = "fallback")]
        fallback: Vec<String>,
        /// Make this the provider for models that name none.
        #[arg(long)]
        default: bool,
    },
    /// List configured providers.
    List,
    /// Remove a provider.
    Rm {
        /// Provider id.
        id: String,
    },
}

fn load_config(path: &Option<std::path::PathBuf>) -> Result<Config> {
    let path = match path {
        Some(p) => p.clone(),
        None => config_path()?,
    };
    Config::load(&path).with_context(|| format!("loading {}", path.display()))
}

fn provider_path(cli: &Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match cli {
        Some(p) => Ok(p.clone()),
        None => config_path(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { port } => {
            let cfg = load_config(&cli.config)?;
            magpie_gateway::serve(cfg, port).await?;
        }
        Command::Provider { command } => match command {
            ProviderCommand::Add {
                id,
                protocol,
                base_url,
                api_key,
                name,
                models,
                fallback,
                default,
            } => {
                let path = provider_path(&cli.config)?;
                let mut cfg = Config::load(&path)?;
                let provider = ProviderConfig {
                    id: id.clone(),
                    name: name.unwrap_or_default(),
                    protocol,
                    base_url,
                    api_key,
                    models,
                    fallback,
                };
                match cfg.provider.iter().position(|p| p.id == id) {
                    Some(i) => cfg.provider[i] = provider,
                    None => cfg.provider.push(provider),
                }
                if default || cfg.default_provider.is_empty() {
                    cfg.default_provider = id.clone();
                }
                cfg.save(&path)?;
                println!("saved provider `{}` to {}", id, path.display());
            }
            ProviderCommand::List => {
                let cfg = load_config(&cli.config)?;
                if cfg.provider.is_empty() {
                    println!(
                        "no providers; add one: magpie-lite provider add <id> --base-url ... --api-key ..."
                    );
                    return Ok(());
                }
                for p in &cfg.provider {
                    let marker = if p.id == cfg.default_provider {
                        " (default)"
                    } else {
                        ""
                    };
                    println!(
                        "{:<14} {:<10} {}{}",
                        p.id,
                        p.protocol.as_str(),
                        p.base_url,
                        marker
                    );
                    for m in &p.models {
                        println!("               {}/{}", p.id, m);
                    }
                    for route in &p.fallback {
                        println!("               ↳ fallback {route}");
                    }
                }
            }
            ProviderCommand::Rm { id } => {
                let path = provider_path(&cli.config)?;
                let mut cfg = Config::load(&path)?;
                let before = cfg.provider.len();
                cfg.provider.retain(|p| p.id != id);
                if cfg.provider.len() == before {
                    anyhow::bail!("no provider `{id}`");
                }
                if cfg.default_provider == id {
                    cfg.default_provider = cfg
                        .provider
                        .first()
                        .map(|p| p.id.clone())
                        .unwrap_or_default();
                }
                cfg.save(&path)?;
                println!("removed provider `{id}`");
            }
        },
        Command::Claude { model } => {
            let cfg = load_config(&cli.config)?;
            let path = agents::claude::wire(&model, cfg.port, &cfg.token)?;
            println!("Claude Code wired onto {model} ({})", path.display());
            println!("restart Claude Code for the change to take effect");
        }
        Command::Codex { model } => {
            let cfg = load_config(&cli.config)?;
            let path = agents::codex::wire(&model, cfg.port, &cfg.token)?;
            println!("Codex wired onto {model} ({})", path.display());
            println!("restart Codex for the change to take effect");
        }
        Command::Ls => {
            for a in agents::list() {
                let state = if a.installed {
                    "installed"
                } else {
                    "not found"
                };
                println!("{:<8} {:<12} {}", a.id, a.name, state);
            }
        }
        Command::Models => {
            let cfg = load_config(&cli.config)?;
            let catalog = cfg.catalog();
            if catalog.is_empty() {
                println!(
                    "no models configured; add providers with --models, or name provider/model directly"
                );
                return Ok(());
            }
            for (id, owner) in catalog {
                println!("{:<40} {}", id, owner);
            }
        }
    }
    Ok(())
}
