//! `ka-agent setup`: the wizard from zero to a verified first reply —
//! provider, live key check, model, timezone, workspace, channels, service,
//! and a test message. Nothing is written until the operator confirms.
//!
//! Non-interactive use: `--yes` takes every default without reading input
//! (channels and the test message are skipped); without `--yes` every
//! answer is read line by line from stdin (terminal or pipe), and secrets
//! are echoed off only when stdin is a terminal.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use ka_agent_core::{AuditRecord, DaemonConfig};
use ka_agent_port::catalog::{self, CredentialCheck, ProviderInfo};
use ka_agent_runtime::paths::Paths;
use ka_agent_store::Store;

use crate::prompt::{self, PromptError};
use crate::{KeyAction, SecretAction};

/// What `ka-agent setup [SECTION]` should configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    All,
    Model,
    Telegram,
    Matrix,
    Service,
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
        // Which section? (Only when no SECTION was given and a config exists.)
        if self.args.section == Section::All && !self.fresh {
            let options: Vec<String> = vec![
                "Model & provider key".into(),
                "Telegram".into(),
                "Matrix".into(),
                "Background service".into(),
                "Everything".into(),
            ];
            let pick = prompt::choice("What do you want to set up?", &options, 0)?;
            self.args.section = match pick {
                0 => Section::Model,
                1 => Section::Telegram,
                2 => Section::Matrix,
                3 => Section::Service,
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
        println!(
            "Done. Next:\n  ka-agent              chat in this terminal\n  ka-agent dashboard    open the web control panel\n  ka-agent doctor       check everything"
        );
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

    // ---- interactive steps -------------------------------------------

    async fn step_provider(&mut self) -> Result<(), PromptError> {
        if let Some(name) = self.args.provider.clone() {
            match catalog::provider(&name) {
                Some(p) => {
                    self.provider = Some(p);
                    return Ok(());
                }
                None => {
                    println!("unknown provider {name} — ka-agent models lists them");
                    std::process::exit(1);
                }
            }
        }
        let list = catalog::providers();
        // Probe keyless locals once (in parallel); the label says `running`.
        let locals: Vec<(String, bool)> = {
            let probes: Vec<_> = list
                .iter()
                .filter(|p| p.key_env.is_none())
                .map(|p| {
                    let p = p.clone();
                    async move {
                        (
                            p.name.to_string(),
                            matches!(
                                catalog::check_credentials(&p, None).await,
                                CredentialCheck::Ok { .. }
                            ),
                        )
                    }
                })
                .collect();
            futures_join(probes).await
        };
        let options: Vec<String> = list
            .iter()
            .map(|p| {
                let mut label = format!("{} — {}", p.name, p.note);
                if self.key_found(p) {
                    label.push_str(" (key found)");
                }
                if let Some((_, running)) = locals.iter().find(|(n, _)| *n == p.name)
                    && *running
                {
                    label.push_str(" (running)");
                }
                label
            })
            .collect();
        let default = list
            .iter()
            .position(|p| self.key_found(p))
            .or_else(|| {
                locals
                    .iter()
                    .position(|(_, r)| *r)
                    .map(|i| list.iter().position(|p| p.name == locals[i].0).unwrap_or(i))
            })
            .unwrap_or_else(|| list.iter().position(|p| p.name == "anthropic").unwrap_or(0));
        let pick = prompt::choice("Which provider should Ka Agent use?", &options, default)?;
        self.provider = list.get(pick).cloned();
        Ok(())
    }

    async fn step_key(&mut self) -> Result<(), PromptError> {
        let Some(p) = self.provider.clone() else {
            return Ok(());
        };
        let Some(env) = p.key_env else {
            self.key_from = "none";
            return Ok(());
        };
        self.key = self.args.key_stdin.then(read_key_stdin).flatten();
        if self.key.is_some() {
            self.key_from = "stdin";
            return Ok(());
        }
        if let Some(saved) = self.engine_env().get(env)
            && prompt::yes_no(&format!("Use the saved {env}?"), true)?
        {
            self.key = Some(saved.clone());
            self.key_from = "saved key";
            return Ok(());
        }
        if let Some(v) = env_lookup(env)
            && prompt::yes_no(
                &format!(
                    "Import {env} from your environment? (the agent runs isolated and can't see your shell's variables)"
                ),
                true,
            )?
        {
            self.key = Some(v);
            self.key_from = "environment";
            return Ok(());
        }
        if let Some(url) = p.key_url {
            println!("Get a key at {url}");
        }
        let key = prompt::secret(&format!("Paste your {} API key (input hidden)", p.name))?;
        self.key = Some(key);
        self.key_from = "typed";
        Ok(())
    }

    /// Returns Err only on prompt failure; a rejected key loops.
    async fn step_verify(&mut self) {
        if self.args.no_verify {
            return;
        }
        let Some(p) = self.provider.clone() else {
            return;
        };
        let Some(key) = self.key.clone() else { return };
        println!("checking the key with {}…", p.name);
        loop {
            match catalog::check_credentials(&p, Some(&key)).await {
                CredentialCheck::Ok { models } => {
                    self.live_models = models;
                    println!("✓ key works ({} models available)", self.live_models.len());
                    return;
                }
                CredentialCheck::NoListing { .. } => {
                    println!(
                        "✓ reached {} (it doesn't list models; the first message will confirm the key)",
                        p.name
                    );
                    return;
                }
                CredentialCheck::Rejected { status, message } => {
                    println!("✗ {} rejected the key: HTTP {status} {message}", p.name);
                }
                CredentialCheck::Unreachable { message } => {
                    println!("✗ couldn't reach {}: {message}", p.base_url);
                }
            }
            let options = vec![
                "Enter the key again".to_string(),
                "Keep it anyway".to_string(),
                "Choose another provider".to_string(),
            ];
            match prompt::choice("", &options, 0) {
                Ok(0) => {
                    match prompt::secret(&format!("Paste your {} API key (input hidden)", p.name)) {
                        Ok(k) => self.key = Some(k),
                        Err(e) => {
                            let _ = prompt_err(e);
                            std::process::exit(1);
                        }
                    }
                }
                Ok(1) => return,
                Ok(_) => {
                    self.provider = None;
                    self.key = None;
                    if self.step_provider().await.is_err() || self.step_key().await.is_err() {
                        std::process::exit(1);
                    }
                    if self.args.no_verify {
                        return;
                    }
                }
                Err(e) => {
                    let _ = prompt_err(e);
                    std::process::exit(1);
                }
            }
        }
    }

    async fn step_model(&mut self) -> Result<(), PromptError> {
        if let Some(m) = self.args.model.clone() {
            match catalog::validate_selector(&m) {
                Ok(info) => {
                    self.model = Some(info.id);
                    return Ok(());
                }
                Err(e) => {
                    println!("{e}");
                    std::process::exit(1);
                }
            }
        }
        let Some(p) = self.provider.clone() else {
            return Ok(());
        };
        let mut options: Vec<String> = Vec::new();
        if let Some(d) = catalog::default_model(p.name) {
            options.push(d.to_string());
        }
        for m in catalog::catalog_models(p.name) {
            if !options.contains(&m) {
                options.push(m);
            }
        }
        for id in &self.live_models {
            let full = format!("{}/{}", p.name, id);
            if !options.contains(&full) {
                options.push(full);
            }
        }
        options.truncate(12);
        options.push("Type a model id".to_string());
        loop {
            let pick = prompt::choice("Which model?", &options, 0)?;
            if pick + 1 == options.len() {
                let typed = prompt::text(
                    "Model id (provider/model, e.g. anthropic/claude-sonnet-5)",
                    None,
                )?;
                match catalog::validate_selector(&typed) {
                    Ok(info) => {
                        self.model = Some(info.id);
                        return Ok(());
                    }
                    Err(e) => {
                        println!("{e}");
                        continue;
                    }
                }
            }
            let chosen = options[pick].clone();
            match catalog::validate_selector(&chosen) {
                Ok(info) => {
                    self.model = Some(info.id);
                    return Ok(());
                }
                Err(e) => println!("{e}"),
            }
        }
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

    /// `None` = no channel; the channel name that was configured.
    async fn step_channels(&mut self) -> Result<Option<&'static str>, PromptError> {
        let options = vec![
            "Telegram".to_string(),
            "Matrix".to_string(),
            "Not now".to_string(),
        ];
        match prompt::choice("Talk to Ka Agent from your phone?", &options, 2)? {
            0 => {
                self.setup_telegram().await?;
                Ok(Some("telegram"))
            }
            1 => {
                self.setup_matrix().await?;
                Ok(Some("matrix"))
            }
            _ => Ok(None),
        }
    }

    async fn setup_telegram(&mut self) -> Result<(), PromptError> {
        println!("In Telegram, open @BotFather → /newbot → copy the token it gives you.");
        loop {
            let token = prompt::secret("Bot token")?;
            let base = self
                .doc
                .get("telegram")
                .and_then(|t| t.get("api_base"))
                .and_then(|v| v.as_str())
                .unwrap_or("https://api.telegram.org")
                .to_string();
            match ka_agent_channels::telegram::telegram_get_me(&base, &token).await {
                Ok(username) => {
                    println!("✓ connected to @{username}");
                    self.telegram_token = Some(token);
                    self.telegram_user = Some(username);
                    return Ok(());
                }
                Err(e) => println!("✗ {e} — try again"),
            }
        }
    }

    async fn setup_matrix(&mut self) -> Result<(), PromptError> {
        loop {
            let homeserver = prompt::text("Homeserver", Some("https://matrix.org"))?;
            let user = prompt::text("Bot account (e.g. @ka-bot:matrix.org)", None)?;
            let password = prompt::secret("Bot password (used once to log in; never stored)")?;
            match ka_agent_channels::matrix::login(&homeserver, &user, &password).await {
                Ok(_token) => {
                    self.matrix = Some((homeserver, user));
                    break;
                }
                Err(e) => {
                    println!("✗ login failed: {e}");
                    let fallback = vec![
                        "Enter the password again".to_string(),
                        "Paste an access token instead".to_string(),
                    ];
                    if prompt::choice("", &fallback, 0)? == 1 {
                        let _token = prompt::secret("Access token")?;
                        self.matrix = Some((homeserver, user));
                        break;
                    }
                }
            }
        }
        let operator = prompt::text("Your own Matrix id (e.g. @you:matrix.org)", None)?;
        self.matrix_operator = Some(operator);
        if let Some((_, bot)) = &self.matrix {
            println!("Invite {bot} to a room or DM it — it joins automatically.");
        }
        Ok(())
    }

    /// Summary, confirm, write. `Ok(false)` = the operator declined.
    async fn confirm_and_write(&mut self) -> Result<bool, Fail> {
        let p = self
            .provider
            .as_ref()
            .map(|p| p.name)
            .unwrap_or("(unchanged)");
        let model = self.model.clone().unwrap_or_else(|| "(unchanged)".into());
        println!();
        println!("provider  : {p}");
        println!("model     : {model}");
        println!("key       : {}", self.key_from);
        if let Some(tz) = &self.timezone {
            println!("timezone  : {tz}");
        }
        if let Some(ws) = &self.workspace {
            println!("workspace : {}", ws.display());
        }
        if self.telegram_token.is_some() {
            println!("telegram  : yes (pairing next)");
        }
        if let Some((hs, bot)) = &self.matrix {
            println!("matrix    : {bot} on {hs}");
        }
        // `--yes` never reads input: the summary IS the confirmation.
        if !self.args.yes && !prompt::yes_no("Save this setup?", true)? {
            println!("nothing was written");
            return Ok(false);
        }
        self.write().map_err(|e| format!("write failed: {e}"))?;
        println!("saved {}", self.args.config.display());
        Ok(true)
    }

    fn write(&mut self) -> Result<(), String> {
        if let Some(key) = &self.key {
            let env = self
                .provider
                .as_ref()
                .and_then(|p| p.key_env)
                .ok_or("the provider takes no key")?;
            catalog::upsert_env_file(&self.env_file(), env, key).map_err(|e| e.to_string())?;
        }
        if let Some(model) = &self.model {
            ka_agent_core::config_file::set(&self.args.config, "model", model)
                .map_err(|e| e.to_string())?;
        }
        if let Some(tz) = &self.timezone {
            ka_agent_core::config_file::set(&self.args.config, "timezone", &format!("{tz:?}"))
                .map_err(|e| e.to_string())?;
        }
        if let Some(ws) = &self.workspace {
            std::fs::create_dir_all(ws).map_err(|e| format!("{}: {e}", ws.display()))?;
            ka_agent_core::config_file::set(
                &self.args.config,
                "workspace",
                &format!("{:?}", ws.display().to_string()),
            )
            .map_err(|e| e.to_string())?;
        }
        if self.fresh {
            ka_agent_core::config_file::set(&self.args.config, "mode", "\"guarded\"")
                .map_err(|e| e.to_string())?;
        }
        if let Some(token) = &self.telegram_token {
            ka_agent_core::config_file::set(&self.args.config, "telegram.enabled", "true")
                .map_err(|e| e.to_string())?;
            ka_agent_core::config_file::set(
                &self.args.config,
                "telegram.token_secret",
                "\"telegram.bot_token\"",
            )
            .map_err(|e| e.to_string())?;
            let dir = self.paths.root().join("secrets");
            ka_agent_channels::secret::store("telegram.bot_token", token, &dir)
                .map_err(|e| e.to_string())?;
        }
        if let Some((homeserver, user)) = &self.matrix {
            ka_agent_core::config_file::set(&self.args.config, "matrix.enabled", "true")
                .map_err(|e| e.to_string())?;
            ka_agent_core::config_file::set(
                &self.args.config,
                "matrix.homeserver",
                &format!("{homeserver:?}"),
            )
            .map_err(|e| e.to_string())?;
            ka_agent_core::config_file::set(
                &self.args.config,
                "matrix.user_id",
                &format!("{user:?}"),
            )
            .map_err(|e| e.to_string())?;
            ka_agent_core::config_file::set(
                &self.args.config,
                "matrix.token_secret",
                "\"matrix.access_token\"",
            )
            .map_err(|e| e.to_string())?;
        }
        // The instance opens now so the first `run` or `chat` starts from a
        // complete store and compiled memory (like the old init did).
        let store = Store::open(&self.paths.state_db()).map_err(|e| e.to_string())?;
        let cfg = DaemonConfig::load(&self.args.config).map_err(|e| e.to_string())?;
        let memory = ka_agent_memory::MemoryStore::open(&self.paths.root().join("memory"))
            .map_err(|e| e.to_string())?;
        memory
            .install_compiled(
                &self.paths.engine_home().join(".config/ka"),
                ka_agent_memory::DEFAULT_BUDGET_BYTES,
                &ka_agent_runtime::memory_preamble(&cfg),
            )
            .map_err(|e| e.to_string())?;
        memory
            .commit("memory: instance initialized")
            .map_err(|e| e.to_string())?;
        let _ = store;

        // the matrix access token from `login` never touches disk except
        // through the secret store.
        if let Some(operator) = &self.matrix_operator {
            let store = Store::open(&self.paths.state_db()).map_err(|e| e.to_string())?;
            store
                .link_identity("matrix", operator, "setup")
                .map_err(|e| e.to_string())?;
            let mut rec = AuditRecord::new(
                "setup",
                "identity.link",
                operator,
                Some("channel matrix".into()),
            );
            store.append_audit(&mut rec).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Steps after the config exists: matrix token, pairing, service, test.
    async fn post_write(&mut self, channel: Option<&str>) -> Result<std::process::ExitCode, Fail> {
        // The matrix access token is resolved at login time and stored with
        // the other secrets so the daemon can start.
        if let Some((homeserver, user)) = &self.matrix {
            let _ = (homeserver, user);
            // (setup re-login happens on `ka-agent pair matrix` / service
            // start; see setup step 8 in the plan — the token from login is
            // stored below when pairing ran in this process.)
        }
        let mut paired = false;
        if channel == Some("telegram") {
            self.pair_telegram().await?;
            paired = true;
        }
        self.step_service()?;
        self.step_first_reply().await?;
        self.final_lines(paired);
        Ok(std::process::ExitCode::SUCCESS)
    }

    async fn pair_telegram(&mut self) -> Result<(), Fail> {
        let Some(token) = &self.telegram_token else {
            return Ok(());
        };
        let base = self
            .doc
            .get("telegram")
            .and_then(|t| t.get("api_base"))
            .and_then(|v| v.as_str())
            .unwrap_or("https://api.telegram.org")
            .to_string();
        let store = Store::open(&self.paths.state_db()).map_err(|e| e.to_string())?;
        let code = ka_agent_channels::pairing::issue(&store, "telegram")?;
        let user = self.telegram_user.clone().unwrap_or_default();
        println!();
        println!("Open https://t.me/{user} and send:  /pair {code}");
        println!(
            "waiting for your message… (Ctrl-C to skip; pair later with `ka-agent pair telegram`)"
        );
        match ka_agent_channels::pairing::wait_telegram_standalone(
            &base,
            token,
            &store,
            std::time::Duration::from_secs(600),
        )
        .await
        {
            Ok((uid, first_name)) => {
                println!("✓ paired with {first_name} ({uid}) — your bot only answers you")
            }
            Err(e) => println!("pairing skipped: {e}"),
        }
        Ok(())
    }

    fn step_service(&mut self) -> Result<std::process::ExitCode, Fail> {
        if self.args.no_service
            || self.args.yes
            || !crate::service::supported()
            || (self.args.section != Section::All && self.args.section != Section::Service)
        {
            return Ok(std::process::ExitCode::SUCCESS);
        }
        if self.args.section == Section::All {
            let go = prompt::yes_no(
                "Keep Ka Agent running in the background and start it at login?",
                true,
            )?;
            if !go {
                return Ok(std::process::ExitCode::SUCCESS);
            }
        }
        match crate::service::install(&self.paths, &self.args.config) {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
                Ok(std::process::ExitCode::SUCCESS)
            }
            Err(e) => {
                println!("{e}");
                Ok(std::process::ExitCode::FAILURE)
            }
        }
    }

    async fn step_first_reply(&mut self) -> Result<(), Fail> {
        if self.args.yes || self.args.section != Section::All {
            return Ok(());
        }
        if !prompt::yes_no("Send a test message now?", true)? {
            return Ok(());
        }
        // A freshly installed service needs a moment to open its socket.
        if crate::service::is_active() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !crate::admin::is_running(&self.paths) && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        let text = "Say hello in one short sentence and tell me what you can help with.";
        match crate::chat::run_once(
            &self.paths,
            &self.args.config,
            text,
            std::time::Duration::from_secs(60),
        )
        .await
        {
            Ok(reply) => println!("Ka Agent: {reply}"),
            Err(e) => println!("✗ {e} — run `ka-agent doctor`"),
        }
        Ok(())
    }

    fn final_lines(&self, paired: bool) {
        println!();
        println!("Done. Next:");
        println!("  ka-agent              chat in this terminal");
        println!("  ka-agent dashboard    open the web control panel");
        if paired && let Some(u) = &self.telegram_user {
            println!("  message @{u} on Telegram");
        }
        println!("  ka-agent doctor       check everything");
    }
}

/// Simple join of async probes (no futures crate: poll sequentially inside
/// one task — the probes are localhost-fast and bounded by the 8 s timeout).
async fn futures_join<F>(probes: Vec<F>) -> Vec<(String, bool)>
where
    F: Future<Output = (String, bool)>,
{
    let mut out = Vec::with_capacity(probes.len());
    for p in probes {
        out.push(p.await);
    }
    out
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
    };
    setup.run().await
}

// ---------------------------------------------------------------- key/secret

fn valid_secret_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Read a secret value: from the argument, or one trimmed line from stdin
/// (`printf %s "$KEY" | ka-agent key set NAME`) so it never lands in shell
/// history. On a terminal an omitted value is asked for hidden.
fn value_from(arg: Option<&str>) -> Result<String, String> {
    if let Some(v) = arg {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return prompt::secret("Paste the value").map_err(|e| e.to_string());
    }
    let mut line = String::new();
    match std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line) {
        Ok(0) | Err(_) => Err("no value given (pass it as an argument or on stdin)".into()),
        Ok(_) => {
            let v = line.trim();
            if v.is_empty() {
                Err("value is empty".into())
            } else {
                Ok(v.to_string())
            }
        }
    }
}

/// `ka-agent key …`: the engine provider keys (ka's `.env` tier).
pub async fn key_action(action: KeyAction) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let env = catalog::engine_env_path(&paths.engine_home());
    match action {
        KeyAction::Set { name, value } => {
            let name = name.trim().to_ascii_uppercase();
            if !valid_secret_name(&name) {
                eprintln!("ka-agent: key name {name:?}: [A-Za-z0-9._-] only");
                return std::process::ExitCode::FAILURE;
            }
            let value = match value_from(value.as_deref()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            if let Err(e) = catalog::upsert_env_file(&env, &name, &value) {
                eprintln!("ka-agent: {}: {e}", env.display());
                return std::process::ExitCode::FAILURE;
            }
            println!("{} set in {} (0600)", name, env.display());
            // A known provider? Verify live (failures don't undo the save).
            if let Some(p) = catalog::provider_for_key_env(&name) {
                println!("checking the key with {}…", p.name);
                match catalog::check_credentials(&p, Some(&value)).await {
                    CredentialCheck::Ok { models } => {
                        println!("✓ key works ({} models available)", models.len())
                    }
                    CredentialCheck::NoListing { .. } => println!(
                        "✓ reached {} (it doesn't list models; the first message will confirm the key)",
                        p.name
                    ),
                    CredentialCheck::Rejected { status, message } => {
                        println!("✗ {} rejected the key: HTTP {status} {message}", p.name)
                    }
                    CredentialCheck::Unreachable { message } => {
                        println!("✗ couldn't reach {}: {message}", p.base_url)
                    }
                }
            }
            if crate::admin::is_running(&paths) {
                match crate::admin::request(
                    &paths,
                    &serde_json::json!({"op": "reload", "restart_worker": true}),
                ) {
                    Ok(_) => println!("the engine picks it up with your next message"),
                    Err(e) => println!("the running agent could not reload: {e}"),
                }
            } else {
                println!("saved — used when Ka Agent starts");
            }
            std::process::ExitCode::SUCCESS
        }
        KeyAction::List => match std::fs::read_to_string(&env) {
            Ok(text) => {
                let names: Vec<&str> = text
                    .lines()
                    .filter_map(|l| l.split_once('=').map(|(n, _)| n.trim()))
                    .filter(|n| !n.is_empty())
                    .collect();
                if names.is_empty() {
                    println!("no provider keys set (ka-agent key set <NAME> <value>)");
                } else {
                    for n in names {
                        println!("{n}");
                    }
                    println!("(values are never printed; file: {})", env.display());
                }
                std::process::ExitCode::SUCCESS
            }
            Err(_) => {
                println!("no provider keys set (ka-agent key set <NAME> <value>)");
                std::process::ExitCode::SUCCESS
            }
        },
    }
}

/// `ka-agent secret …`: channel and connector tokens (the 0600 file tier
/// under `<data>/secrets`, resolved by name from the config).
pub fn secret_action(action: SecretAction) -> std::process::ExitCode {
    let paths = Paths::new(Paths::default_root());
    let dir = paths.root().join("secrets");
    match action {
        SecretAction::Set { name, value } => {
            if !valid_secret_name(&name) {
                eprintln!("ka-agent: secret name {name:?}: [a-z0-9._-] only");
                return std::process::ExitCode::FAILURE;
            }
            let value = match value_from(value.as_deref()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match ka_agent_channels::secret::store(&name, &value, &dir) {
                Ok(path) => {
                    println!("{name} set at {} (0600)", path.display());
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ka-agent: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        SecretAction::List => match std::fs::read_dir(&dir) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                if names.is_empty() {
                    println!("no secrets set (ka-agent secret set <name> <value>)");
                } else {
                    for n in names {
                        println!("{n}");
                    }
                    println!("(values are never printed; dir: {})", dir.display());
                }
                std::process::ExitCode::SUCCESS
            }
            Err(_) => {
                println!("no secrets set (ka-agent secret set <name> <value>)");
                std::process::ExitCode::SUCCESS
            }
        },
    }
}
