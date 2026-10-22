//! `ka-agent` — the Ka Agent binary: the resident daemon with its admin
//! socket, the local chat surface, first-run setup, ops commands, and the
//! hidden `__worker` / `__mcp` subcommands the runtime and engine supervise.

mod admin;
mod chat;
mod config_cli;
mod daemon;
mod dashboard;
mod doctor;
mod grant;
mod identity;
mod layout;
mod mcp_cli;
mod memory_cli;
mod model_cli;
mod ops;
mod pair;
mod plugin_cli;
mod profile_cli;
mod prompt;
mod service;
mod setup;
mod usage_cli;
use service::ServiceAction;
mod skills_cli;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ka-agent",
    version,
    about = "Ka Agent — resident assistant on the Ka engine"
)]
struct Cli {
    /// Profile to act on (overrides $KA_AGENT_PROFILE and `profile use`).
    #[arg(short = 'p', long, global = true, value_name = "NAME")]
    profile: Option<String>,
    /// No subcommand: open the chat (or first-run setup on a fresh install).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// First-run wizard: provider, live key check, model, timezone,
    /// channels, service, and a verified first reply.
    #[command(visible_alias = "init")]
    Setup {
        /// Section: all | model | telegram | matrix | service.
        section: Option<String>,
        /// Config path (default: the active profile's, `ka-agent config path`).
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        provider: Option<String>,
        /// Exact model selector (provider/model).
        #[arg(long)]
        model: Option<String>,
        /// Read the API key from stdin's first line.
        #[arg(long)]
        key_stdin: bool,
        #[arg(long)]
        timezone: Option<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Non-interactive: every prompt takes its default.
        #[arg(long)]
        yes: bool,
        /// Skip the live credential check.
        #[arg(long)]
        no_verify: bool,
        /// Do not offer the background service.
        #[arg(long)]
        no_service: bool,
    },
    /// Local chat surface over the runtime.
    Chat {
        /// Config path (default: the active profile's, `ka-agent config path`).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Hidden: run the engine with a scripted speaker (e2e tests).
        #[arg(long, hide = true)]
        scenario: Option<PathBuf>,
    },
    /// Run the resident daemon: runtime + channels + admin socket.
    #[command(alias = "daemon")]
    Run {
        /// Config path (default: the active profile's, `ka-agent config path`).
        /// Invalid config exits 1 (no fallback to defaults).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Hidden test hook: run the engine worker with a scripted speaker.
        #[arg(long, hide = true)]
        scenario: Option<PathBuf>,
        /// Enable the loopback-only web UI on this port (default: disabled).
        #[arg(long)]
        web_port: Option<u16>,
    },
    /// Ask a running daemon to exit gracefully (the background service
    /// restarts it).
    Restart,
    /// Query a running daemon over its admin socket.
    Status,
    /// Issue a one-time pairing code for a channel (`ka-agent pair telegram`).
    Pair {
        /// telegram | matrix
        channel: String,
        /// Config path.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Show or switch the model (`ka-agent model provider/model`).
    Model {
        /// Omit to show the current model and its key state.
        selector: Option<String>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// List models per provider.
    Models {
        /// Omit for every provider.
        provider: Option<String>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Inspect or change the config (format-preserving, live reload).
    Config {
        #[command(subcommand)]
        action: ConfigAction,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Install, start, stop or remove the background service; `ka-agent logs`.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Connect MCP servers: their tools become the agent's tools.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Manage profiles: independent instances (data, config, service).
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Spend per day: turns, tokens, cost (reads the store directly).
    Usage {
        /// How many days back (1-366, default 30).
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
    /// Show the daemon log (`-f` to follow).
    Logs {
        /// Follow the log like `tail -f`.
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
    },
    /// Link channel identities to this instance's operator (SEED.md §8.1).
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },
    /// Install, inspect and manage plugins (SEED.md §6).
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Manage engine provider keys (ka's .env tier under the data root).
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Manage channel and connector secrets (0600 files under <data>/secrets).
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Inspect or edit the stable-memory Markdown files.
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Manage skills: list, enable, disable (SEED.md §9.1).
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
    /// Open the web control panel in your browser (one-shot login link).
    Dashboard {
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Print the login URL without opening a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Instance health report.
    Doctor {
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Also check the provider key and channel tokens live.
        #[arg(long)]
        online: bool,
    },
    /// Hidden: the supervised worker process (spawned by the runtime).
    #[command(name = "__worker", hide = true)]
    Worker {
        /// Default scripted-speaker scenario (e2e; Open can override).
        #[arg(long, hide = true)]
        scenario: Option<PathBuf>,
    },
    /// Hidden: the MCP bridge the Ka engine spawns as `kagent`.
    #[command(name = "__mcp", hide = true)]
    McpBridge,
    /// Manage tool grants (SEED.md §8.2).
    Grant {
        #[command(subcommand)]
        action: GrantAction,
    },
    /// Write a restorable backup (state + memory + notes + config).
    Backup {
        /// Output directory (default: <data>/backups/<timestamp>, pruned).
        out: Option<PathBuf>,
        /// Backups to keep when pruning the default directory.
        #[arg(long, default_value_t = 7)]
        keep: usize,
        /// Config path (copied into the backup).
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Restore an instance from a backup into the data root.
    Restore {
        /// Backup directory (as written by `ka-agent backup`).
        dir: PathBuf,
        /// Allow restoring over a non-empty data root.
        #[arg(long)]
        force: bool,
    },
    /// Export instance data to a directory.
    Export {
        /// Output directory (created if absent).
        out: PathBuf,
    },
    /// Delete all instance data.
    Purge {
        /// Skip confirmation prompt.
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(clap::Subcommand)]
enum McpAction {
    /// List configured MCP servers.
    List,
    /// Add a server: `mcp add notes -- npx -y notes-mcp` or `mcp add home --url http://…`.
    Add {
        name: String,
        /// Streamable-HTTP endpoint (instead of a command).
        #[arg(long)]
        url: Option<String>,
        /// Environment for a stdio server; `KEY=secret:<name>` reads the secret store.
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// HTTP header for a url server; `Authorization=secret:<name>` works too.
        #[arg(long, value_name = "KEY=VALUE")]
        header: Vec<String>,
        /// The command and its arguments (after `--`).
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Remove a server.
    Rm { name: String },
}

#[derive(clap::Subcommand)]
enum ProfileAction {
    /// List profiles (active one marked, running ones flagged).
    List,
    /// Create a profile (config copied from another, minus channels).
    Create {
        name: String,
        /// Profile whose config to start from.
        #[arg(long, default_value = "default")]
        from: String,
        /// Also copy that profile's provider keys.
        #[arg(long)]
        copy_keys: bool,
    },
    /// Make a profile the default for commands without `-p`.
    Use { name: String },
    /// Print a profile's paths and state.
    Show { name: Option<String> },
    /// Delete a profile's data and config (not `default`).
    Remove {
        name: String,
        /// Required: this deletes data.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Subcommand)]
enum ConfigAction {
    /// Print the config file's absolute path.
    Path,
    /// Print the config file verbatim.
    Show,
    /// Print one value (`ka-agent config get budgets.daily_usd`).
    Get { key: String },
    /// Set one value (dotted key; arrays are edited with `config edit`).
    Set { key: String, value: String },
    /// Remove one value (default applies again).
    Unset { key: String },
    /// Open $EDITOR on a validated copy.
    Edit {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum GrantAction {
    /// Add a grant scope.
    Add { scope: String },
    /// Remove a grant scope.
    Remove { scope: String },
    /// List all grant scopes.
    List,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum PluginAction {
    /// Generate this instance's ed25519 signing key (once; TOFU pins it).
    Keygen,
    /// Pack a plugin directory into a signed archive.
    Pack {
        /// The plugin directory (contains manifest.toml).
        dir: PathBuf,
        /// Where to write `<id>-<version>.kplug` (default: the current
        /// directory).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Verify, pin and install an archive (never enables it).
    Install {
        /// The `.kplug` archive.
        archive: PathBuf,
    },
    /// List installed plugins, their state and their tools.
    List,
    /// Enable a plugin (policy: CLI-only).
    Enable {
        /// Plugin id.
        id: String,
    },
    /// Disable a plugin (policy: CLI-only).
    Disable {
        /// Plugin id.
        id: String,
    },
    /// Remove a plugin (refuses while enabled; use `disable` first).
    Rm {
        /// Plugin id.
        id: String,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
enum IdentityAction {
    /// List the identities linked to this operator.
    List,
    /// Link a channel identity (`ka-agent identity link telegram 12345`).
    Link {
        /// Channel name: telegram, matrix, …
        channel: String,
        /// The channel's stable address (numeric id, @user:domain, …).
        address: String,
    },
    /// Unlink a channel identity.
    Unlink {
        /// Channel name.
        channel: String,
        /// The address to unlink.
        address: String,
    },
    /// Unknown contacts that messaged the bot, with the link command.
    Pending,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum KeyAction {
    /// Set a provider key (`NAME` like ANTHROPIC_API_KEY; value on the
    /// argument or one line on stdin). Written 0600; never printed again.
    Set {
        /// The environment variable name the engine resolves the key by.
        name: String,
        /// The key (omit to read one line from stdin).
        value: Option<String>,
    },
    /// List set provider key names (values are never printed).
    List,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum SecretAction {
    /// Set a secret (`ka-agent secret set telegram.bot_token`).
    Set {
        /// Secret name, as referenced from ka-agent.toml.
        name: String,
        /// The value (omit to read one line from stdin).
        value: Option<String>,
    },
    /// List secret names (values are never printed).
    List,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum MemoryAction {
    /// Print the memory directory (plain Markdown; edit it or use `edit`).
    Path,
    /// Print one memory file (facts by default).
    Show {
        /// facts | profile | persona | preferences | environment | conventions.
        file: Option<String>,
    },
    /// Edit one memory file in $EDITOR, then recompile and commit.
    Edit {
        /// facts | profile | persona | preferences | environment | conventions.
        file: Option<String>,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
enum SkillsAction {
    /// List skills: the operator's library with [enabled] markers.
    List,
    /// Provision a skill into the engine home (ka discovers it there).
    Enable {
        /// Skill name (directory under ~/.ka-agent/skills).
        name: String,
    },
    /// Remove a skill's provisioning (the source copy is never touched).
    Disable {
        /// Skill name.
        name: String,
    },
}

use clap::CommandFactory as _;

fn config_path(flag: &Option<PathBuf>) -> PathBuf {
    flag.clone().unwrap_or_else(default_config_path)
}

fn default_config_path() -> PathBuf {
    ka_agent_core::layout::default_config_path()
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if let Some(p) = &cli.profile {
        if !ka_agent_core::layout::valid_profile_name(p) {
            eprintln!("ka-agent: profile name {p:?}: [a-z0-9-], up to 32 chars");
            return std::process::ExitCode::FAILURE;
        }
        if std::env::var_os("KA_AGENT_DATA_DIR").is_some_and(|v| !v.is_empty()) {
            eprintln!(
                "ka-agent: --profile conflicts with KA_AGENT_DATA_DIR (it pins one instance)"
            );
            return std::process::ExitCode::FAILURE;
        }
        ka_agent_core::layout::set_profile_override(p);
    }
    let picking_profile = matches!(
        cli.command,
        Some(Command::Profile {
            action: ProfileAction::Use { .. } | ProfileAction::List | ProfileAction::Create { .. }
        })
    );
    if !picking_profile
        && std::env::var_os("KA_AGENT_DATA_DIR").is_none_or(|v| v.is_empty())
        && let Some(problem) = ka_agent_core::layout::profile_selection_problem()
    {
        eprintln!("ka-agent: {problem}");
        return std::process::ExitCode::FAILURE;
    }
    // One-time move to the ~/.ka-agent layout — never for the supervised
    // helper processes (they run inside an already-migrated environment).
    let helper = matches!(
        cli.command,
        Some(Command::Worker { .. }) | Some(Command::McpBridge)
    );
    if !helper && let Err(e) = layout::migrate() {
        eprintln!("ka-agent: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let command = match cli.command {
        // Plain `ka-agent`: chat when configured; first-run setup otherwise
        // (interactive terminals only — a pipe gets help, not a wizard).
        None => {
            let config = config_path(&None);
            if config.exists() {
                Command::Chat {
                    config: None,
                    scenario: None,
                }
            } else if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                Command::Setup {
                    section: None,
                    config: None,
                    provider: None,
                    model: None,
                    key_stdin: false,
                    timezone: None,
                    workspace: None,
                    yes: false,
                    no_verify: false,
                    no_service: false,
                }
            } else {
                Cli::command().print_help().ok();
                return std::process::ExitCode::from(2);
            }
        }
        Some(c) => c,
    };
    match command {
        // The worker builds its own current-thread runtime inside
        // `run_worker`; it must not run inside another tokio runtime.
        Command::Worker { scenario } => {
            match ka_agent_port::run_worker(ka_agent_port::WorkerRun { scenario }) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("ka-agent worker: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Command::McpBridge => match ka_agent_channels::run_mcp() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ka-agent mcp: {e}");
                std::process::ExitCode::FAILURE
            }
        },
        Command::Setup {
            section,
            config,
            provider,
            model,
            key_stdin,
            timezone,
            workspace,
            yes,
            no_verify,
            no_service,
        } => {
            let section = match section.as_deref() {
                None | Some("all") => setup::Section::All,
                Some("model") => setup::Section::Model,
                Some("telegram") => setup::Section::Telegram,
                Some("matrix") => setup::Section::Matrix,
                Some("service") => setup::Section::Service,
                Some(other) => {
                    eprintln!(
                        "ka-agent: unknown section {other:?} — all | model | telegram | matrix | service"
                    );
                    return std::process::ExitCode::FAILURE;
                }
            };
            // Prompts block on stdin; a current-thread runtime is enough and
            // keeps the key check (async) simple.
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                eprintln!("ka-agent: cannot build async runtime");
                return std::process::ExitCode::FAILURE;
            };
            let args = setup::SetupArgs {
                section,
                provider,
                model,
                key_stdin,
                timezone,
                workspace,
                yes,
                no_verify,
                no_service,
                config: config_path(&config),
            };
            rt.block_on(setup::run(args))
        }
        Command::Chat { config, scenario } => {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            else {
                eprintln!("ka-agent: cannot build async runtime");
                return std::process::ExitCode::FAILURE;
            };
            rt.block_on(chat::chat(config_path(&config), scenario))
        }
        Command::Run {
            config,
            scenario,
            web_port,
        } => {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            else {
                eprintln!("ka-agent: cannot build async runtime");
                return std::process::ExitCode::FAILURE;
            };
            rt.block_on(daemon::run_daemon(config_path(&config), scenario, web_port))
        }
        Command::Restart => daemon::restart_client(),
        Command::Status => daemon::status_client(),
        Command::Pair { channel, config } => pair::pair(channel, config_path(&config)),
        Command::Model { selector, config } => {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                eprintln!("ka-agent: cannot build async runtime");
                return std::process::ExitCode::FAILURE;
            };
            rt.block_on(model_cli::model(model_cli::ModelArgs {
                selector,
                config: config_path(&config),
            }))
        }
        Command::Models {
            provider,
            config: _,
        } => model_cli::models(provider),
        Command::Config { action, config } => match action {
            ConfigAction::Path => {
                config_cli::config(config_cli::ConfigCommand::Path, config_path(&config))
            }
            ConfigAction::Show => {
                config_cli::config(config_cli::ConfigCommand::Show, config_path(&config))
            }
            ConfigAction::Get { key } => {
                config_cli::config(config_cli::ConfigCommand::Get { key }, config_path(&config))
            }
            ConfigAction::Set { key, value } => config_cli::config(
                config_cli::ConfigCommand::Set { key, value },
                config_path(&config),
            ),
            ConfigAction::Unset { key } => config_cli::config(
                config_cli::ConfigCommand::Unset { key },
                config_path(&config),
            ),
            ConfigAction::Edit { config: cfg2 } => config_cli::config(
                config_cli::ConfigCommand::Edit,
                config_path(&cfg2.or(config)),
            ),
        },
        Command::Service { action, config } => {
            service::service_action(action, config_path(&config))
        }
        Command::Logs { follow, lines } => service::logs(follow, lines),
        Command::Profile { action } => profile_cli::profile_action(action),
        Command::Mcp { action, config } => mcp_cli::mcp_action(action, config_path(&config)),
        Command::Usage { days } => usage_cli::usage(days),
        Command::Grant { action } => grant::grant_action(action),
        Command::Backup { out, keep, config } => ops::backup_to(out, keep, config_path(&config)),
        Command::Restore { dir, force } => ops::restore_from(dir, force),
        Command::Export { out } => ops::export(&out),
        Command::Purge { confirm } => ops::purge(confirm),
        Command::Identity { action } => identity::identity_action(action),
        Command::Plugin { action } => plugin_cli::plugin_action(action),
        Command::Key { action } => {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                eprintln!("ka-agent: cannot build async runtime");
                return std::process::ExitCode::FAILURE;
            };
            rt.block_on(setup::key_action(action))
        }
        Command::Secret { action } => setup::secret_action(action),
        Command::Memory { action } => memory_cli::memory_action(action),
        Command::Skills { action } => skills_cli::skills_action(action),
        Command::Dashboard { config, no_open } => {
            dashboard::dashboard(config_path(&config), no_open)
        }
        Command::Doctor { config, online } => doctor::doctor(config_path(&config), online),
    }
}
