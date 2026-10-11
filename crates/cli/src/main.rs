//! magpie-lite: one local gateway for every coding agent.

mod agents;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use magpie_gateway::{Config, GroupConfig, Protocol, ProviderConfig, config_path};

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
        /// Optional JSONL file for sanitized request history (overrides the config).
        #[arg(long)]
        history_file: Option<std::path::PathBuf>,
    },
    /// Manage upstream providers.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Manage named route chains.
    Group {
        #[command(subcommand)]
        command: GroupCommand,
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
    /// Point Crush at the gateway, on a model.
    Crush {
        /// Model as the gateway names it: deepseek/deepseek-chat.
        model: String,
    },
    /// Point Gemini CLI at the gateway, on a model.
    Gemini {
        /// Model as the gateway names it: `deepseek/deepseek-chat`.
        model: String,
    },
    /// Point Goose at the gateway, on a model.
    Goose {
        /// Model as the gateway names it: `deepseek/deepseek-chat`.
        model: String,
    },
    /// Point OpenCode at the gateway, on a model.
    #[command(name = "opencode")]
    OpenCode {
        /// Model as the gateway names it: `deepseek/deepseek-chat`.
        model: String,
    },
    /// Point Zed at the gateway, on a model.
    Zed {
        /// Model as the gateway names it: `deepseek/deepseek-chat`.
        model: String,
    },
    /// List the agents this build can wire, and whether they are installed.
    Ls,
    /// Show request and usage statistics from the running gateway.
    Stats,
    /// List the models the configured providers serve.
    Models,
}

#[derive(Subcommand)]
enum GroupCommand {
    /// Add or update a named route chain.
    Add {
        /// Group id; agents pick the chain as `group/<id>`.
        id: String,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
        /// Ordered routes: the first available model serves the request.
        #[arg(long = "route")]
        routes: Vec<String>,
    },
    /// List configured groups.
    List,
    /// Remove a group.
    Rm {
        /// Group id.
        id: String,
    },
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
        /// API keys; repeat --api-key to build a rotating pool.
        #[arg(long = "api-key", required = true, num_args = 1)]
        api_keys: Vec<String>,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
        /// Known model ids, for bare-name routing and `models`.
        #[arg(long, num_args = 1..)]
        models: Vec<String>,
        /// Fallback routes, tried in order when this provider is rate limited, out of quota, or down.
        #[arg(long = "fallback")]
        fallback: Vec<String>,
        /// Cooldown in milliseconds after a key is quota-limited or rate limited.
        #[arg(long)]
        key_cooldown_ms: Option<u64>,
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
        Command::Serve { port, history_file } => {
            let path = provider_path(&cli.config)?;
            let cfg = Config::load(&path)?;
            magpie_gateway::serve_with_config_path(cfg, port, Some(path), history_file).await?;
        }
        Command::Provider { command } => match command {
            ProviderCommand::Add {
                id,
                protocol,
                base_url,
                api_keys,
                name,
                models,
                fallback,
                key_cooldown_ms,
                default,
            } => {
                if id == "group" {
                    anyhow::bail!("provider id `group` is reserved for `group/<id>` routes");
                }
                let path = provider_path(&cli.config)?;
                let mut cfg = Config::load(&path)?;
                let provider = ProviderConfig {
                    id: id.clone(),
                    name: name.unwrap_or_default(),
                    protocol,
                    base_url,
                    api_key: String::new(),
                    api_keys,
                    models,
                    fallback,
                    key_cooldown_ms,
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
                    let key_count = p.api_key_pool().len();
                    let cooldown = cfg.key_cooldown_for(p);
                    println!(
                        "{:<14} {:<10} {}{} [{} key{}, cooldown {}s]",
                        p.id,
                        p.protocol.as_str(),
                        p.base_url,
                        marker,
                        key_count,
                        if key_count == 1 { "" } else { "s" },
                        cooldown.as_secs_f32()
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
        Command::Group { command } => match command {
            GroupCommand::Add { id, name, routes } => {
                let path = provider_path(&cli.config)?;
                let mut cfg = Config::load(&path)?;
                let group = GroupConfig {
                    id: id.clone(),
                    name: name.unwrap_or_default(),
                    routes,
                };
                group.validate()?;
                for route in &group.routes {
                    let provider = route.split_once('/').unwrap().0;
                    if cfg.provider(provider).is_none() {
                        anyhow::bail!("unknown provider `{provider}`");
                    }
                }
                match cfg.group.iter().position(|g| g.id == id) {
                    Some(i) => cfg.group[i] = group,
                    None => cfg.group.push(group),
                }
                cfg.save(&path)?;
                println!("saved group `group/{id}` to {}", path.display());
            }
            GroupCommand::List => {
                let cfg = load_config(&cli.config)?;
                if cfg.group.is_empty() {
                    println!(
                        "no groups; add one: magpie-lite group add <id> --route provider/model"
                    );
                    return Ok(());
                }
                for g in &cfg.group {
                    let label = if g.name.is_empty() {
                        g.id.as_str()
                    } else {
                        g.name.as_str()
                    };
                    println!("group/{:<18} {}", g.id, label);
                    for (i, route) in g.routes.iter().enumerate() {
                        let marker = if i == 0 { "route" } else { "fallback" };
                        println!("{:<26} {marker} {route}", "");
                    }
                }
            }
            GroupCommand::Rm { id } => {
                let path = provider_path(&cli.config)?;
                let mut cfg = Config::load(&path)?;
                let before = cfg.group.len();
                cfg.group.retain(|g| g.id != id);
                if cfg.group.len() == before {
                    anyhow::bail!("no group `{id}`");
                }
                cfg.save(&path)?;
                println!("removed group `{id}`");
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
        Command::Crush { model } => {
            let cfg = load_config(&cli.config)?;
            let path = agents::crush::wire(&model, cfg.port, &cfg.token)?;
            println!("Crush wired onto {model} ({})", path.display());
            println!("restart Crush for the provider and model changes to take effect");
        }
        Command::Gemini { model } => {
            let cfg = load_config(&cli.config)?;
            let path = agents::gemini::wire(&model, cfg.port, &cfg.token)?;
            println!("Gemini CLI wired onto {model} ({})", path.display());
            println!("restart Gemini CLI for the change to take effect");
        }
        Command::Goose { model } => {
            let cfg = load_config(&cli.config)?;
            let path = agents::goose::wire(&model, cfg.port, &cfg.token)?;
            println!("Goose wired onto {model} ({})", path.display());
            println!("restart Goose for the provider and model changes to take effect");
        }
        Command::OpenCode { model } => {
            let cfg = load_config(&cli.config)?;
            let path = agents::opencode::wire(&model, cfg.port, &cfg.token)?;
            println!("OpenCode wired onto {model} ({})", path.display());
            println!("restart OpenCode for the change to take effect");
        }
        Command::Zed { model } => {
            let cfg = load_config(&cli.config)?;
            let path = agents::zed::wire(&model, cfg.port, &cfg.token)?;
            println!("Zed wired onto {model} ({})", path.display());
            println!(
                "set {} to the gateway token before launching Zed, or enter it in Zed's AI provider settings",
                agents::zed::API_KEY_ENV_VAR
            );
            println!("restart Zed for the change to take effect");
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
        Command::Stats => {
            let cfg = load_config(&cli.config)?;
            let url = format!("http://127.0.0.1:{}/stats", cfg.port);
            let response = magpie_gateway::upstream::http_client()?
                .get(&url)
                .bearer_auth(&cfg.token)
                .send()
                .await
                .with_context(|| format!("requesting {url}; is the gateway running?"))?;
            if !response.status().is_success() {
                anyhow::bail!("gateway returned {}", response.status());
            }
            let stats: serde_json::Value = response
                .json()
                .await
                .with_context(|| format!("reading response from {url}"))?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
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
