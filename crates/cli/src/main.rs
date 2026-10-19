//! magpie-lite: one local gateway for every coding agent.

use magpie_agents::lifecycle::{Action, AgentId, Gateway, Manager, state_directory};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use magpie_gateway::{
    Affinity, Config, GroupConfig, Protocol, ProviderConfig, RoutingPolicy, config_path,
};

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
    /// Preview, connect, disconnect, and recover agent configuration.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
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
enum AgentCommand {
    /// Show detection, managed connection, drift, and recovery status as JSON.
    List,
    /// Preview an operation without touching agent files. The output never contains keys.
    Preview {
        agent: AgentId,
        #[arg(required_unless_present = "disconnect", conflicts_with = "disconnect")]
        model: Option<String>,
        /// Preview restoration instead of a connection.
        #[arg(long)]
        disconnect: bool,
    },
    /// Connect with a durable backup; optionally require a previously reviewed revision.
    Connect {
        agent: AgentId,
        model: String,
        #[arg(long)]
        revision: Option<String>,
    },
    /// Restore the pre-connection files; refuses to overwrite subsequent user edits.
    Disconnect {
        agent: AgentId,
        #[arg(long)]
        revision: Option<String>,
    },
    /// Roll back an interrupted connect, or finish an interrupted disconnect.
    Recover { agent: AgentId },
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
        /// Route policy: order, rotate, usage, or weight.
        #[arg(long, default_value = "order")]
        routing: RoutingPolicy,
        /// Per-route weight for `--routing weight`, in route order; repeat for each route.
        #[arg(long = "route-weight", num_args = 1)]
        route_weights: Vec<u32>,
        /// First-content budget (ms) for streaming non-final routes; 0 disables.
        #[arg(long, default_value_t = 0)]
        first_token_timeout_ms: u64,
        /// How strongly a conversation stays with the key that answered it: auto, session, turn, or off.
        #[arg(long, default_value = "auto")]
        affinity: Affinity,
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
        #[arg(long = "api-key", num_args = 1)]
        api_keys: Vec<String>,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
        /// Known model ids, for bare-name routing and `models`.
        #[arg(long, num_args = 1..)]
        models: Vec<String>,
        /// Known context window in tokens, as model=window; repeat for each
        /// model. A group member whose window cannot hold a request is not
        /// asked it.
        #[arg(long = "context", value_parser = parse_context)]
        contexts: Vec<(String, u64)>,
        /// Fallback routes, tried in order when this provider is rate limited, out of quota, or down.
        #[arg(long = "fallback")]
        fallback: Vec<String>,
        /// Cooldown in milliseconds after a key is quota-limited or rate limited.
        #[arg(long)]
        key_cooldown_ms: Option<u64>,
        /// How requests spread over the key pool: rotate (the default), order, usage, or weight.
        #[arg(long, default_value = "rotate")]
        routing: RoutingPolicy,
        /// Per-key weight for `--routing weight`, in pool order; repeat for each key.
        #[arg(long = "key-weight", num_args = 1)]
        key_weights: Vec<u32>,
        /// How strongly a conversation stays with the key that answered it: auto, session, turn, or off.
        #[arg(long, default_value = "auto")]
        affinity: Affinity,
        /// Requests out at the vendor at once on each key; one more queues in turn.
        #[arg(long)]
        max_concurrency: Option<u32>,
        /// Requests one key may send in a rolling minute.
        #[arg(long)]
        max_rpm: Option<u32>,
        /// How many requests may wait in one key's queue; one more is a 429.
        #[arg(long)]
        queue_limit: Option<u32>,
        /// Seconds a request may wait in a key's queue or for room in its minute.
        #[arg(long)]
        queue_wait_s: Option<u32>,
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

fn save_config(path: &std::path::Path, config: &Config) -> Result<()> {
    let revision = if path.exists() {
        Config::persisted_revision(path)?
            .checked_add(1)
            .context("configuration revision exhausted")?
    } else {
        1
    };
    config.save_with_revision(path, revision)?;
    Ok(())
}

fn provider_path(cli: &Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match cli {
        Some(p) => Ok(p.clone()),
        None => config_path(),
    }
}

/// Parse `model=window`, the shape `provider add --context` takes.
fn parse_context(value: &str) -> Result<(String, u64)> {
    let (model, window) = value
        .rsplit_once('=')
        .context("--context takes model=window, for example deepseek-chat=131072")?;
    let window: u64 = window
        .parse()
        .context("--context window must be a whole number of tokens")?;
    if model.is_empty() || model.chars().any(char::is_whitespace) || window == 0 {
        anyhow::bail!(
            "--context needs a non-empty model id with no whitespace and a window above zero"
        );
    }
    Ok((model.to_owned(), window))
}

fn agent_manager(path: &Option<std::path::PathBuf>) -> Result<Manager> {
    Ok(Manager::native(state_directory(&provider_path(path)?)?)?)
}
fn agent_config(
    path: &Option<std::path::PathBuf>,
    action: Action,
    model: Option<&str>,
) -> Result<Config> {
    // Restoring/recovering must not depend on a still-valid gateway config.
    if action == Action::Disconnect {
        return Ok(Config::default());
    }
    let config = load_config(path)?;
    if model.is_none_or(|model| config.resolve(model).is_none()) {
        anyhow::bail!("model does not resolve to a configured provider or routing group");
    }
    Ok(config)
}
fn agent_apply(
    path: &Option<std::path::PathBuf>,
    agent: AgentId,
    action: Action,
    model: Option<&str>,
    revision: Option<&str>,
) -> Result<()> {
    let manager = agent_manager(path)?;
    let config = agent_config(path, action, model)?;
    let gateway = || Gateway {
        port: config.port,
        token: &config.token,
    };
    let preview = manager.preview(agent, action, model, gateway())?;
    let revision = revision.unwrap_or(&preview.revision);
    let status = manager.apply(agent, action, model, revision, gateway())?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
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
                contexts,
                fallback,
                key_cooldown_ms,
                routing,
                key_weights,
                affinity,
                max_concurrency,
                max_rpm,
                queue_limit,
                queue_wait_s,
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
                    contexts: contexts.into_iter().collect(),
                    fallback,
                    key_cooldown_ms,
                    routing,
                    weights: key_weights,
                    capabilities: Default::default(),
                    affinity,
                    max_concurrency,
                    max_rpm,
                    queue_limit,
                    queue_wait_s,
                    key_generation: 0,
                };
                cfg.set_provider(provider, default)?;
                save_config(&path, &cfg)?;
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
                        "{:<14} {:<10} {}{} [{} key{}, cooldown {}s, routing {}, affinity {}]",
                        p.id,
                        p.protocol.as_str(),
                        p.base_url,
                        marker,
                        key_count,
                        if key_count == 1 { "" } else { "s" },
                        cooldown.as_secs_f32(),
                        p.routing.as_str(),
                        p.affinity.as_str()
                    );
                    for m in &p.models {
                        println!("               {}/{}", p.id, m);
                        if let Some(window) = p.contexts.get(m) {
                            println!("               ↳ window {window} tokens");
                        }
                    }
                    for route in &p.fallback {
                        println!("               ↳ fallback {route}");
                    }
                }
            }
            ProviderCommand::Rm { id } => {
                let path = provider_path(&cli.config)?;
                let mut cfg = Config::load(&path)?;
                cfg.remove_provider(&id)?;
                save_config(&path, &cfg)?;
                println!("removed provider `{id}`");
            }
        },
        Command::Group { command } => match command {
            GroupCommand::Add {
                id,
                name,
                routes,
                routing,
                route_weights,
                first_token_timeout_ms,
                affinity,
            } => {
                let path = provider_path(&cli.config)?;
                let mut cfg = Config::load(&path)?;
                let group = GroupConfig {
                    id: id.clone(),
                    name: name.unwrap_or_default(),
                    routes,
                    weights: route_weights,
                    routing,
                    first_token_timeout_ms,
                    affinity,
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
                save_config(&path, &cfg)?;
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
                    println!(
                        "group/{:<18} {} [routing: {}, weights: {:?}, affinity: {}, first-token: {} ms]",
                        g.id,
                        label,
                        g.routing.as_str(),
                        g.route_weights(),
                        g.affinity.as_str(),
                        g.first_token_timeout_ms
                    );
                    for (i, route) in g.routes.iter().enumerate() {
                        let marker = if i == 0 { "route" } else { "fallback" };
                        let weight = g.weights.get(i).copied().unwrap_or(1);
                        println!("{:<26} {marker} {route} (weight: {weight})", "");
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
                save_config(&path, &cfg)?;
                println!("removed group `{id}`");
            }
        },
        Command::Agent { command } => match command {
            AgentCommand::List => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&agent_manager(&cli.config)?.statuses()?)?
                );
            }
            AgentCommand::Preview {
                agent,
                model,
                disconnect,
            } => {
                let action = if disconnect {
                    Action::Disconnect
                } else {
                    Action::Connect
                };
                let manager = agent_manager(&cli.config)?;
                let config = agent_config(&cli.config, action, model.as_deref())?;
                let preview = manager.preview(
                    agent,
                    action,
                    model.as_deref(),
                    Gateway {
                        port: config.port,
                        token: &config.token,
                    },
                )?;
                println!("{}", serde_json::to_string_pretty(&preview)?);
            }
            AgentCommand::Connect {
                agent,
                model,
                revision,
            } => {
                agent_apply(
                    &cli.config,
                    agent,
                    Action::Connect,
                    Some(&model),
                    revision.as_deref(),
                )?;
            }
            AgentCommand::Disconnect { agent, revision } => {
                agent_apply(
                    &cli.config,
                    agent,
                    Action::Disconnect,
                    None,
                    revision.as_deref(),
                )?;
            }
            AgentCommand::Recover { agent } => {
                let status = agent_manager(&cli.config)?.recover(agent)?;
                println!("{}", serde_json::to_string_pretty(&status)?);
            }
        },
        Command::Claude { model } => agent_apply(
            &cli.config,
            AgentId::Claude,
            Action::Connect,
            Some(&model),
            None,
        )?,
        Command::Codex { model } => agent_apply(
            &cli.config,
            AgentId::Codex,
            Action::Connect,
            Some(&model),
            None,
        )?,
        Command::Crush { model } => agent_apply(
            &cli.config,
            AgentId::Crush,
            Action::Connect,
            Some(&model),
            None,
        )?,
        Command::Gemini { model } => agent_apply(
            &cli.config,
            AgentId::Gemini,
            Action::Connect,
            Some(&model),
            None,
        )?,
        Command::Goose { model } => agent_apply(
            &cli.config,
            AgentId::Goose,
            Action::Connect,
            Some(&model),
            None,
        )?,
        Command::OpenCode { model } => agent_apply(
            &cli.config,
            AgentId::Opencode,
            Action::Connect,
            Some(&model),
            None,
        )?,
        Command::Zed { model } => agent_apply(
            &cli.config,
            AgentId::Zed,
            Action::Connect,
            Some(&model),
            None,
        )?,
        Command::Ls => {
            for agent in agent_manager(&cli.config)?.statuses()? {
                println!(
                    "{:<8} {:<12} detected={} connected={} drifted={} recovery={}",
                    agent.id.id(),
                    agent.name,
                    agent.detected,
                    agent.connected,
                    agent.drifted,
                    agent.recovery_needed
                );
                println!(
                    "         config_file={} config_directory={}",
                    agent.detection.config_file, agent.detection.config_directory
                );
                if let Some(cli) = agent.detection.cli {
                    println!(
                        "         CLI: {} ({})",
                        cli.path.display(),
                        match cli.on_path {
                            Some(true) => "on launcher's PATH",
                            Some(false) => "not on launcher's PATH",
                            None => "PATH membership unknown; search limited",
                        }
                    );
                } else {
                    println!("         CLI: not found (not a running-agent check)");
                }
                if agent.detection.search_limited {
                    println!(
                        "         CLI search reached its entry limit; an installation may be omitted"
                    );
                }
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
