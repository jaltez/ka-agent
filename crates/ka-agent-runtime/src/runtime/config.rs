// Configuration: reload + report, the memory preamble, timezone publish.
// Split from runtime.rs (moves only).
use super::*;

/// What a config reload changed: applied to the running agent, or waiting
/// for a restart.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ReloadReport {
    pub applied: Vec<String>,
    pub restart_required: Vec<String>,
}

/// The operator zone for out-of-process readers (the MCP bridge
/// schedules local-time crons with it). Best effort: a failed write only
/// means the bridge falls back to the system zone.
pub(super) fn publish_timezone(store: &Store, cfg: &DaemonConfig) {
    if let Some(name) = cfg.time_zone().iana_name() {
        let _ = store.meta_set(ka_agent_core::layout::TIMEZONE_META_KEY, name);
    }
}

/// The memory preamble tells the model which zone to schedule in: every
/// clock time the operator writes is local, never UTC.
pub fn memory_preamble(_cfg: &DaemonConfig) -> String {
    // The operator's clock context rides the per-turn `<ctx .../>` line
    // (step 2.2); the compiled MEMORY.md carries facts only.
    String::new()
}

impl Runtime {
    /// Swap in a new configuration. Settings the runtime reads per use apply
    /// immediately; channel, workspace and web settings need a restart. A
    /// changed model/mode/engine layer (or `restart_worker`, after a key
    /// change) stops an idle worker so the next message starts a fresh one
    /// with the new settings; a running turn finishes on the old ones.
    pub async fn reload_config(&self, new: DaemonConfig, restart_worker: bool) -> ReloadReport {
        let old = self.cfg();
        let changed = ka_agent_core::config_keys::changed(&old, &new);
        let report = ReloadReport {
            applied: changed
                .iter()
                .filter(|k| k.apply != ka_agent_core::config_keys::Apply::Restart)
                .map(|k| k.path.to_string())
                .collect(),
            restart_required: changed
                .iter()
                .filter(|k| k.apply == ka_agent_core::config_keys::Apply::Restart)
                .map(|k| k.path.to_string())
                .collect(),
        };
        // Worker-class keys shape the engine the worker was spawned
        // with: stop an idle one so the next message starts fresh (a
        // running turn finishes on the old settings).
        let engine_changed = changed
            .iter()
            .any(|k| k.apply == ka_agent_core::config_keys::Apply::Worker);
        {
            let mut g = self.lock();
            g.budget.daily_usd_cap = new.budgets.daily_usd;
            g.budget.daily_token_cap = new.budgets.daily_tokens;
        }
        publish_timezone(&self.sh.store, &new);
        *self.sh.cfg.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(new);
        // remember the file's current mtime: the hot-reload watcher must
        // not re-announce this swap (config set / IPC reload land here)
        self.remember_config_mtime();
        if restart_worker || engine_changed {
            self.stop_idle_worker().await;
        }
        report
    }

    /// Record the config file's current mtime as "seen" (called by every
    /// reload path), and return the previous value for the watcher.
    fn remember_config_mtime(&self) {
        let Some(path) = self.config_path() else {
            return;
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        *self
            .sh
            .config_mtime
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = mtime;
    }

    /// One hot-reload check (each maintenance tick): a config file whose
    /// mtime moved is reloaded — a valid file applies live (with one
    /// notice naming what changed), an invalid one keeps the running
    /// settings and notices the error once per distinct text.
    pub async fn watch_config_file(&self) {
        let Some(path) = self.config_path() else {
            return;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            return;
        };
        let Ok(mtime) = meta.modified() else {
            return;
        };
        let seen = *self
            .sh
            .config_mtime
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if seen == Some(mtime) {
            return;
        }
        match DaemonConfig::load(&path) {
            Ok(new) => {
                let report = self.reload_config(new, false).await;
                let mut line = String::from("config reloaded");
                if !report.applied.is_empty() {
                    line.push_str(&format!(" · applied: {}", report.applied.join(", ")));
                }
                if !report.restart_required.is_empty() {
                    line.push_str(&format!(
                        " · restart needed: {}",
                        report.restart_required.join(", ")
                    ));
                }
                self.sh.emit(AgentEvent::Notice { text: line });
                // persist the last good reload for doctor/status
                let _ = self.sh.store.meta_set("config.reload_error", "");
            }
            Err(e) => {
                let msg =
                    format!("config.toml has an error — still running the previous settings: {e}");
                let already = self
                    .sh
                    .store
                    .meta_get("config.reload_error")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                if already != msg {
                    self.sh.emit(AgentEvent::Notice { text: msg.clone() });
                    let _ = self.sh.store.meta_set("config.reload_error", &msg);
                }
                // do not reload, and do not re-check until the file moves
                // again: park the mtime so the notice fires once
                *self
                    .sh
                    .config_mtime
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(mtime);
            }
        }
    }

    /// Compile the persona into the engine's IDENTITY.md (re-read per
    /// turn by `[prompt] identity_file`). Called at startup, after a
    /// persona edit, and from the maintenance watcher.
    pub fn refresh_identity(&self) -> Result<(), Error> {
        let persona = self.sh.memory.persona_text();
        let text = crate::runtime::compile_identity(&persona);
        let dir = self.sh.paths.engine_home().join(".config/ka");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("IDENTITY.md"), text)?;
        self.remember_persona_mtime();
        Ok(())
    }

    /// The persona's display name (`Ka` by default).
    pub fn persona_name(&self) -> String {
        self.sh.memory.persona_name()
    }

    /// The current persona text.
    pub fn persona_text(&self) -> String {
        self.sh.memory.persona_text()
    }

    /// Persist a new persona and recompile IDENTITY.md in-process.
    pub fn set_persona(&self, text: &str) -> Result<(), Error> {
        self.sh.memory.write_persona(text)?;
        self.refresh_identity()
    }

    fn persona_path(&self) -> std::path::PathBuf {
        self.sh.paths.root().join("memory/persona.md")
    }

    fn remember_persona_mtime(&self) {
        let mtime = std::fs::metadata(self.persona_path())
            .and_then(|m| m.modified())
            .ok();
        *self
            .sh
            .persona_mtime
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = mtime;
    }

    /// One persona watch (each maintenance tick): a hand-edited
    /// persona.md recompiles IDENTITY.md; the next turn speaks as the new
    /// persona without a restart.
    pub async fn watch_persona(&self) {
        let Ok(meta) = std::fs::metadata(self.persona_path()) else {
            return;
        };
        let Ok(mtime) = meta.modified() else {
            return;
        };
        let seen = *self
            .sh
            .persona_mtime
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if seen == Some(mtime) {
            return;
        }
        if let Err(e) = self.refresh_identity() {
            self.sh.emit(crate::event::AgentEvent::Error {
                class: "internal".into(),
                message: format!("persona: {e}"),
            });
        }
    }

    /// The model's provider key state against the engine `.env` (the
    /// same file the worker reads): `None` when no model is configured.
    pub fn model_key_state(&self) -> Result<Option<(String, ModelKeyState)>, Error> {
        let Some(model) = self.cfg().model.clone() else {
            return Ok(None);
        };
        let sel = ka_agent_port::catalog::validate_selector(&model).map_err(Error::Refused)?;
        let env = ka_agent_port::catalog::read_env_file(&ka_agent_port::catalog::engine_env_path(
            &self.sh.paths.engine_home(),
        ));
        let state = match ka_agent_port::catalog::key_state(&sel, &env) {
            ka_agent_port::catalog::KeyState::Present => ModelKeyState::Present,
            ka_agent_port::catalog::KeyState::Missing(name) => ModelKeyState::Missing(name),
            ka_agent_port::catalog::KeyState::Keyless => ModelKeyState::Keyless,
        };
        Ok(Some((sel.id, state)))
    }

    /// `/model <provider/model>`: validate, save to the config file, apply
    /// live. Returns the normalized selector for the reply.
    pub async fn set_model(&self, sel: &str) -> Result<String, Error> {
        let info = ka_agent_port::catalog::validate_selector(sel).map_err(Error::Refused)?;
        let path = self
            .config_path()
            .ok_or_else(|| Error::Refused("no config file — run ka-agent setup".into()))?;
        ka_agent_core::config_file::set(&path, "model", &info.id).map_err(Error::Config)?;
        let new = DaemonConfig::load(&path)?;
        self.reload_config(new, false).await;
        Ok(info.id)
    }

    /// `/mode`: switch the permission mode, persisted and applied to a
    /// live worker at once. From a phone channel the mode may only get
    /// stricter (free < accept_edits < guarded < plan): a lost phone must
    /// not be able to turn approvals off.
    pub async fn set_mode(&self, mode: &str, source: &str) -> Result<String, Error> {
        fn strictness(m: &str) -> Option<u8> {
            Some(match m {
                "free" => 0,
                "accept_edits" => 1,
                "guarded" => 2,
                "plan" => 3,
                _ => return None,
            })
        }
        let mode = mode.trim();
        let new = strictness(mode).ok_or_else(|| {
            Error::Refused(format!(
                "unknown mode {mode:?}: guarded | accept_edits | plan | free"
            ))
        })?;
        let phone = source.starts_with("telegram:") || source.starts_with("matrix:");
        let current = strictness(&self.cfg().mode).unwrap_or(2);
        if phone && new < current {
            return Err(Error::Refused(
                "from the phone the mode can only get stricter — loosen it from the terminal or web".into(),
            ));
        }
        let path = self
            .config_path()
            .ok_or_else(|| Error::Refused("no config file — run ka-agent setup".into()))?;
        ka_agent_core::config_file::set(&path, "mode", &format!("{mode:?}"))
            .map_err(Error::Config)?;
        let mut new_cfg = (*self.cfg()).clone();
        new_cfg.mode = mode.to_string();
        // Live worker: switch now (a turn in flight finishes under the old
        // mode); the config keeps it for the next worker.
        if let Some(port) = self.live_port() {
            let _ = port.set_mode(mode.to_string()).await;
        }
        *self.sh.cfg.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(new_cfg);
        self.audit(audit_actions::MODE_SET, mode, Some(format!("by {source}")))?;
        Ok(mode.to_string())
    }
}
