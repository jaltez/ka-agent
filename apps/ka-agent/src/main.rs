//! `ka-agent` — the Ka Agent binary: the resident daemon with its admin
//! socket, the local chat surface, first-run setup, ops commands, and the
//! hidden `__worker` / `__mcp` subcommands the runtime and engine supervise.

mod chat;
mod daemon;
mod doctor;
mod grant;
mod identity;
mod memory_cli;
mod ops;
mod plugin_cli;
mod setup;
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// First-run setup: write a starter config and prepare the data root.
    Init {
        /// Config path (default: ~/.config/ka-agent/ka-agent.toml).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Workspace directory for engines (default: <data>/workspace).
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Local chat surface over the runtime.
    Chat {
        /// Config path (default: ~/.config/ka-agent/ka-agent.toml).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Hidden: run the engine with a scripted speaker (e2e tests).
        #[arg(long, hide = true)]
        scenario: Option<PathBuf>,
    },
    /// Run the resident daemon: runtime + channels + admin socket.
    #[command(alias = "daemon")]
    Run {
        /// Config path (default: ~/.config/ka-agent/ka-agent.toml).
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
    /// Ask a running daemon to exit gracefully (the systemd unit restarts it).
    Restart,
    /// Query a running daemon over its admin socket.
    Status,
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
    /// Generate a one-shot auth URL for the web UI (M7).
    WebToken {
        /// Port the web server listens on (must match `run --web-port`).
        #[arg(long, default_value_t = 8480)]
        port: u16,
    },
    /// Instance health report.
    Doctor {
        #[arg(short, long)]
        config: Option<PathBuf>,
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
    Mcp,
    /// Manage tool grants (SEED.md §8.2).
    Grant {
        #[command(subcommand)]
        action: GrantAction,
    },
    /// Write a restorable backup (state + memory + notes + config).
    Backup {
        /// Output directory (created; must not already exist).
        out: PathBuf,
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
        /// Skill name (directory under ~/.config/ka-agent/skills).
        name: String,
    },
    /// Remove a skill's provisioning (the source copy is never touched).
    Disable {
        /// Skill name.
        name: String,
    },
}

fn config_path(flag: &Option<PathBuf>) -> PathBuf {
    flag.clone().unwrap_or_else(|| {
        if let Some(p) = std::env::var_os("KA_AGENT_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".config")
            });
        base.join("ka-agent/ka-agent.toml")
    })
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
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
        Command::Mcp => match ka_agent_channels::run_mcp() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ka-agent mcp: {e}");
                std::process::ExitCode::FAILURE
            }
        },
        Command::Init { config, workspace } => setup::init(config, workspace),
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
        Command::Grant { action } => grant::grant_action(action),
        Command::Backup { out, config } => ops::backup_to(out, config_path(&config)),
        Command::Restore { dir, force } => ops::restore_from(dir, force),
        Command::Export { out } => ops::export(&out),
        Command::Purge { confirm } => ops::purge(confirm),
        Command::Identity { action } => identity::identity_action(action),
        Command::Plugin { action } => plugin_cli::plugin_action(action),
        Command::Key { action } => setup::key_action(action),
        Command::Secret { action } => setup::secret_action(action),
        Command::Memory { action } => memory_cli::memory_action(action),
        Command::Skills { action } => skills_cli::skills_action(action),
        Command::WebToken { port } => ops::web_token(port),
        Command::Doctor { config } => doctor::doctor(config_path(&config)),
    }
}
