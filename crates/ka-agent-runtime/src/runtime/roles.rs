//! Users: source resolution, per-user state, and the role policy every
//! worker composes from (ADR-0003 + ADR-0004).
//!
//! One daemon, one store; a channel identity resolves to a **user**, and the
//! user's role partitions the session, memory, workspace, grants, toolset,
//! budget and commands. The terminal, web panel and CLI act as the operator
//! (`operator`, role `admin`). Users run in parallel: each has its own
//! `UserState`, its own worker when it is mid-turn, and its own queued
//! messages.

use super::intake::drain_queue;
use super::*;
use ka_agent_core::Principal;
use ka_agent_core::config::RoleSpec;

/// The channels whose identities resolve to a user. `web:<username>` is the
/// account login's own identity row.
pub const PRINCIPAL_CHANNELS: &[&str] = &["telegram", "matrix", "web", "slack", "discord"];

/// The caps that bind `role` under `cfg`: the role's USD cap when set,
/// else the profile's; tokens always follow the profile.
pub fn budget_caps_of(cfg: &DaemonConfig, role: &str) -> (Option<f64>, Option<u64>) {
    let usd = if role == ka_agent_core::ADMIN_ROLE {
        cfg.budgets.daily_usd
    } else {
        cfg.roles
            .iter()
            .find(|r| r.name == role)
            .and_then(|r| r.budget_daily_usd)
            .or(cfg.budgets.daily_usd)
    };
    (usd, cfg.budgets.daily_tokens)
}

/// A role an identity names but the config no longer defines: fail closed
/// with a read-only, engine-muted spec rather than escalating to admin.
fn unknown_role_spec(name: &str) -> RoleSpec {
    RoleSpec {
        name: name.to_string(),
        mode: "readonly".to_string(),
        allow: Vec::new(),
        disable: [
            "agents",
            "skills",
            "mcp",
            "hooks",
            "web",
            "lsp",
            "debug",
            "tool:bash",
            "tool:write",
            "tool:edit",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        workspace: None,
        budget_daily_usd: None,
        commands: Vec::new(),
        escalate: Vec::new(),
        grants: Default::default(),
    }
}

impl Runtime {
    // ---- resolution ----------------------------------------------------

    /// The principal a source acts as. `None` = refuse: an unlinked identity,
    /// an identity with no user, or a disabled account. Phone and workplace
    /// channels resolve through `identities`; local surfaces, the CLI, cron
    /// fires without a creator and retries are the operator.
    pub fn principal_for_source(&self, source: &str) -> Option<Principal> {
        // `cli-bg`/`api-bg` background asks speak as their channel.
        let base = source.strip_suffix("-bg").unwrap_or(source);
        if let Some((channel, address)) = base.split_once(':')
            && !address.is_empty()
            && PRINCIPAL_CHANNELS.contains(&channel)
        {
            return self
                .sh
                .store
                .identity_principal(channel, address)
                .ok()
                .flatten()
                .map(Principal::from_pair);
        }
        Some(Principal::operator())
    }

    /// The role a source acts as, when it has one (a resolved principal).
    pub fn role_for_source(&self, source: &str) -> Option<String> {
        self.principal_for_source(source).map(|p| p.role)
    }

    /// The configured spec for a role (`None` for admin and for a role the
    /// config no longer defines).
    pub fn role_spec(&self, role: &str) -> Option<RoleSpec> {
        if role == ka_agent_core::ADMIN_ROLE {
            return None;
        }
        self.cfg().roles.iter().find(|r| r.name == role).cloned()
    }

    /// The spec a role's worker composes from; unknown roles fail closed.
    pub fn effective_role(&self, role: &str) -> RoleSpec {
        self.role_spec(role)
            .unwrap_or_else(|| unknown_role_spec(role))
    }

    /// The role's permission-mode ceiling.
    pub fn role_mode_ceiling(&self, role: &str) -> Option<String> {
        if role == ka_agent_core::ADMIN_ROLE {
            return None;
        }
        self.effective_role(role).mode_canonical()
    }

    /// One user's effective permission mode: a `/mode` choice inside the
    /// role's ceiling for their session, else the ceiling (the profile mode
    /// for admin).
    pub fn effective_mode_for(&self, username: &str) -> String {
        let (role, session_mode) = match self.user_handle(username) {
            Some(st) => {
                let g = st.lock();
                (g.role.clone(), g.session_mode.clone())
            }
            None => (
                self.sh
                    .store
                    .get_user(username)
                    .ok()
                    .flatten()
                    .map(|u| u.role)
                    .unwrap_or_else(|| ka_agent_core::ADMIN_ROLE.to_string()),
                None,
            ),
        };
        if role == ka_agent_core::ADMIN_ROLE {
            return self.cfg().mode.clone();
        }
        session_mode.unwrap_or_else(|| {
            self.effective_role(&role)
                .mode_canonical()
                .unwrap_or_else(|| "readonly".to_string())
        })
    }

    /// The per-turn toolset allowlist for a role: `Some(allow)` for an
    /// allow-style role, `None` for admin and disable-style roles (their
    /// worker already carries the subtractive specs).
    pub fn role_toolset(&self, role: &str) -> Option<Vec<String>> {
        if role == ka_agent_core::ADMIN_ROLE {
            return None;
        }
        let spec = self.effective_role(role);
        (!spec.allow.is_empty()).then_some(spec.allow)
    }

    /// Whether `tool` survives a role's subtractive specs (the coarse
    /// groups an operator can disable wholesale).
    fn tool_survives_disable(specs: &[String], tool: &str) -> bool {
        let broad = |group: &str| specs.iter().any(|s| s == group);
        if specs.iter().any(|s| s == &format!("tool:{tool}")) {
            return false;
        }
        if tool.contains("__") && broad("mcp") {
            return false;
        }
        if matches!(tool, "web_search" | "web_fetch") && broad("web") {
            return false;
        }
        if matches!(tool, "delegate" | "tasks") && broad("agents") {
            return false;
        }
        if matches!(tool, "lsp" | "lsp_hover" | "lsp_definition") && broad("lsp") {
            return false;
        }
        if tool == "debug" && broad("debug") {
            return false;
        }
        true
    }

    /// Whether a cron may narrow its creator role's toolset to `tools`:
    /// admin accepts any exact names; a role accepts only what its own
    /// toolset exposes.
    pub fn validate_cron_tools(&self, role: &str, tools: &[String]) -> Result<(), String> {
        if role == ka_agent_core::ADMIN_ROLE || tools.is_empty() {
            return Ok(());
        }
        let spec = self.effective_role(role);
        if !spec.allow.is_empty() {
            if let Some(bad) = tools.iter().find(|t| !spec.allow.contains(t)) {
                return Err(format!(
                    "tool {bad:?} is not in role {role}'s toolset ({})",
                    spec.allow.join(", ")
                ));
            }
            return Ok(());
        }
        if let Some(bad) = tools
            .iter()
            .find(|t| !Self::tool_survives_disable(&spec.disable, t))
        {
            return Err(format!("tool {bad:?} is disabled for role {role}"));
        }
        Ok(())
    }

    /// The tools a fire may actually use: the job's list intersected with
    /// the role's current toolset (a later config change only narrows).
    pub fn clamp_cron_tools(&self, role: &str, tools: &[String]) -> Vec<String> {
        if tools.is_empty() {
            return Vec::new();
        }
        let spec = self.effective_role(role);
        tools
            .iter()
            .filter(|t| {
                if !spec.allow.is_empty() {
                    spec.allow.contains(t)
                } else {
                    Self::tool_survives_disable(&spec.disable, t)
                }
            })
            .cloned()
            .collect()
    }

    /// A user's workspace: the operator keeps the profile workspace, a role
    /// with an explicit `workspace` keeps that directory, everyone else gets
    /// `<data>/users/<name>/workspace`.
    pub fn workspace_for_user(&self, username: &str, role: &str) -> std::path::PathBuf {
        if role == ka_agent_core::ADMIN_ROLE {
            return self.cfg().workspace_path();
        }
        if let Some(spec) = self.role_spec(role)
            && spec.workspace.is_some()
        {
            return spec.workspace_path(self.sh.paths.root());
        }
        self.sh.paths.user_workspace(username)
    }

    // ---- per-user state ------------------------------------------------

    /// One user's state handle, when they have one (they do after their first
    /// message).
    pub fn user_handle(&self, username: &str) -> Option<UserHandle> {
        self.sh
            .users
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(username)
            .cloned()
    }

    /// Every user with state, ordered by last activity (least recent first):
    /// the worker cap's eviction order and the fairness drain's rotation.
    pub(super) fn users_by_activity(&self) -> Vec<(String, UserHandle)> {
        let mut out: Vec<(String, UserHandle)> = self
            .sh
            .users
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by_key(|(_, st)| st.lock().last_active_ms);
        out
    }

    /// The user's state, created on first use: their latest session (or a
    /// fresh one), their own budget window and their partition directories.
    pub fn ensure_user(&self, who: &Principal) -> Result<UserHandle, Error> {
        if let Some(st) = self.user_handle(&who.username) {
            let mut g = st.lock();
            if g.role != who.role {
                // The role changed (config reload, admin edit): the state
                // follows, and the next worker composes the new policy.
                g.role = who.role.clone();
                g.session.role = who.role.clone();
            }
            drop(g);
            return Ok(st);
        }
        // A user named by a source must exist as a row; an identity-less
        // principal would be a config path we never take.
        if self.sh.store.get_user(&who.username)?.is_none() {
            return Err(Error::Refused(format!(
                "unknown user {:?} — `ka-agent user invite` first",
                who.username
            )));
        }
        self.sh.paths.ensure_user_dirs(&who.username)?;
        let mut session = match self.sh.store.latest_session_for_user(&who.username)? {
            Some(mut session) => {
                // A turn that died with a previous process must not leave
                // the user looking busy (same policy as boot adoption).
                if matches!(
                    session.status,
                    SessionStatus::Running
                        | SessionStatus::AwaitingApproval
                        | SessionStatus::Stopping
                ) && session.transition(SessionStatus::Idle).is_ok()
                {
                    self.sh
                        .store
                        .update_session_status(&session.id, session.status)?;
                }
                // Pre-users sessions recorded the legacy `roles/<r>/workspace`
                // path the v12 bootstrap moved to `users/<r>`: repoint the row,
                // or the new guard lists would refuse every file tool.
                let legacy = self.sh.paths.root().join("roles");
                if session.workspace.starts_with(&legacy)
                    && let Ok(rest) = session.workspace.strip_prefix(&legacy)
                {
                    let moved = self.sh.paths.users_root().join(rest);
                    session.workspace = moved.clone();
                    let _ = self
                        .sh
                        .store
                        .set_session_workspace(&session.id, &moved);
                }
                session
            }
            None => {
                let session = Session::new_for_user(
                    self.workspace_for_user(&who.username, &who.role),
                    &who.username,
                    &who.role,
                );
                self.sh.store.insert_session(&session)?;
                let mut rec = AuditRecord::new(
                    "system",
                    audit_actions::SESSION_NEW,
                    session.id.as_str(),
                    Some(format!("user {} bootstrap", who.username)),
                )
                .with_role(&who.role)
                .with_user(&who.username);
                self.sh.store.append_audit(&mut rec)?;
                session
            }
        };
        session.role = who.role.clone();
        // First state: compile persona/house rules + memory into the user's
        // engine home, so their first worker already reads them.
        if let Err(e) = self.refresh_compiled_memory_for(&who.username) {
            eprintln!("runtime: MEMORY.md not written for {}: {e}", who.username);
        }
        let identity = crate::runtime::compile_identity(&self.persona_text());
        let dir = self
            .sh
            .paths
            .user_engine_home(&who.username)
            .join(".config/ka");
        if let Err(e) = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join("IDENTITY.md"), &identity))
        {
            eprintln!("runtime: IDENTITY.md not written for {}: {e}", who.username);
        }
        let budget = self.load_budget_for(&who.username, &who.role);
        let state = UserState {
            session,
            role: who.role.clone(),
            session_mode: None,
            session_model: None,
            agent: None,
            session_allow: std::collections::HashSet::new(),
            live: None,
            budget,
            crashes: Vec::new(),
            restart_attempt: 0,
            effort: None,
            last_active_ms: ka_agent_core::now_ms(),
        };
        let handle = UserHandle::new(state);
        // First-touch serialization: two simultaneous first messages must
        // not both insert a session row. The map lock decides the winner;
        // the loser adopts the winner's handle (its own row, if any slipped
        // in, becomes the orphan `latest_session_for_user` already covers).
        let mut map = self.sh.users.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get(&who.username) {
            return Ok(existing.clone());
        }
        map.insert(who.username.clone(), handle);
        Ok(map[&who.username].clone())
    }

    /// The acting user's live `/model` pick for this session, if any
    /// (policy was already enforced when it was set).
    pub fn session_model_for(&self, username: &str) -> Option<String> {
        self.user_handle(username).and_then(|st| st.lock().session_model.clone())
    }

    /// The model a turn of `who` runs on: their session choice, else the
    /// policy default, else the configured model — always inside the user's
    /// allowlist (an empty allowlist allows the whole catalog).
    pub fn model_for(&self, who: &Principal, background: bool) -> Option<String> {
        let cfg = self.cfg();
        let (allow, default) = self
            .sh
            .store
            .model_policy_for(&who.username)
            .unwrap_or_default();
        let ok = |m: &str| ka_agent_store::model_allowed(&allow, m);
        if background && let Some(bg) = cfg.background_model.clone().filter(|m| ok(m)) {
            return Some(bg);
        }
        if let Some(session) = self.user_handle(&who.username).and_then(|st| st.lock().session_model.clone())
            && ok(&session)
        {
            return Some(session);
        }
        if let Some(default) = default.clone().filter(|d| ok(d)) {
            return Some(default);
        }
        cfg.model.clone().filter(|m| ok(m))
    }

    /// Make sure the user's state exists (their partition directories, their
    /// session row, their budget window), without exposing the handle.
    pub fn ensure_user_state(&self, who: &Principal) -> Result<(), Error> {
        self.ensure_user(who).map(|_| ())
    }

    /// `/agent <name>`: run this conversation as a shared agent the user may
    /// use. `None` clears it. Returns the confirmation line.
    pub fn set_agent(&self, who: &Principal, name: Option<&str>) -> Result<String, Error> {
        let st = self.ensure_user(who)?;
        let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
            st.lock().agent = None;
            return Ok("agent: off (back to your own assistant)".into());
        };
        let level = self
            .sh
            .store
            .agent_level(name, &who.username, &who.role)?
            .ok_or_else(|| Error::Refused(format!("no shared agent {name:?} you can use")))?;
        st.lock().agent = Some(name.to_string());
        Ok(format!(
            "agent {name} on ({level}) — persona, skills and knowledge come from it"
        ))
    }

    /// The agent this user's conversation runs as, when it still exists and
    /// the user may use it.
    pub fn active_agent(&self, who: &Principal) -> Option<ka_agent_store::AgentRow> {
        let name = self.user_handle(&who.username)?.lock().agent.clone()?;
        let row = self.sh.store.get_agent(&name).ok()??;
        self.sh
            .store
            .agent_level(&name, &who.username, &who.role)
            .ok()
            .flatten()
            .map(|_| row)
    }

    /// Provenance (M3): the first source of `turn` whose access groups the
    /// recipient's groups do not cover. `None` = the delivery is allowed.
    /// A source with an empty `group_names` row is unrestricted.
    pub fn provenance_conflict(
        &self,
        turn: &str,
        recipient_groups: &[String],
    ) -> Option<ka_agent_store::TurnSourceRow> {
        let rows = self.sh.store.turn_sources(turn).ok()?;
        rows.into_iter().find(|row| {
            !row.group_names.is_empty()
                && !row
                    .group_names
                    .iter()
                    .any(|g| recipient_groups.iter().any(|r| r == g))
        })
    }

    /// The groups a delivery audience belongs to (a user's groups, or the
    /// shared channel account's own).
    pub fn groups_of_user(&self, username: &str) -> Vec<String> {
        self.sh.store.groups_of(username).unwrap_or_default()
    }

    /// The user's current session, creating their state when needed.
    pub fn session_for_user(&self, who: &Principal) -> Result<Session, Error> {
        Ok(self.ensure_user(who)?.lock().session.clone())
    }

    /// The store meta key holding a user's budget window (the operator keeps
    /// the pre-users `budget` key, so an existing install continues its day).
    fn budget_meta_key(username: &str) -> String {
        if username == ka_agent_core::OPERATOR_USER {
            "budget".to_string()
        } else {
            format!("budget:user:{username}")
        }
    }

    /// The caps that bind a user: their own daily quota when set, else the
    /// role's cap, else the profile's; tokens always follow the profile.
    pub fn budget_caps_for(&self, username: &str, role: &str) -> (Option<f64>, Option<u64>) {
        let cfg = self.cfg();
        let quota = self
            .sh
            .store
            .get_user(username)
            .ok()
            .flatten()
            .and_then(|u| u.quota_daily_usd);
        let (usd, tokens) = budget_caps_of(&cfg, role);
        (quota.or(usd), tokens)
    }

    /// A user's budget state: today's saved window when it matches, with the
    /// current caps (a reload or quotal change always wins).
    fn load_budget_for(&self, username: &str, role: &str) -> BudgetState {
        let (usd, tokens) = self.budget_caps_for(username, role);
        let mut budget = BudgetState::new(usd, tokens);
        if let Ok(Some(saved)) = self.sh.store.meta_get(&Self::budget_meta_key(username))
            && let Ok(saved) = serde_json::from_str::<BudgetState>(&saved)
            && saved.day == ka_agent_core::day_key(ka_agent_core::now_ms())
        {
            budget = saved;
            budget.daily_usd_cap = usd;
            budget.daily_token_cap = tokens;
        }
        budget
    }

    /// A user's current budget verdict (their live state when they have one,
    /// else today's saved window).
    pub fn budget_verdict_for(&self, username: &str) -> BudgetVerdict {
        if let Some(st) = self.user_handle(username) {
            return st.lock().budget.verdict();
        }
        let role = self
            .sh
            .store
            .get_user(username)
            .ok()
            .flatten()
            .map(|u| u.role)
            .unwrap_or_else(|| ka_agent_core::ADMIN_ROLE.to_string());
        self.load_budget_for(username, &role).verdict()
    }

    /// Persist a user's budget window under their key.
    pub(super) fn persist_budget(&self, username: &str, budget: &BudgetState) {
        if let Ok(json) = serde_json::to_string(budget) {
            let _ = self.sh.store.meta_set(&Self::budget_meta_key(username), &json);
        }
    }

    /// One user's reasoning effort (`/effort`; cleared by `/new`).
    pub fn effort_for(&self, username: &str) -> Option<String> {
        self.user_handle(username).and_then(|st| st.lock().effort.clone())
    }

    /// Make `who` ready to receive a turn. `false` while *that user's* own
    /// session is mid-turn (the caller queues instead of disturbing it).
    pub fn activate_user(&self, who: &Principal) -> bool {
        let Ok(st) = self.ensure_user(who) else {
            return false;
        };
        if matches!(
            st.lock().session.status,
            SessionStatus::Running | SessionStatus::AwaitingApproval | SessionStatus::Stopping
        ) {
            return false;
        }
        st.lock().last_active_ms = ka_agent_core::now_ms();
        true
    }

    /// Run the fairness drain on its own task. The spawn is what keeps the
    /// supervisor acyclic: `drive` and the crash path await this, and the
    /// drain awaits `ensure_worker`, which spawns `drive` — awaiting it
    /// inline would make the futures recursively non-`Send`.
    pub(super) fn spawn_drain(&self) {
        let rt = Runtime {
            sh: Arc::clone(&self.sh),
        };
        tokio::spawn(async move { rt.drain_waiting_users().await });
    }

    /// After a turn settles: hand a free worker slot to a user with queued
    /// messages, least recently active first (one user's long turn never
    /// head-of-line blocks another).
    pub(super) async fn drain_waiting_users(&self) {
        for (username, st) in self.users_by_activity() {
            let Some(user) = self
                .sh
                .store
                .get_user(&username)
                .ok()
                .flatten()
                .filter(|u| !u.disabled)
            else {
                continue;
            };
            let who = Principal::new(&username, &user.role);
            let (sid, status, has_live) = {
                let g = st.lock();
                (g.session.id.clone(), g.session.status, g.live.is_some())
            };
            if status != SessionStatus::Idle {
                continue;
            }
            if self.sh.store.queue_len(&sid).unwrap_or(0) == 0 {
                continue;
            }
            // Over the worker cap the message waits for a slot; an idle
            // worker is evicted inside `ensure_worker`.
            if !has_live && self.live_worker_count() >= self.max_workers() {
                self.evict_lru_idle_worker(&username);
            }
            {
                let mut g = st.lock();
                if g.session.transition(SessionStatus::Running).is_err() {
                    continue;
                }
                g.last_active_ms = ka_agent_core::now_ms();
            }
            self.persist_session_status(&username, &sid, SessionStatus::Running);
            if self.ensure_worker(&who, "console").await.is_err() {
                let mut g = st.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                drop(g);
                self.persist_session_status(&username, &sid, SessionStatus::Idle);
                continue;
            }
            if !drain_queue(&self.sh, &who, &st).await {
                let mut g = st.lock();
                if g.session.status == SessionStatus::Running {
                    g.session.transition(SessionStatus::Idle).ok();
                }
                drop(g);
                self.persist_session_status(&username, &sid, SessionStatus::Idle);
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::paths::Paths;
    use crate::surface::Surface;
    use ka_agent_port::WorkerPort;
    use std::sync::Arc;

    struct NullSurface;
    impl Surface for NullSurface {
        fn emit(&self, _env: &crate::event::Envelope) {}
    }

    fn runtime(cfg: DaemonConfig) -> Runtime {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ka-roles-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(ka_agent_store::Store::open(&dir.join("state.db")).unwrap());
        let mut cfg = cfg;
        cfg.workspace = dir.join("workspace");
        Runtime::new(
            cfg,
            Paths::new(dir),
            store,
            WorkerPort::default(),
            Arc::new(NullSurface),
        )
        .unwrap()
    }

    fn cfg_with_guest() -> DaemonConfig {
        DaemonConfig {
            sandbox: "off".into(),
            roles: vec![RoleSpec {
                name: "guest".into(),
                mode: "readonly".into(),
                allow: vec!["read".into()],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn sources_resolve_to_their_linked_user() {
        let rt = runtime(cfg_with_guest());
        rt.store()
            .link_identity_as("telegram", "42", "cli", "guest")
            .unwrap();
        // Linking a guest identity created the role-named user.
        assert_eq!(
            rt.principal_for_source("telegram:42"),
            Some(Principal::new("guest", "guest"))
        );
        // An unlinked sender has no principal: the runtime refuses it.
        assert_eq!(rt.principal_for_source("telegram:99"), None);
        // The web relay's logins are identities too; bare `web` (loopback)
        // stays the operator.
        rt.store()
            .link_identity_to_user("web", "ana", "invite", "guest", "ana")
            .unwrap();
        assert_eq!(
            rt.principal_for_source("web:ana"),
            Some(Principal::new("ana", "guest"))
        );
        assert_eq!(rt.principal_for_source("web"), Some(Principal::operator()));
        for local in ["console", "cli", "api", "cli-bg", "api-bg", "cron", "briefing", "retry"] {
            assert_eq!(
                rt.principal_for_source(local),
                Some(Principal::operator()),
                "{local}"
            );
        }
        // A disabled account stops resolving.
        rt.store().set_user_disabled("ana", true).unwrap();
        assert_eq!(rt.principal_for_source("web:ana"), None);
    }

    #[test]
    fn unknown_roles_fail_closed() {
        let rt = runtime(cfg_with_guest());
        let spec = rt.effective_role("ghost");
        assert_eq!(spec.mode_canonical().as_deref(), Some("readonly"));
        assert!(spec.disable.iter().any(|s| s == "tool:bash"));
        assert!(rt.role_spec("admin").is_none(), "admin is implicit");
        assert_eq!(rt.effective_mode_for(ka_agent_core::OPERATOR_USER), "ask");
    }

    #[test]
    fn guard_lists_separate_user_trees() {
        let rt = runtime(cfg_with_guest());
        let root = rt.sh.paths.root().to_path_buf();
        let admin_write = rt.guard_write_paths(ka_agent_core::OPERATOR_USER, "admin");
        assert!(admin_write.contains(&root.join("users")));
        assert!(admin_write.contains(&root.join("roles")));
        // The pre-users role trees are not a subject's either.
        let guest_write = rt.guard_write_paths("guest", "guest");
        assert!(guest_write.contains(&root.join("roles")));
        let own_ws = rt.workspace_for_user("guest", "guest");
        assert!(
            !guest_write.iter().any(|p| own_ws.starts_with(p)),
            "a user may write their own workspace: {guest_write:?}"
        );
        let admin_ws = rt.workspace_for_user(ka_agent_core::OPERATOR_USER, "admin");
        assert!(
            guest_write.iter().any(|p| admin_ws.starts_with(p)),
            "a user may not write the operator workspace"
        );
        let guest_read = rt.guard_read_paths("guest", "guest");
        for blocked in ["memory", "notes", "ka", "engine-home", "state.db"] {
            assert!(
                guest_read.contains(&root.join(blocked)),
                "guest reads must block {blocked}: {guest_read:?}"
            );
        }
        // The operator keeps the profile root itself.
        let op_ws = rt.workspace_for_user(ka_agent_core::OPERATOR_USER, "admin");
        assert_eq!(op_ws, rt.config().workspace_path());
    }

    #[test]
    fn role_budget_caps_follow_the_config() {
        let mut cfg = cfg_with_guest();
        cfg.budgets.daily_usd = Some(10.0);
        cfg.budgets.daily_tokens = Some(1000);
        cfg.roles[0].budget_daily_usd = Some(2.0);
        let rt = runtime(cfg);
        rt.store()
            .create_user(&ka_agent_store::UserRow {
                username: "ana".into(),
                display_name: "Ana".into(),
                pw_hash: None,
                role: "guest".into(),
                quota_daily_usd: None,
                disabled: false,
                created_ms: 1,
                created_by: Some("test".into()),
            })
            .unwrap();
        assert_eq!(
            rt.budget_caps_for(ka_agent_core::OPERATOR_USER, "admin"),
            (Some(10.0), Some(1000))
        );
        assert_eq!(rt.budget_caps_for("ana", "guest"), (Some(2.0), Some(1000)));
        // A per-user quota overrides the role cap.
        rt.store().set_user_quota("ana", Some(0.5)).unwrap();
        assert_eq!(rt.budget_caps_for("ana", "guest").0, Some(0.5));
    }

    #[test]
    fn cron_tools_only_narrow_the_role_toolset() {
        let rt = runtime(cfg_with_guest());
        assert!(
            rt.validate_cron_tools("guest", &["read".into()]).is_ok(),
            "an allowed tool passes"
        );
        assert!(
            rt.validate_cron_tools("guest", &["bash".into()]).is_err(),
            "a tool outside the role's allowlist is refused"
        );
        // Clamping never widens.
        assert_eq!(
            rt.clamp_cron_tools("guest", &["read".into(), "bash".into()]),
            vec!["read".to_string()]
        );
        // Admin accepts any name.
        assert!(rt.validate_cron_tools("admin", &["bash".into()]).is_ok());
    }

    #[test]
    fn provenance_checks_the_audience() {
        let rt = runtime(cfg_with_guest());
        rt.store()
            .record_turn_source("turn_1", "kb", "kb:secret/a.md", &["ops".into()])
            .unwrap();
        rt.store()
            .record_turn_source("turn_1", "file", "notes/plan.md", &[])
            .unwrap();
        // The ops group keeps it; anyone else is a conflict naming the
        // restricted source.
        assert!(rt.provenance_conflict("turn_1", &["ops".into()]).is_none());
        let conflict = rt
            .provenance_conflict("turn_1", &["dev".into()])
            .expect("conflict");
        assert_eq!(conflict.reference, "kb:secret/a.md");
        assert_eq!(conflict.group_names, vec!["ops".to_string()]);
        assert!(rt.provenance_conflict("turn_2", &[]).is_none());
    }

    #[test]
    fn mode_is_clamped_to_the_role_ceiling() {
        let rt = runtime(cfg_with_guest());
        rt.store()
            .link_identity_as("telegram", "42", "test", "guest")
            .unwrap();
        assert_eq!(rt.effective_mode_for("guest"), "readonly");
        assert_eq!(rt.effective_mode_for(ka_agent_core::OPERATOR_USER), "ask");
    }
}
