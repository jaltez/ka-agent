//! `ka-agent` — the Ka Agent binary: the resident daemon with its admin
//! socket, the local chat surface, first-run setup, ops commands, and
//! hidden `__worker` / `__mcp` subcommands the runtime and engine supervise.

mod admin;
mod bridge;
mod briefing_cli;
mod chat;
mod cli;
mod config_cli;
mod cron_cli;
mod daemon;
mod dashboard;
mod doctor;
mod grant;
mod host;
mod identity;
mod import_cli;
mod ipc;
mod layout;
mod mcp_cli;
mod memory_cli;
mod model_cli;
mod ops;
mod pair;
mod persona_cli;
mod plugin_cli;
mod profile_cli;
mod prompt;
mod secrets_cli;
mod service;
mod setup;
mod skills_cli;
mod update_cli;
mod usage_cli;

use clap::Parser as _;
use std::path::PathBuf;

pub(crate) use cli::{
    ApprovalsAction, BriefingAction, ConfigAction, CronAction, GrantAction, IdentityAction,
    ImportAction, McpAction, MemoryAction, PluginAction, ProfileAction, SkillsAction, UpdateAction,
};
use cli::{Cli, Command};

/// The completion script for `shell`, with profile names completed after
/// `-p/--profile` (read from the profiles directory when you press Tab).
fn completions(shell: clap_complete::Shell) -> std::process::ExitCode {
    use clap::CommandFactory as _;
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "ka-agent", &mut buf);
    let mut script = String::from_utf8_lossy(&buf).into_owned();
    let profiles = r#"ls -1 "${KA_AGENT_HOME:-$HOME/.ka-agent}/profiles" 2>/dev/null"#;
    match shell {
        clap_complete::Shell::Bash => {
            // clap offers file names for every valued option; profiles are
            // directory names under ~/.ka-agent/profiles.
            for opt in ["--profile", "-p"] {
                script = script.replace(
                    &format!("{opt})\n                    COMPREPLY=($(compgen -f \"${{cur}}\"))"),
                    &format!("{opt})\n                    COMPREPLY=($(compgen -W \"$({profiles})\" -- \"${{cur}}\"))"),
                );
            }
        }
        clap_complete::Shell::Fish => {
            script.push_str(&format!(
                "complete -c ka-agent -s p -l profile -x -a '({profiles})'\n"
            ));
        }
        _ => {}
    }
    // `| head` closes the pipe early: that is not an error worth a panic.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), script.as_bytes());
    std::process::ExitCode::SUCCESS
}

/// What a non-interactive `ka-agent` prints on a fresh machine: where to
/// start, not the full command list.
const START_HERE: &str = "\
Ka Agent isn't set up yet. Start here:
  ka-agent setup       guided setup, about 3 minutes
  ka-agent chat        talk to it in this terminal
  ka-agent status      is it running, what it costs
  ka-agent dashboard   open the web panel
  ka-agent --help      everything else
";

/// One wording for "this config can't be used", everywhere a command
/// needs one: a missing file is "not set up yet", never an I/O error.
pub(crate) fn config_problem(path: &std::path::Path, err: &dyn std::fmt::Display) -> String {
    if path.exists() {
        let text = err.to_string();
        // `unknown field \`name\``: point at the closest registered key
        let hint = text
            .split("unknown field `")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .and_then(ka_agent_core::config_keys::suggest)
            .map(|near| format!("\ndid you mean `{near}`?"))
            .unwrap_or_default();
        format!(
            "ka-agent: config {} is invalid: {text}{hint}\nfix it with `ka-agent config edit`",
            path.display()
        )
    } else {
        format!(
            "ka-agent: Ka Agent isn't set up yet ({} missing) — run `ka-agent setup` (about 3 minutes)",
            path.display()
        )
    }
}

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
            action: ProfileAction::Use { .. }
                | ProfileAction::List { .. }
                | ProfileAction::Create { .. }
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
                print!("{}", START_HERE);
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
        Command::McpBridge => match bridge::run() {
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
                Some("search") => setup::Section::Search,
                Some("briefing") => setup::Section::Briefing,
                Some("voice") => setup::Section::Voice,
                Some(other) => {
                    eprintln!(
                        "ka-agent: unknown section {other:?} — all | model | telegram | matrix | service | search | briefing | voice"
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
                .worker_threads(2)
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
                .worker_threads(2)
                .enable_all()
                .build()
            else {
                eprintln!("ka-agent: cannot build async runtime");
                return std::process::ExitCode::FAILURE;
            };
            rt.block_on(daemon::run_daemon(config_path(&config), scenario, web_port))
        }
        Command::Persona { action, config } => persona_cli::persona(action, config_path(&config)),
        Command::Restart => daemon::restart_client(),
        Command::Status { json } => daemon::status_client(json),
        Command::Completions { shell } => completions(shell),
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
            ConfigAction::Schema { json } => config_cli::config(
                config_cli::ConfigCommand::Schema { json },
                config_path(&config),
            ),
        },
        Command::Service { action, config } => {
            service::service_action(action, config_path(&config))
        }
        Command::Logs { follow, lines } => service::logs(follow, lines),
        Command::Profile { action } => profile_cli::profile_action(action),
        Command::Briefing { action } => briefing_cli::briefing_action(action),
        Command::Cron { action, json } => cron_cli::cron_action(action, json),
        Command::Import { action } => import_cli::import(action),
        Command::Update { action } => {
            update_cli::update(action.unwrap_or(UpdateAction::Run { version: None }))
        }
        Command::Approvals { action } => cron_cli::approvals_action(action),
        Command::Mcp { action, config } => mcp_cli::mcp_action(action, config_path(&config)),
        Command::Usage { days, json } => usage_cli::usage(days, json),
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
            rt.block_on(secrets_cli::key_action(action))
        }
        Command::Secret { action } => secrets_cli::secret_action(action),
        Command::Memory { action } => memory_cli::memory_action(action),
        Command::Skills { action } => skills_cli::skills_action(action),
        Command::Dashboard { config, no_open } => {
            dashboard::dashboard(config_path(&config), no_open)
        }
        Command::Doctor {
            config,
            online,
            json,
        } => doctor::doctor(config_path(&config), online, json),
    }
}
