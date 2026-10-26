//! Setup config writing and the post-write steps (service, pairing,
//! first reply, final card). Split from setup.rs (moves only).

use super::*;

impl Setup {
    /// Summary, confirm, write. `Ok(false)` = the operator declined.
    pub(super) async fn confirm_and_write(&mut self) -> Result<bool, Fail> {
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
        if let Some((provider, _)) = &self.search {
            println!("search    : {provider}");
        }
        if let Some(t) = &self.briefing {
            println!("briefing  : every day at {t}");
        }
        if let Some((v, _)) = &self.voice {
            println!("voice     : {}", v.provider);
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
        if let Some((voice, key)) = &self.voice {
            if let (Some(key), Some(env)) = (key, voice.key_env()) {
                catalog::upsert_env_file(&self.env_file(), env, key).map_err(|e| e.to_string())?;
            }
            if let Some(base) = &voice.base_url {
                ka_agent_core::config_file::set(
                    &self.args.config,
                    "voice.base_url",
                    &format!("{base:?}"),
                )
                .map_err(|e| e.to_string())?;
            }
            ka_agent_core::config_file::set(
                &self.args.config,
                "voice.provider",
                &format!("{:?}", voice.provider),
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some((provider, key)) = &self.search {
            if let Some(key) = key {
                let env = ka_agent_core::Search {
                    provider: provider.clone(),
                    base_url: None,
                }
                .key_env();
                catalog::upsert_env_file(&self.env_file(), &env, key).map_err(|e| e.to_string())?;
            }
            ka_agent_core::config_file::set(
                &self.args.config,
                "search.provider",
                &format!("{provider:?}"),
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(token) = &self.telegram_token {
            // `token_secret` before `enabled`: every config_file::set
            // validates the whole file, and `enabled = true` without a
            // secret name is invalid — the wizard used to abort right
            // after accepting a perfectly good token.
            ka_agent_core::config_file::set(
                &self.args.config,
                "telegram.token_secret",
                "\"telegram.bot_token\"",
            )
            .map_err(|e| e.to_string())?;
            ka_agent_core::config_file::set(&self.args.config, "telegram.enabled", "true")
                .map_err(|e| e.to_string())?;
            let dir = self.paths.root().join("secrets");
            ka_agent_core::secret::store("telegram.bot_token", token, &dir)
                .map_err(|e| e.to_string())?;
        }
        if let Some((homeserver, user)) = &self.matrix {
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
            ka_agent_core::config_file::set(&self.args.config, "matrix.enabled", "true")
                .map_err(|e| e.to_string())?;
        }
        // The instance opens now so the first `run` or `chat` starts from a
        // complete store and compiled memory (like the old init did).
        let store = Store::open(&self.paths.state_db()).map_err(|e| e.to_string())?;
        let cfg = DaemonConfig::load(&self.args.config).map_err(|e| e.to_string())?;
        if let Some(t) = &self.briefing {
            let now = jiff::Timestamp::now().to_zoned(cfg.time_zone());
            ka_agent_runtime::briefing::enable(&store, t, &now)?;
        }
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
    pub(super) async fn post_write(
        &mut self,
        channel: Option<&str>,
    ) -> Result<std::process::ExitCode, Fail> {
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

    pub(super) fn step_service(&mut self) -> Result<std::process::ExitCode, Fail> {
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

    pub(super) async fn step_first_reply(&mut self) -> Result<(), Fail> {
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

    pub(super) fn final_lines(&self, paired: bool) {
        print!("{}", self.live_card(paired));
    }

    /// The end of setup: where to talk, what to try (only what this
    /// install can actually do), where to look when something is off.
    fn name(&self) -> String {
        let path = ka_agent_core::layout::base_dir()
            .join("memory")
            .join("persona.md");
        std::fs::read_to_string(path)
            .ok()
            .map(|t| ka_agent_memory::persona::persona_name(&t))
            .unwrap_or_else(|| "Ka".to_string())
    }

    pub(super) fn live_card(&self, paired: bool) -> String {
        let name = self.name();
        let mut out = format!(
            "\n☥ {name} is awake.\n\nTalk to {name}:\n  ka-agent             in this terminal\n"
        );
        if paired && let Some(u) = &self.telegram_user {
            out.push_str(&format!("  @{u}  on Telegram\n"));
        }
        if self.matrix.is_some() {
            out.push_str("  your Matrix room\n");
        }
        out.push_str("  ka-agent dashboard   in the browser\n\nTry:\n");
        out.push_str("  \"remind me at 18:30 to call mum\"\n");
        out.push_str("  \"remember that I prefer short answers\"\n");
        let search_on = self.search.is_some()
            || self
                .doc
                .get("search")
                .and_then(|s| s.get("provider"))
                .is_some();
        if search_on {
            out.push_str("  \"search the web for this week's tech news\"\n");
        }
        out.push_str("  /cron add every day at 08:00 summarise my notes\n");
        out.push_str("\nHealth: ka-agent status · ka-agent doctor\n");
        out
    }
}
