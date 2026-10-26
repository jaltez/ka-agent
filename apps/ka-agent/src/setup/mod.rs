//! `ka-agent setup`: the wizard from zero to a verified first reply —
//! provider, live key check, model, timezone, workspace, channels, service,
//! and a test message. Nothing is written until the operator confirms.
//!
//! Non-interactive use: `--yes` takes every default without reading input
//! (channels and the test message are skipped); without `--yes` every
//! answer is read line by line from stdin (terminal or pipe), and secrets
//! are echoed off only when stdin is a terminal.
//!
//! Split from the former single `setup.rs` (moves only): [`provider`]
//! holds provider/key/model selection, [`channels`] the Telegram/Matrix
//! sections, [`extras`] search/briefing/voice, [`write`] the config
//! write and post-write steps.

mod channels;
mod extras;
mod provider;
mod write;

use std::io::BufRead;
use std::path::{Path, PathBuf};

use ka_agent_core::{AuditRecord, DaemonConfig};
use ka_agent_port::catalog::{self, CredentialCheck, ProviderInfo};
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::prompt::{self, PromptError};

/// What `ka-agent setup [SECTION]` should configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    All,
    Model,
    Telegram,
    Matrix,
    Service,
    Search,
    Briefing,
    Voice,
}

/// Parsed `ka-agent setup` arguments.
pub struct SetupArgs {
    pub section: Section,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub key_stdin: bool,
    pub timezone: Option<String>,
    pub workspace: Option<PathBuf>,
    pub yes: bool,
    pub no_verify: bool,
    pub no_service: bool,
    pub config: PathBuf,
}

/// Read the setup key from stdin's first line (`--key-stdin`, either mode).
fn read_key_stdin() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            let v = line.trim().to_string();
            (!v.is_empty()).then_some(v)
        }
    }
}

fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

struct Setup {
    args: SetupArgs,
    paths: Paths,
    doc: toml_edit::DocumentMut,
    fresh: bool,
    /// The provider chosen at step 2 (for the model step and the write).
    provider: Option<ProviderInfo>,
    key: Option<String>,
    key_from: &'static str,
    live_models: Vec<String>,
    model: Option<String>,
    timezone: Option<String>,
    workspace: Option<PathBuf>,
    telegram_token: Option<String>,
    telegram_user: Option<String>,
    matrix: Option<(String, String)>, // homeserver, user_id (bot)
    matrix_operator: Option<String>,
    /// Web search chosen in setup: provider and, when newly entered, its key.
    search: Option<(String, Option<String>)>,
    /// Daily briefing time chosen in setup (`HH:MM`).
    briefing: Option<String>,
    /// Voice notes: the `[voice]` choice and, when newly entered, its key.
    voice: Option<(ka_agent_core::Voice, Option<String>)>,
}

fn prompt_err(e: PromptError) -> std::process::ExitCode {
    match e {
        PromptError::Closed => println!("setup aborted (input closed)"),
        PromptError::Io(io) => eprintln!("ka-agent: {io}"),
    }
    std::process::ExitCode::FAILURE
}

/// Wizard failures: a closed prompt aborts quietly; anything else prints.
enum Fail {
    Prompt(PromptError),
    Msg(String),
}

impl From<PromptError> for Fail {
    fn from(e: PromptError) -> Self {
        Fail::Prompt(e)
    }
}

impl From<String> for Fail {
    fn from(e: String) -> Self {
        Fail::Msg(e)
    }
}

impl Setup {
    fn env_file(&self) -> PathBuf {
        catalog::engine_env_path(&self.paths.engine_home())
    }

    fn engine_env(&self) -> std::collections::BTreeMap<String, String> {
        catalog::read_env_file(&self.env_file())
    }

    fn key_found(&self, p: &ProviderInfo) -> bool {
        p.key_env
            .is_some_and(|e| self.engine_env().contains_key(e) || env_lookup(e).is_some())
    }

    async fn run(&mut self) -> std::process::ExitCode {
        match self.wizard().await {
            Ok(code) => code,
            Err(Fail::Prompt(e)) => prompt_err(e),
            Err(Fail::Msg(m)) => {
                println!("setup aborted: {m}");
                std::process::ExitCode::FAILURE
            }
        }
    }

    async fn wizard(&mut self) -> Result<std::process::ExitCode, Fail> {
        println!(
            "Ka Agent setup — about 3 minutes. Nothing is saved until you confirm at the end."
        );
        if self.args.yes {
            return self.yes_flow().await;
        }
        // An explicit `ka-agent setup telegram` runs only that section;
        // before, it fell through to the full wizard and buried the
        // channel steps behind provider/model prompts.
        if self.args.section != Section::All {
            return self.partial(self.args.section).await;
        }
        // No SECTION given and a config exists: ask which.
        if !self.fresh {
            let options: Vec<String> = vec![
                "Model & provider key".into(),
                "Telegram".into(),
                "Matrix".into(),
                "Background service".into(),
                "Web search".into(),
                "Daily briefing".into(),
                "Voice notes".into(),
                "Everything".into(),
            ];
            let pick = prompt::choice("What do you want to set up?", &options, 0)?;
            self.args.section = match pick {
                0 => Section::Model,
                1 => Section::Telegram,
                2 => Section::Matrix,
                3 => Section::Service,
                4 => Section::Search,
                5 => Section::Briefing,
                6 => Section::Voice,
                _ => Section::All,
            };
            if self.args.section != Section::All {
                return self.partial(self.args.section).await;
            }
        }
        self.step_provider().await?;
        self.step_key().await?;
        self.step_verify().await;
        self.step_model().await?;
        self.step_timezone().await?;
        self.step_workspace().await?;
        let channel = if self.args.section == Section::All {
            self.step_channels().await?
        } else {
            None
        };
        self.step_search()?;
        self.step_briefing()?;
        // Voice notes only matter once a phone can send them.
        if channel.is_some() {
            self.step_voice()?;
        }
        if !self.confirm_and_write().await? {
            return Ok(std::process::ExitCode::SUCCESS);
        }
        self.post_write(channel).await
    }

    /// A single SECTION on an existing install: only those steps.
    async fn partial(&mut self, section: Section) -> Result<std::process::ExitCode, Fail> {
        match section {
            Section::Model => {
                self.step_provider().await?;
                self.step_key().await?;
                self.step_verify().await;
                self.step_model().await?;
                if !self.confirm_and_write().await? {
                    return Ok(std::process::ExitCode::SUCCESS);
                }
                self.post_write(None).await
            }
            Section::Telegram => {
                self.setup_telegram().await?;
                if !self.confirm_and_write().await? {
                    return Ok(std::process::ExitCode::SUCCESS);
                }
                self.post_write(Some("telegram")).await
            }
            Section::Matrix => {
                self.setup_matrix().await?;
                if !self.confirm_and_write().await? {
                    return Ok(std::process::ExitCode::SUCCESS);
                }
                self.post_write(Some("matrix")).await
            }
            Section::Service => self.step_service(),
            Section::Voice => {
                self.step_voice()?;
                if self.voice.is_none() || !self.confirm_and_write().await? {
                    return Ok(std::process::ExitCode::SUCCESS);
                }
                crate::config_cli::apply_live(&self.paths, &self.args.config);
                println!("Voice notes on — restart the agent so the phone channels pick it up");
                Ok(std::process::ExitCode::SUCCESS)
            }
            Section::Briefing => {
                self.step_briefing()?;
                if self.briefing.is_none() || !self.confirm_and_write().await? {
                    return Ok(std::process::ExitCode::SUCCESS);
                }
                println!("Daily briefing on — `ka-agent briefing now` sends one right away");
                Ok(std::process::ExitCode::SUCCESS)
            }
            Section::Search => {
                self.step_search()?;
                if self.search.is_none() || !self.confirm_and_write().await? {
                    return Ok(std::process::ExitCode::SUCCESS);
                }
                crate::config_cli::apply_live(&self.paths, &self.args.config);
                println!("Web search is on — try: \"search the web for …\"");
                Ok(std::process::ExitCode::SUCCESS)
            }
            Section::All => unreachable!("handled by wizard()"),
        }
    }

    /// `--yes`: every default, no prompts, channels and test skipped.
    async fn yes_flow(&mut self) -> Result<std::process::ExitCode, Fail> {
        // Provider (flag or first with a key found, else anthropic).
        let providers = catalog::providers();
        let name = match self.args.provider.clone() {
            Some(n) => n,
            None => providers
                .iter()
                .find(|p| p.key_env.is_some_and(|_e| self.key_found(p)))
                .or_else(|| providers.first())
                .map(|p| p.name.to_string())
                .unwrap_or_else(|| "anthropic".to_string()),
        };
        let p = catalog::provider(&name)
            .ok_or_else(|| format!("unknown provider {name} — ka-agent models lists them"))?;
        self.provider = Some(p.clone());
        // Key: --key-stdin → engine .env → process env → fail.
        self.key = self.args.key_stdin.then(read_key_stdin).flatten();
        let mut key_from = if self.args.key_stdin { "stdin" } else { "" };
        if self.key.is_none()
            && let Some(env) = p.key_env
        {
            if let Some(v) = self.engine_env().get(env) {
                self.key = Some(v.clone());
                key_from = "saved key";
            } else if let Some(v) = env_lookup(env) {
                self.key = Some(v);
                key_from = "environment";
            }
        }
        if p.key_env.is_some() && self.key.is_none() {
            let env = p.key_env.unwrap_or("KEY");
            return Err(Fail::Msg(format!(
                "no {env} found — pass --key-stdin or run `ka-agent setup` in a terminal"
            )));
        }
        self.key_from = match key_from {
            "stdin" => "stdin",
            "saved key" => "saved key",
            "environment" => "environment",
            _ => "none",
        };
        // Model.
        let model = match self.args.model.clone() {
            Some(m) => m,
            None => catalog::default_model(p.name)
                .map(str::to_string)
                .ok_or_else(|| {
                    Fail::Msg(format!(
                        "no default model for {} — pass --model provider/model",
                        p.name
                    ))
                })?,
        };
        let info = catalog::validate_selector(&model)?;
        self.model = Some(info.id);
        self.step_timezone_yes()?;
        self.step_workspace_yes()?;
        self.confirm_and_write().await?;
        print!("{}", self.live_card(false));
        Ok(std::process::ExitCode::SUCCESS)
    }

    fn step_timezone_yes(&mut self) -> Result<(), String> {
        self.timezone = Some(match self.args.timezone.clone() {
            Some(tz) => validate_tz(&tz)?,
            None => system_tz().ok_or_else(|| {
                "could not detect the timezone — pass --timezone Europe/Madrid".to_string()
            })?,
        });
        Ok(())
    }

    fn step_workspace_yes(&mut self) -> Result<(), String> {
        self.workspace = Some(match self.args.workspace.clone() {
            Some(w) => w,
            None => default_workspace(&self.args.config),
        });
        Ok(())
    }

    async fn step_timezone(&mut self) -> Result<(), PromptError> {
        if let Some(tz) = self.args.timezone.clone() {
            match validate_tz(&tz) {
                Ok(tz) => self.timezone = Some(tz),
                Err(e) => {
                    println!("{e}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        let detected = system_tz().unwrap_or_else(|| "UTC".to_string());
        if prompt::yes_no(&format!("Your timezone is {detected}?"), true)? {
            self.timezone = Some(detected);
            return Ok(());
        }
        loop {
            let tz = prompt::text("IANA timezone (e.g. Europe/Madrid)", Some("UTC"))?;
            match validate_tz(&tz) {
                Ok(tz) => {
                    self.timezone = Some(tz);
                    return Ok(());
                }
                Err(e) => println!("{e}"),
            }
        }
    }

    async fn step_workspace(&mut self) -> Result<(), PromptError> {
        let default = self
            .args
            .workspace
            .clone()
            .unwrap_or_else(|| default_workspace(&self.args.config));
        let ws = prompt::text(
            "Folder the agent works in",
            Some(&default.display().to_string()),
        )?;
        self.workspace = Some(PathBuf::from(ws));
        Ok(())
    }
}

fn system_tz() -> Option<String> {
    jiff::tz::TimeZone::system().iana_name().map(str::to_string)
}

fn validate_tz(tz: &str) -> Result<String, String> {
    match jiff::tz::TimeZone::get(tz) {
        Ok(_) => Ok(tz.to_string()),
        Err(_) => Err(format!(
            "{tz:?} is not a known IANA zone (e.g. \"Europe/Madrid\")"
        )),
    }
}

fn default_workspace(config: &Path) -> PathBuf {
    let paths = Paths::new(Paths::default_root());
    let _ = config;
    paths.root().join("workspace")
}

/// `ka-agent setup …` entry point (main.rs runs it on a current-thread
/// runtime; blocking prompts are fine because nothing else runs meanwhile).
pub async fn run(args: SetupArgs) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    if let Err(e) = paths.ensure_dirs() {
        eprintln!("ka-agent: cannot create {}: {e}", paths.root().display());
        return std::process::ExitCode::FAILURE;
    }
    let fresh = !args.config.exists();
    let doc = match ka_agent_core::config_file::read_doc(&args.config) {
        Ok(mut d) => {
            // A new non-default profile must not fight the default one for
            // the web panel's port.
            let profile = ka_agent_core::layout::profile_name();
            if fresh
                && profile != "default"
                && args.config == ka_agent_core::layout::config_path_for(&profile)
            {
                d["web"]["port"] =
                    toml_edit::value(i64::from(crate::profile_cli::unclaimed_web_port()));
            }
            d
        }
        Err(e) => {
            eprintln!("ka-agent: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut setup = Setup {
        args,
        paths,
        doc,
        fresh,
        provider: None,
        key: None,
        key_from: "none",
        live_models: Vec::new(),
        model: None,
        timezone: None,
        workspace: None,
        telegram_token: None,
        telegram_user: None,
        matrix: None,
        matrix_operator: None,
        search: None,
        briefing: None,
        voice: None,
    };
    setup.run().await
}
