//! magpie-lite: one local gateway for every coding agent.

use magpie_agents::lifecycle::{Action, AgentId, Gateway, Manager, state_directory};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use magpie_gateway::{
    Affinity, Config, GroupConfig, Protocol, ProviderConfig, RoutingPolicy, Rule, config_path,
    presets,
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
        /// Ordered routes: the first available model serves the request. A
        /// route ending in `:level` (low, medium, high, ...) fixes that
        /// effort on the member: `mock/model:high` asks for high.
        #[arg(long = "route")]
        routes: Vec<String>,
        /// Route policy: order, rotate, usage, weight, or manual (which
        /// sends every request to `--pick`, defaulting to the first route).
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
        /// Seconds a busy group keeps re-asking its members as slots free;
        /// a negative disables re-asking, above 240 takes 240, unset takes 60.
        #[arg(long)]
        patience: Option<i64>,
        /// A routing rule as JSON, e.g. `{"use":"deepseek/deepseek-chat","images":true}`;
        /// repeat for each rule. Rules put their member first for the requests
        /// they match; a `"pause":true` rule leaves its member out instead. A
        /// `"useEffort":"low"` asks the member for that reasoning when the
        /// rule matches.
        #[arg(long = "rule", num_args = 1)]
        rules: Vec<String>,
        /// The model judging the rules' intents (`provider/model`); a rule
        /// with an intent needs it.
        #[arg(long, default_value = "")]
        classifier: String,
        /// How the turn's reasoning is picked: blank leaves what the agent
        /// asked for, `auto` has the group's classifier judge how hard the
        /// turn is as it begins.
        #[arg(long, default_value = "")]
        effort: String,
        /// Reasoning levels the group offers its members (`low`, `medium`,
        /// `high`, `xhigh`, `max`); every reasoning ask for a member is
        /// sent at the nearest offered, except off, which stays off.
        /// Repeat for each level; empty offers everything.
        #[arg(long = "level", num_args = 1)]
        levels: Vec<String>,
        /// The member a `manual` group sends every request to
        /// (`provider/model`); the others, and the rules, wait until
        /// another member is picked. A bare model id one member has
        /// names it too; setting a pick makes the group manual.
        #[arg(long, default_value = "")]
        pick: String,
    },
    /// List configured groups.
    List,
    /// Remove a group.
    Rm {
        /// Group id.
        id: String,
    },
    /// Point a group at another member: every request goes to it, so the
    /// group is manual from then on (the console re-picks the same way,
    /// on the group's card).
    Pick {
        /// Group id.
        id: String,
        /// The member: its route, or a bare model id one member has.
        member: String,
    },
    /// Say whether magpie finds groups on its own, or turn that on or
    /// off: off, none is listed or served, while the groups made or
    /// changed stay — and a request for a found one goes to its model
    /// from one provider.
    Auto {
        /// `on` or `off`; blank just says.
        state: Option<String>,
    },
}

#[derive(Subcommand)]
// One-shot CLI arg enum: the Add variant legitimately dwarfs the rest,
// and boxing it would churn the arg plumbing for a value built once per
// invocation.
#[allow(clippy::large_enum_variant)]
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
        /// Max reply tokens, as model=limit; repeat for each model. A
        /// request naming more is lowered to it on any route, and an
        /// unnamed limit headed for Anthropic's API is filled with it.
        #[arg(long = "output", value_parser = parse_output)]
        outputs: Vec<(String, u64)>,
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
        /// Per-key concurrency override in pool order (`--max-concurrency`
        /// when absent); repeat for each key, 0 for no limit on that key.
        #[arg(long = "key-limit", num_args = 1)]
        key_limits: Vec<u32>,
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

/// Parse `model=limit`, the shape `provider add --output` takes.
fn parse_output(value: &str) -> Result<(String, u64)> {
    let (model, limit) = value
        .rsplit_once('=')
        .context("--output takes model=limit, for example claude-sonnet-4-5=32000")?;
    let limit: u64 = limit
        .parse()
        .context("--output limit must be a whole number of tokens")?;
    if model.is_empty() || model.chars().any(char::is_whitespace) || limit == 0 {
        anyhow::bail!(
            "--output needs a non-empty model id with no whitespace and a limit above zero"
        );
    }
    Ok((model.to_owned(), limit))
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
                outputs,
                fallback,
                key_cooldown_ms,
                routing,
                key_weights,
                key_limits,
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
                    base_url: base_url.clone(),
                    api_key: String::new(),
                    api_keys,
                    models,
                    contexts: contexts.into_iter().collect(),
                    outputs: outputs.into_iter().collect(),
                    fallback,
                    key_cooldown_ms,
                    routing,
                    weights: key_weights,
                    key_limits,
                    // A known vendor's host prefills capability metadata;
                    // a stored provider's customized metadata is kept.
                    capabilities: presets::capabilities_for_add(
                        cfg.provider(&id).map(|p| &p.capabilities),
                        &base_url,
                        protocol,
                    ),
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
                    if let Some(summary) =
                        presets::describe_capabilities(&p.capabilities, p.protocol)
                    {
                        println!("               ↳ {summary}");
                    }
                    for m in &p.models {
                        println!("               {}/{}", p.id, m);
                        if let Some(window) = p.contexts.get(m) {
                            println!("               ↳ window {window} tokens");
                        }
                        if let Some(limit) = p.outputs.get(m) {
                            println!("               ↳ max output {limit} tokens");
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
        Command::Group { command } => {
            match command {
                GroupCommand::Add {
                    id,
                    name,
                    routes,
                    routing,
                    route_weights,
                    first_token_timeout_ms,
                    affinity,
                    patience,
                    rules,
                    classifier,
                    effort,
                    levels,
                    pick,
                } => {
                    let path = provider_path(&cli.config)?;
                    let mut cfg = Config::load(&path)?;
                    let mut parsed = Vec::with_capacity(rules.len());
                    for (index, raw) in rules.iter().enumerate() {
                        let rule: Rule = serde_json::from_str(raw).with_context(|| {
                        format!("rule {} is not a JSON object like {{\"use\":\"provider/model\",...}}", index + 1)
                    })?;
                        parsed.push(rule);
                    }
                    let mut group = GroupConfig {
                        id: id.clone(),
                        name: name.unwrap_or_default(),
                        routes,
                        rules: parsed,
                        classifier: classifier.trim().to_owned(),
                        effort: effort.trim().to_owned(),
                        levels,
                        pick: pick.trim().to_owned(),
                        auto: false,
                        weights: route_weights,
                        routing,
                        first_token_timeout_ms,
                        affinity,
                        patience_s: patience,
                    };
                    if !group.pick.is_empty() {
                        // A pick names its member, and makes the group
                        // manual (the reference's `pick` field, #317).
                        group.pick = group.pick_member(&group.pick.clone())?;
                        group.routing = RoutingPolicy::Manual;
                    }
                    cfg.clean_member_efforts(&mut group);
                    group.validate()?;
                    group.check_classifier(&cfg)?;
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
                    cfg.hidden_auto_groups.retain(|hidden| hidden != &id);
                    save_config(&path, &cfg)?;
                    println!("saved group `group/{id}` to {}", path.display());
                }
                GroupCommand::List => {
                    let cfg = load_config(&cli.config)?;
                    if cfg.no_auto_groups {
                        println!(
                            "found groups are off · only the groups you made or changed; `magpie-lite group auto on` brings the others back"
                        );
                    }
                    if cfg.group.is_empty() {
                        println!(
                            "no groups; add one: magpie-lite group add <id> --route provider/model"
                        );
                        return Ok(());
                    }
                    let auto = cfg.auto_groups();
                    for g in cfg.group.iter().chain(auto.iter()) {
                        let label = if g.name.is_empty() {
                            g.id.as_str()
                        } else {
                            g.name.as_str()
                        };
                        let patience = match g.patience_s {
                            Some(n) if n < 0 => "off".to_owned(),
                            _ => format!("{}s", g.patience().as_secs()),
                        };
                        println!(
                            "{:<24} {} [routing: {}, weights: {:?}, affinity: {}, first-token: {} ms, patience: {}]",
                            cfg.group_public_ref(g),
                            label,
                            g.routing.as_str(),
                            g.route_weights(),
                            g.affinity.as_str(),
                            g.first_token_timeout_ms,
                            patience
                        );
                        if g.auto {
                            println!(
                                "{:<26} found: one model on {} providers; `group rm {}` hides it",
                                "",
                                g.routes.len(),
                                cfg.group_public_ref(g),
                            );
                        }
                        for (i, route) in g.routes.iter().enumerate() {
                            let marker = if g.routing == RoutingPolicy::Manual {
                                if route == &g.pick { "pick" } else { "waiting" }
                            } else if i == 0 {
                                "route"
                            } else {
                                "fallback"
                            };
                            let weight = g.weights.get(i).copied().unwrap_or(1);
                            println!("{:<26} {marker} {route} (weight: {weight})", "");
                        }
                        if !g.classifier.is_empty() {
                            println!("{:<26} classifier {}", "", g.classifier);
                        }
                        if !g.pick.is_empty() {
                            println!("{:<26} pick {}", "", g.pick);
                        }
                        if !g.effort.is_empty() {
                            println!("{:<26} effort {}", "", g.effort);
                        }
                        for (i, rule) in g.rules.iter().enumerate() {
                            let kind = if rule.pause { "rule (pause)" } else { "rule" };
                            println!("{:<26} {kind} {}: {}", "", i + 1, rule.summary());
                        }
                    }
                    for g in cfg.hidden_found_groups() {
                        let bring = g
                            .routes
                            .iter()
                            .map(|route| format!("--route {route}"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        println!(
                            "{:<24} {} [found, hidden]",
                            cfg.group_public_ref(&g),
                            g.label()
                        );
                        println!(
                            "{:<26} bring back with: magpie-lite group add {} {bring}",
                            "", g.id
                        );
                    }
                }
                GroupCommand::Rm { id } => {
                    let path = provider_path(&cli.config)?;
                    let mut cfg = Config::load(&path)?;
                    // A found group is addressed by its public ref too.
                    let id = cfg.group_by_ref(&id).map(|g| g.id).unwrap_or(id);
                    let before = cfg.group.len();
                    cfg.group.retain(|g| g.id != id);
                    if cfg.group.len() != before {
                        save_config(&path, &cfg)?;
                        println!("removed group `{id}`");
                    } else if cfg.group(&id).is_some() {
                        // Found, never changed: hide it rather than deleting
                        // what the providers still derive.
                        if !cfg.hidden_auto_groups.iter().any(|hidden| hidden == &id) {
                            cfg.hidden_auto_groups.push(id.clone());
                        }
                        save_config(&path, &cfg)?;
                        println!("hid found group `{id}`; `group list` brings it back");
                    } else {
                        anyhow::bail!("no group `{id}`");
                    }
                }
                GroupCommand::Pick { id, member } => {
                    let path = provider_path(&cli.config)?;
                    let mut cfg = Config::load(&path)?;
                    // A found group is addressed by its public ref too.
                    let id = cfg.group_by_ref(&id).map(|g| g.id).unwrap_or(id);
                    let Some(index) = cfg.group.iter().position(|g| g.id == id) else {
                        anyhow::bail!("no group `{id}`");
                    };
                    // Refused when the member is not in the group; a bare
                    // model id one member has names it (the reference's
                    // `pickMember`, #317).
                    let pick = cfg.group[index].pick_member(&member)?;
                    cfg.group[index].pick = pick.clone();
                    cfg.group[index].routing = RoutingPolicy::Manual;
                    cfg.group[index].validate()?;
                    save_config(&path, &cfg)?;
                    println!("group `group/{id}` now sends every request to `{pick}`");
                }
                GroupCommand::Auto { state } => {
                    let path = provider_path(&cli.config)?;
                    let mut cfg = Config::load(&path)?;
                    let Some(state) = state else {
                        // Lite classifiers name models, never groups, and
                        // routes name providers directly, so no user group
                        // can hold a found one: turning them off orphans
                        // nothing but agents set to one, whose requests
                        // still go to its model from one provider.
                        if cfg.no_auto_groups {
                            println!(
                                "found groups are off · only the groups you made or changed; `magpie-lite group auto on` brings the others back"
                            );
                        } else {
                            println!(
                                "found groups are on · a model two or more providers serve is a group of them (auto-<model>); `magpie-lite group auto off` turns them off"
                            );
                        }
                        return Ok(());
                    };
                    let on = match state.to_ascii_lowercase().as_str() {
                        "on" | "true" | "yes" | "1" => true,
                        "off" | "false" | "no" | "0" => false,
                        _ => anyhow::bail!("magpie-lite group auto [on|off]"),
                    };
                    cfg.no_auto_groups = !on;
                    save_config(&path, &cfg)?;
                    if on {
                        println!(
                            "found groups are on · a model two or more providers serve is a group of them again"
                        );
                    } else {
                        println!(
                            "found groups are off · the groups you made or changed stay; a request for a found one goes to its model from one provider"
                        );
                    }
                }
            }
        }
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
