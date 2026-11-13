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
    // The operator's clock context rides the per-turn `<ctx .../>` line
    // (step 2.2); the compiled MEMORY.md carries facts only. The operator
    // language (D8) is the one instruction that belongs here: it holds
    // for every turn regardless of the memory contents.
    cfg.language_preamble()
}

impl Runtime {
    /// Swap in a new configuration. Settings the runtime reads per use apply
    /// immediately; channel, workspace and web settings need a restart. A
    /// changed model/mode/engine layer (or `restart_worker`, after a key
    /// change) stops an idle worker so the next message starts a fresh one
    /// with the new settings; a running turn finishes on the old ones.
    pub async fn reload_config(&self, new: DaemonConfig, restart_worker: bool) -> ReloadReport {
        // One writer at a time: a hot-reload that loaded the file before a
        // live setter could otherwise apply its stale snapshot last and
        // silently undo the setter (a `/mode readonly` reverting to ask).
        let _guard = self.sh.config_lock.lock().await;
        self.reload_config_locked(new, restart_worker).await
    }

    /// The swap without the config lock: callers that already hold it
    /// (the file watcher, the live setters) use this so the load and the
    /// apply cannot interleave with another writer.
    pub(crate) async fn reload_config_locked(
        &self,
        new: DaemonConfig,
        restart_worker: bool,
    ) -> ReloadReport {
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
        // Every user's caps follow the reload (their own quota wins, else
        // their role's USD cap, else the profile's).
        for (username, st) in self.users_by_activity() {
            let role = st.lock().role.clone();
            let cfg_new = &new;
            let (usd, tokens) = {
                let quota = self
                    .sh
                    .store
                    .get_user(&username)
                    .ok()
                    .flatten()
                    .and_then(|u| u.quota_daily_usd);
                let (usd, tokens) = super::roles::budget_caps_of(cfg_new, &role);
                (quota.or(usd), tokens)
            };
            let mut g = st.lock();
            g.budget.daily_usd_cap = usd;
            g.budget.daily_token_cap = tokens;
        }
        // New roles get their tree now; a removed role's files stay (the
        // operator may want them back).
        if let Err(e) = self.sh.paths.ensure_role_dirs_for(&new.roles) {
            self.sh.emit(AgentEvent::Notice {
                text: format!("role directories not created: {e}"),
            });
        }
        publish_timezone(&self.sh.store, &new);
        // `calendar.digest_time` is Apply::Live: an existing digest cron
        // bakes its wall-clock time into the stored `at_local`, so retime
        // it here or the digest keeps firing at the old hour. (Before the
        // swap: `new` is moved into the shared config below.)
        if changed.iter().any(|k| k.path == "calendar.digest_time") {
            self.retime_calendar_crons(&new);
        }
        *self.sh.cfg.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(new);
        // remember the file's current mtime: the hot-reload watcher must
        // not re-announce this swap (config set / IPC reload land here)
        self.remember_config_mtime();
        if restart_worker || engine_changed {
            self.stop_idle_worker().await;
        }
        report
    }

    /// Rewrite every existing calendar-digest cron's `at_local` (and its
    /// next fire, when it is live) to the configured `calendar.digest_time`.
    fn retime_calendar_crons(&self, new: &DaemonConfig) {
        let Some((hh, mm)) = ka_agent_core::when::parse_hhmm(&new.calendar.digest_time) else {
            return;
        };
        let Ok(tasks) = self.sh.store.cron_tasks() else {
            return;
        };
        let now_zoned = jiff::Timestamp::now().to_zoned(new.time_zone());
        let mut retimed = false;
        for task in tasks {
            let Some(mut spec) = task
                .payload
                .as_deref()
                .and_then(ka_agent_core::CronSpec::from_payload)
            else {
                continue;
            };
            if spec.mode != ka_agent_core::CronMode::NoAgent("calendar".into()) {
                continue;
            }
            if spec.at_local.as_deref() == Some(new.calendar.digest_time.as_str()) {
                continue;
            }
            spec.at_local = Some(new.calendar.digest_time.clone());
            let _ = self
                .sh
                .store
                .update_task_payload(&task.id, &spec.to_payload());
            if task.status == ka_agent_core::TaskStatus::Queued {
                let next = ka_agent_core::when::next_local(hh, mm, &spec.day_rule(), &now_zoned);
                let _ = self
                    .sh
                    .store
                    .update_task_schedule(&task.id, Some(next), "queued");
                retimed = true;
            }
        }
        if retimed {
            self.sh.scheduler_tick.notify_one();
        }
    }

    /// Record the config file's current mtime as "seen" (called by every
    /// reload path), and return the previous value for the watcher.
    pub(super) fn remember_config_mtime(&self) {
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
        // Hold the lock from the mtime check through the apply: a live
        // setter that writes the file meanwhile must not be undone by the
        // snapshot taken here.
        let _guard = self.sh.config_lock.lock().await;
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
                let report = self.reload_config_locked(new, false).await;
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

    /// `/effort [level]`: session-scoped reasoning effort. "" clears it
    /// (the model's own default); it resets with every `/new`.
    pub async fn set_effort(
        &self,
        level: &str,
        who: &ka_agent_core::Principal,
    ) -> Result<String, Error> {
        let level = level.trim().to_ascii_lowercase();
        if !level.is_empty() && !matches!(level.as_str(), "off" | "low" | "medium" | "high" | "max")
        {
            return Err(Error::Refused(format!(
                "unknown effort {level:?} (off|low|medium|high|max)"
            )));
        }
        if let Some(port) = self.live_port_for(who) {
            port.set_effort(level.clone()).await.map_err(Error::Port)?;
        }
        if let Some(st) = self.user_handle(&who.username) {
            st.lock().effort = (!level.is_empty()).then(|| level.clone());
        }
        self.settings_event_for(who);
        Ok(if level.is_empty() {
            "effort: model default".to_string()
        } else {
            format!("effort → {level} (this session)")
        })
    }

    /// One user's current effort level, when one was set.
    pub fn effort(&self, who: &ka_agent_core::Principal) -> Option<String> {
        self.effort_for(&who.username)
    }

    /// `/undo`: ask the engine to revert its most recent file change; the
    /// engine's answer arrives as a Note.
    pub async fn undo(&self, who: &ka_agent_core::Principal) -> Result<(), Error> {
        let port = self
            .live_port_for(who)
            .ok_or_else(|| Error::Refused("no live session — nothing to undo".into()))?;
        port.undo_file().await.map_err(Error::Port)
    }

    /// `/steer <text>`: force a steer into the running reply; when idle it
    /// is just a message.
    pub async fn steer(
        &self,
        text: &str,
        who: &ka_agent_core::Principal,
    ) -> Result<bool, Error> {
        let Some(port) = self.live_port_for(who) else {
            return Ok(false);
        };
        if self.session_status_for(who) != ka_agent_core::SessionStatus::Running {
            return Ok(false);
        }
        port.steer(text.to_string()).await.map_err(Error::Port)?;
        if let Some(st) = self.user_handle(&who.username)
            && let Some(live) = st.lock().live.as_mut()
        {
            live.live_text
                .push_str(&format!("\n\n[added while working] {text}"));
        }
        self.sh.emit(AgentEvent::Note {
            text: "steered into the running reply".into(),
        });
        Ok(true)
    }

    /// Queue rows not yet turned (for `/queue`).
    pub fn queued(
        &self,
        who: &ka_agent_core::Principal,
    ) -> Result<Vec<(String, String, String)>, Error> {
        let Some(sid) = self.session_id_for(who) else {
            return Ok(Vec::new());
        };
        let items = self.sh.store.queued_items(&sid)?;
        Ok(items
            .into_iter()
            .map(|i| (i.id.to_string(), i.source, i.text))
            .collect())
    }

    /// Clear the durable queue (`/queue clear`).
    pub fn clear_queue(&self, who: &ka_agent_core::Principal) -> Result<usize, Error> {
        let Some(sid) = self.session_id_for(who) else {
            return Ok(0);
        };
        let items = self.sh.store.queued_items(&sid)?;
        for item in &items {
            self.sh
                .store
                .mark_turned(&item.id, &ka_agent_core::TurnId::generate())?;
        }
        Ok(items.len())
    }

    /// Remove one queued message by id prefix (the `/queue` picker's ✕):
    /// `Ok(true)` removed, `Ok(false)` no such item, `Err` ambiguous.
    pub fn remove_queued(
        &self,
        id_prefix: &str,
        who: &ka_agent_core::Principal,
    ) -> Result<bool, Error> {
        let Some(sid) = self.session_id_for(who) else {
            return Ok(false);
        };
        let hits: Vec<_> = self
            .sh
            .store
            .queued_items(&sid)?
            .into_iter()
            .filter(|i| ka_agent_core::ids::id_matches(i.id.as_str(), id_prefix.trim()))
            .collect();
        match hits.as_slice() {
            [one] => self.sh.store.queue_remove(&one.id).map_err(Error::Store),
            [] => Ok(false),
            many => Err(Error::Refused(format!(
                "ambiguous id — {} matches ({} …)",
                many.len(),
                ka_agent_core::ids::short_id(many[0].id.as_str())
            ))),
        }
    }

    /// The user's engine tool inventory, as the engine discovered it.
    pub fn tools(&self, who: &ka_agent_core::Principal) -> Vec<String> {
        self.user_handle(&who.username)
            .and_then(|st| st.lock().live.as_ref().map(|l| l.tools.clone()))
            .unwrap_or_default()
    }

    /// Broadcast the current model/mode/effort (surfaces show a status bar).
    pub(crate) fn settings_event_for(&self, who: &ka_agent_core::Principal) {
        let model = self.model_for(who, false);
        let source = format!("web:{}", who.username);
        self.sh.emit_from(
            AgentEvent::Settings {
                model,
                mode: Some(self.effective_mode_for(&who.username)),
                effort: self.effort(who),
                private: Some(self.session_private_for(who)),
            },
            Some(&source),
        );
    }

    /// Compile the persona into the engine's IDENTITY.md (re-read per
    /// turn by `[prompt] identity_file`). Called at startup, after a
    /// persona edit, and from the maintenance watcher.
    pub fn refresh_identity(&self) -> Result<(), Error> {
        // Persona content is profile-wide policy; only the file location is
        // per role (each engine reads its own HOME).
        let persona = self.memory_store(ka_agent_core::OPERATOR_USER)?.persona_text();
        let text = crate::runtime::compile_identity(&persona);
        let mut homes = vec![self.sh.paths.engine_home()];
        homes.extend(
            self.cfg()
                .roles
                .iter()
                .map(|r| self.sh.paths.role_engine_home(&r.name)),
        );
        // Every existing user's engine home reads its own compiled
        // IDENTITY.md.
        if let Ok(users) = self.sh.store.list_users() {
            for row in users {
                homes.push(self.sh.paths.user_engine_home(&row.username));
            }
        }
        for home in homes {
            let dir = home.join(".config/ka");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("IDENTITY.md"), &text)?;
        }
        self.remember_persona_mtime();
        Ok(())
    }

    /// The persona's display name (`Ka` by default).
    pub fn persona_name(&self) -> String {
        self.memory_store(ka_agent_core::OPERATOR_USER)
            .map(|m| m.persona_name())
            .unwrap_or_else(|_| "Ka".to_string())
    }

    /// The current persona text.
    pub fn persona_text(&self) -> String {
        self.memory_store(ka_agent_core::OPERATOR_USER)
            .map(|m| m.persona_text())
            .unwrap_or_default()
    }

    /// Persist a new persona and recompile IDENTITY.md in-process.
    pub fn set_persona(&self, text: &str) -> Result<(), Error> {
        self.memory_store(ka_agent_core::OPERATOR_USER)?
            .write_persona(text)?;
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
                retryable: retryable_class("internal"),
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

    /// `/model <provider/model>`: validate, then — for the operator — save
    /// to the config file, or — for a subject — switch this session inside
    /// the user's allowlist (never persisted). Returns the normalized
    /// selector for the reply.
    pub async fn set_model(
        &self,
        sel: &str,
        who: &ka_agent_core::Principal,
    ) -> Result<String, Error> {
        let info = ka_agent_port::catalog::validate_selector(sel).map_err(Error::Refused)?;
        let (allow, _) = self
            .sh
            .store
            .model_policy_for(&who.username)
            .unwrap_or_default();
        if !ka_agent_store::model_allowed(&allow, &info.id) {
            return Err(Error::Refused(format!(
                "model {} is not allowed for {} (allowed: {})",
                info.id,
                who.username,
                allow.join(", ")
            )));
        }
        if !who.is_admin() {
            if let Some(st) = self.user_handle(&who.username) {
                st.lock().session_model = Some(info.id.clone());
            }
            if let Some(port) = self.live_port_for(who) {
                let _ = port.set_model(info.id.clone()).await;
            }
            self.audit_for(
                who,
                audit_actions::MODE_SET,
                &info.id,
                Some("model (session)".into()),
            )?;
            self.settings_event_for(who);
            return Ok(info.id);
        }
        let path = self
            .config_path()
            .ok_or_else(|| Error::Refused("no config file — run ka-agent setup".into()))?;
        let _guard = self.sh.config_lock.lock().await;
        ka_agent_core::config_file::set(&path, "model", &info.id).map_err(Error::Config)?;
        let new = DaemonConfig::load(&path)?;
        self.reload_config_locked(new, false).await;
        self.settings_event_for(who);
        Ok(info.id)
    }

    /// `/mode`: switch the permission mode, persisted and applied to a
    /// live worker at once. From a phone channel the mode may only get
    /// stricter (allow < accept_edits < ask < readonly): a lost phone must
    /// not be able to turn approvals off.
    pub async fn set_mode(
        &self,
        mode: &str,
        source: &str,
        who: &ka_agent_core::Principal,
    ) -> Result<String, Error> {
        /// Strictness rank; `None` names no mode. The old spellings stay
        /// accepted here as a defense in depth — the daemon's config is
        /// normalized at load, but callers hand in raw operator input.
        fn strictness(m: &str) -> Option<u8> {
            Some(match m {
                "allow" | "free" => 0,
                "accept_edits" | "accept-edits" => 1,
                "ask" | "guarded" => 2,
                "readonly" | "plan" => 3,
                _ => return None,
            })
        }
        let mode = mode.trim();
        let Some(canonical) = ka_agent_core::config::normalize_mode(mode) else {
            return Err(Error::Refused(format!(
                "unknown mode {mode:?}: ask | accept_edits | allow | readonly \
                 (the old names guarded/accept_edits/free/plan still work)"
            )));
        };
        // Unreachable for a word `normalize_mode` accepted, but the phone
        // check below must never run on an unranked word.
        let new = strictness(&canonical)
            .ok_or_else(|| Error::Refused(format!("unknown mode {mode:?}")))?;
        // A role's mode is its ceiling: a request above it is clamped to
        // the ceiling (never an error), applies to this session only, and
        // is never written to the profile config.
        let role = who.role.clone();
        if role != ka_agent_core::ADMIN_ROLE {
            let ceiling = self
                .effective_role(&role)
                .mode_canonical()
                .unwrap_or_else(|| "readonly".to_string());
            let ceiling_rank = strictness(&ceiling).unwrap_or(3);
            // Rank grows with strictness: a request BELOW the ceiling is
            // looser and gets clamped.
            let applied = if new < ceiling_rank {
                ceiling.clone()
            } else {
                canonical.clone()
            };
            if let Some(port) = self.live_port_for(who) {
                let _ = port.set_mode(applied.clone()).await;
            }
            if let Some(st) = self.user_handle(&who.username) {
                st.lock().session_mode = Some(applied.clone());
            }
            self.audit_for(
                who,
                audit_actions::MODE_SET,
                &applied,
                Some(format!("by {source} (role {role})")),
            )?;
            self.settings_event_for(who);
            return Ok(if applied != canonical {
                format!("{applied} (role {role}'s ceiling is {ceiling})")
            } else {
                applied
            });
        }
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
        // One writer at a time, and mark the file as seen so the watcher
        // does not re-announce (or re-apply a stale snapshot) afterwards.
        // The canonical word is what gets persisted — an alias typed by
        // the operator is never written back.
        let _guard = self.sh.config_lock.lock().await;
        ka_agent_core::config_file::set(&path, "mode", &format!("{canonical:?}"))
            .map_err(Error::Config)?;
        let mut new_cfg = (*self.cfg()).clone();
        new_cfg.mode = canonical.clone();
        // Live worker: switch now (a turn in flight finishes under the old
        // mode); the config keeps it for the next worker.
        if let Some(port) = self.live_port_for(who) {
            let _ = port.set_mode(canonical.clone()).await;
        }
        *self.sh.cfg.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(new_cfg);
        self.remember_config_mtime();
        self.audit_for(
            who,
            audit_actions::MODE_SET,
            &canonical,
            Some(format!("by {source}")),
        )?;
        self.settings_event_for(who);
        Ok(canonical)
    }
}
