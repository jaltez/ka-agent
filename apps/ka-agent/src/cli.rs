//! The clap CLI surface: `Cli`, the `Command` enum and every action
//! enum. Split from main.rs (moves only); dispatch stays in main.rs.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::service::ServiceAction;

#[derive(Parser)]
#[command(
    name = "ka-agent",
    version,
    about = "Ka Agent — resident assistant on the Ka engine"
)]
#[command(after_help = ADVANCED_HELP)]
pub(crate) struct Cli {
    /// Profile to act on (overrides $KA_AGENT_PROFILE and `profile use`).
    #[arg(short = 'p', long, global = true, value_name = "NAME")]
    pub(crate) profile: Option<String>,
    /// No subcommand: open the chat (or first-run setup on a fresh install).
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Show, edit or reset the assistant's persona.
    Persona {
        #[command(subcommand)]
        action: Option<PersonaAction>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Bring memory and skills from hermes | openclaw | claude | codex.
    #[command(hide = true)]
    Import {
        #[command(subcommand)]
        action: ImportAction,
    },
    /// Self-update: download the newest release, verify, swap, restart.
    Update {
        #[command(subcommand)]
        action: Option<UpdateAction>,
    },
    /// Guided setup: model and key, phone, web search, background service.
    #[command(visible_alias = "init")]
    Setup {
        /// Section: all | model | telegram | matrix | service | search | briefing | voice.
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
    /// Talk to Ka in this terminal.
    Chat {
        /// Config path (default: the active profile's, `ka-agent config path`).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Hidden: run the engine with a scripted speaker (e2e tests).
        #[arg(long, hide = true)]
        scenario: Option<PathBuf>,
    },
    /// Run the resident daemon: runtime + channels + admin socket.

    #[command(alias = "daemon", hide = true)]
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
    #[command(hide = true)]
    Restart,
    /// Is Ka running, what is it doing, what did it cost today.
    Status {
        /// One JSON document (`{ok, report}`).
        #[arg(long)]
        json: bool,
    },
    /// Issue a one-time pairing code for a channel (`ka-agent pair telegram`).
    #[command(hide = true)]
    Pair {
        /// telegram | matrix
        channel: String,
        /// Config path.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Show or switch the model (`ka-agent model openai/gpt-5.1`).
    Model {
        /// Omit to show the current model and its key state.
        selector: Option<String>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// List models per provider.
    #[command(hide = true)]
    Models {
        /// Omit for every provider.
        provider: Option<String>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Read or change settings (comments kept, applied live).
    Config {
        #[command(subcommand)]
        action: ConfigAction,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Run Ka in the background, starting at login.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Scheduled prompts: list, add, pause, resume, run, history.
    Cron {
        #[command(subcommand)]
        action: Option<CronAction>,
        /// Machine-readable output (`list`, `runs`).
        #[arg(long, global = true)]
        json: bool,
    },
    /// Pending approvals: list, allow (`y <id>`) or deny (`n <id>`).
    Approvals {
        #[command(subcommand)]
        action: Option<ApprovalsAction>,
    },
    /// A morning briefing: calendar, reminders, what waits for you.
    #[command(hide = true)]
    Briefing {
        #[command(subcommand)]
        action: Option<BriefingAction>,
    },
    /// Connect MCP servers: their tools become the agent's tools.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Separate instances (work, home…): own data, settings, service.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// What Ka spent, per day.
    Usage {
        /// How many days back (1-366, default 30).
        #[arg(long, default_value_t = 30)]
        days: u32,
        /// One JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Show the daemon log (`-f` to follow).
    #[command(hide = true)]
    Logs {
        /// Follow the log like `tail -f`.
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
    },
    /// Link channel identities (Telegram/Matrix users) to you, the operator.
    #[command(hide = true)]
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },
    /// Install, inspect and manage signed, sandboxed plugins.
    #[command(hide = true)]
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Manage engine provider keys (ka's .env tier under the data root).
    #[command(hide = true)]
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Manage channel and connector secrets (0600 files under <data>/secrets).
    #[command(hide = true)]
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// What Ka remembers (plain Markdown you can edit).
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Manage skills: list, enable, disable.
    #[command(hide = true)]
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
    /// Open the web panel in your browser.
    Dashboard {
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Print the login URL without opening a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Check everything and say how to fix what isn't right.
    Doctor {
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Also check the provider key and channel tokens live.
        #[arg(long)]
        online: bool,
        /// One JSON document (`{ok, checks: [{level, text}]}`).
        #[arg(long)]
        json: bool,
    },
    /// Print a shell completion script (`source <(ka-agent completions bash)`).
    #[command(hide = true)]
    Completions {
        /// bash | zsh | fish | elvish | powershell
        shell: clap_complete::Shell,
    },
    /// Hidden: the supervised worker process (spawned by the runtime).
    #[command(name = "__worker", hide = true)]
    Worker {
        /// Default scripted-speaker scenario (e2e; Open can override).
        #[arg(long, hide = true)]
        scenario: Option<PathBuf>,
    },
    /// Hidden: the MCP bridge the Ka engine spawns as `kagent`.
    #[command(name = "__guard", hide = true)]
    Guard,
    #[command(name = "__mcp", hide = true)]
    McpBridge,
    /// Manage tool grants (what the agent may reach: hosts, repos, …).
    #[command(hide = true)]
    Grant {
        #[command(subcommand)]
        action: GrantAction,
    },
    /// Write a restorable backup (state + memory + notes + config).
    #[command(hide = true)]
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
    #[command(hide = true)]
    Restore {
        /// Backup directory (as written by `ka-agent backup`).
        dir: PathBuf,
        /// Allow restoring over a non-empty data root.
        #[arg(long)]
        force: bool,
    },
    /// Export instance data to a directory.
    #[command(hide = true)]
    Export {
        /// Output directory (created if absent).
        out: PathBuf,
    },
    /// Delete all instance data.
    #[command(hide = true)]
    Purge {
        /// Skip confirmation prompt.
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum ImportAction {
    /// `import hermes` / `openclaw` / `claude` / `codex`.
    Agent {
        agent: String,
        /// List what would be imported; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum UpdateAction {
    /// Show whether a newer release exists (no download).
    Check {
        /// A specific version instead of the latest.
        version: Option<String>,
    },
    /// Install the newer release and restart the service(s).
    Run {
        /// A specific version instead of the latest.
        version: Option<String>,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum CronAction {
    /// Live crons, next fire first.
    List,
    /// `add every weekday at 9 standup notes` · `add in 2h check the build`.
    Add {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// Remove a cron.
    Rm { id: String },
    /// Pause a cron (kept, not fired).
    Pause { id: String },
    /// Resume at its next occurrence.
    Resume { id: String },
    /// Fire now (then again on schedule).
    Run { id: String },
    /// What happened on its last fires.
    Runs { id: String },
}

#[derive(clap::Subcommand)]
pub(crate) enum ApprovalsAction {
    /// What is waiting for a decision.
    List,
    /// Allow one (id or its tail).
    Y { id: String },
    /// Deny one.
    N { id: String },
}

#[derive(clap::Subcommand)]
pub(crate) enum BriefingAction {
    /// Send it every day at this local time (`on 07:30`).
    On { time: String },
    /// Stop the daily briefing.
    Off,
    /// Send it now (then again at its usual time).
    Now,
}

#[derive(clap::Subcommand)]
pub(crate) enum McpAction {
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
pub(crate) enum ProfileAction {
    /// List profiles (active one marked, running ones flagged).
    List {
        /// One JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Rename a profile (data, config, units, the active pointer).
    Rename { from: String, to: String },
    /// Create a profile (config copied from another, minus channels).
    Create {
        name: String,
        /// Profile whose config to start from.
        #[arg(long, default_value = "default")]
        from: String,
        /// Also copy that profile's provider keys.
        #[arg(long)]
        copy_keys: bool,
        /// Also copy its memory files (never sessions, crons or identities).
        #[arg(long)]
        clone_memory: bool,
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
pub(crate) enum ConfigAction {
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
    /// List every config key: type, default, how a change applies.
    Schema {
        /// Emit the registry as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum PersonaAction {
    /// Print the persona.
    Show,
    /// Open $VISUAL/$EDITOR on a validated copy, save and recompile.
    Edit,
    /// Restore the default persona (keeps the current name).
    Reset,
}

#[derive(clap::Subcommand)]
pub(crate) enum GrantAction {
    /// Add a grant scope.
    Add { scope: String },
    /// Remove a grant scope.
    Remove { scope: String },
    /// List all grant scopes.
    List,
}

#[derive(clap::Subcommand, Debug, Clone)]
pub(crate) enum PluginAction {
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
pub(crate) enum IdentityAction {
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
pub(crate) enum KeyAction {
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
pub(crate) enum SecretAction {
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
pub(crate) enum MemoryAction {
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
pub(crate) enum SkillsAction {
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

/// The footer of `ka-agent --help`: the commands kept out of the main list
/// (they all work; `ka-agent <command> --help` explains each).
const ADVANCED_HELP: &str = "\
More commands: logs briefing run restart pair models key secret identity grant
               plugin skills backup restore export purge completions
               — `ka-agent <command> --help`";
