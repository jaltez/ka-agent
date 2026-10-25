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
pub fn memory_preamble(cfg: &DaemonConfig) -> String {
    let tz = cfg.time_zone();
    let offset = jiff::Timestamp::now().to_zoned(tz.clone()).offset();
    format!(
        "Operator timezone: {} (UTC{} now). Schedule clock times in this zone.",
        tz.iana_name().unwrap_or("UTC"),
        offset
    )
}

impl Runtime {
    /// Swap in a new configuration. Settings the runtime reads per use apply
    /// immediately; channel, workspace and web settings need a restart. A
    /// changed model/mode/engine layer (or `restart_worker`, after a key
    /// change) stops an idle worker so the next message starts a fresh one
    /// with the new settings; a running turn finishes on the old ones.
    pub async fn reload_config(&self, new: DaemonConfig, restart_worker: bool) -> ReloadReport {
        let old = self.cfg();
        let live_fields = [
            ("model", differs(&old.model, &new.model)),
            ("mode", differs(&old.mode, &new.mode)),
            (
                "ka.engine_layer",
                differs(&old.ka.engine_layer, &new.ka.engine_layer),
            ),
            ("search", differs(&old.search, &new.search)),
            ("fallback", differs(&old.fallback, &new.fallback)),
            ("mcp", differs(&old.mcp, &new.mcp)),
            ("approvals", differs(&old.approvals, &new.approvals)),
            (
                "approval_ttl_secs",
                differs(&old.approval_ttl_secs, &new.approval_ttl_secs),
            ),
            (
                "turn_grace_secs",
                differs(&old.turn_grace_secs, &new.turn_grace_secs),
            ),
            (
                "worker_idle_timeout_secs",
                differs(&old.worker_idle_timeout_secs, &new.worker_idle_timeout_secs),
            ),
            (
                "worker_restart",
                differs(&old.worker_restart, &new.worker_restart),
            ),
            ("budgets", differs(&old.budgets, &new.budgets)),
            ("timezone", differs(&old.timezone, &new.timezone)),
            ("calendar", differs(&old.calendar, &new.calendar)),
            ("notify", differs(&old.notify, &new.notify)),
        ];
        let restart_fields = [
            ("workspace", differs(&old.workspace, &new.workspace)),
            ("telegram", differs(&old.telegram, &new.telegram)),
            ("matrix", differs(&old.matrix, &new.matrix)),
            ("web", differs(&old.web, &new.web)),
            // The phone channels build their transcriber at start.
            ("voice", differs(&old.voice, &new.voice)),
        ];
        let names = |fields: &[(&str, bool)]| -> Vec<String> {
            fields
                .iter()
                .filter(|(_, c)| *c)
                .map(|(n, _)| n.to_string())
                .collect()
        };
        let report = ReloadReport {
            applied: names(&live_fields),
            restart_required: names(&restart_fields),
        };
        // model, mode, engine layer, search, fallback, mcp, approvals: all shape the
        // engine the worker was spawned with.
        let engine_changed = live_fields[..7].iter().any(|(_, c)| *c);
        {
            let mut g = self.lock();
            g.budget.daily_usd_cap = new.budgets.daily_usd;
            g.budget.daily_token_cap = new.budgets.daily_tokens;
        }
        publish_timezone(&self.sh.store, &new);
        *self.sh.cfg.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(new);
        if restart_worker || engine_changed {
            self.stop_idle_worker().await;
        }
        report
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
