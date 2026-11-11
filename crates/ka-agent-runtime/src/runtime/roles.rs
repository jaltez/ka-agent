//! Roles: source resolution, per-role specs, and the single-session role
//! switch. One daemon, one store; a linked identity's role partitions the
//! session, memory, workspace, grants, toolset, budget and commands.
//!
//! The switch is the `/new` pattern under one guard: take and kill the live
//! port, park the outgoing role's whole conversation state, load the
//! target's (parked, else its latest session row, else fresh). Terminal,
//! web and CLI surfaces are the implicit `admin` role and keep the profile
//! root layout untouched.

use super::intake::drain_queue;
use super::*;
use ka_agent_core::config::RoleSpec;

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
        grants: Default::default(),
    }
}

impl Runtime {
    /// The role a source acts as. Phone identities carry their linked
    /// role; every local surface, scheduled source and unknown source is
    /// admin (the link gates are the authorization).
    pub fn role_for_source(&self, source: &str) -> String {
        // `cli-bg`/`api-bg` background asks speak as their channel.
        let base = source.strip_suffix("-bg").unwrap_or(source);
        if let Some((channel, address)) = base.split_once(':')
            && !address.is_empty()
            && matches!(channel, "telegram" | "matrix" | "web")
        {
            return self
                .sh
                .store
                .identity_role(channel, address)
                .ok()
                .flatten()
                .unwrap_or_else(|| ka_agent_core::ADMIN_ROLE.to_string());
        }
        ka_agent_core::ADMIN_ROLE.to_string()
    }

    /// The role the live session belongs to.
    pub fn active_role(&self) -> String {
        self.lock().role.clone()
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

    /// The active role's effective permission mode: a `/mode` choice made
    /// inside the role's ceiling for this session, else the ceiling (the
    /// profile mode for admin).
    pub fn effective_mode(&self) -> String {
        let (role, session_mode) = {
            let g = self.lock();
            (g.role.clone(), g.session_mode.clone())
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

    /// A role's workspace: the explicit config path, else
    /// `<data>/roles/<name>/workspace`; admin keeps the profile workspace.
    pub fn workspace_for_role(&self, role: &str) -> std::path::PathBuf {
        if role == ka_agent_core::ADMIN_ROLE {
            return self.cfg().workspace_path();
        }
        match self.role_spec(role) {
            Some(spec) => spec.workspace_path(self.sh.paths.root()),
            None => self.sh.paths.role_workspace(role),
        }
    }

    /// The role's latest session, or a fresh one (inserted).
    pub fn session_for_role(&self, role: &str) -> Result<Session, Error> {
        if let Some(session) = self.sh.store.latest_session_for_role(role)? {
            return Ok(session);
        }
        let session = Session::new_for_role(self.workspace_for_role(role), role);
        self.sh.store.insert_session(&session)?;
        Ok(session)
    }

    /// The store meta key holding a role's budget window (admin keeps the
    /// pre-RBAC `budget` key).
    fn budget_meta_key(role: &str) -> String {
        if role == ka_agent_core::ADMIN_ROLE {
            "budget".to_string()
        } else {
            format!("budget:{role}")
        }
    }

    /// The caps that bind a role: its own USD cap when set, else the
    /// profile's; tokens always follow the profile.
    fn budget_caps(&self, role: &str) -> (Option<f64>, Option<u64>) {
        budget_caps_of(&self.cfg(), role)
    }

    /// A role's budget state: today's saved window when it matches, with
    /// the current caps (a reload/role change always wins).
    fn load_budget(&self, role: &str) -> BudgetState {
        let (usd, tokens) = self.budget_caps(role);
        let mut budget = BudgetState::new(usd, tokens);
        if let Ok(Some(saved)) = self.sh.store.meta_get(&Self::budget_meta_key(role))
            && let Ok(saved) = serde_json::from_str::<BudgetState>(&saved)
            && saved.day == ka_agent_core::day_key(ka_agent_core::now_ms())
        {
            budget = saved;
            budget.daily_usd_cap = usd;
            budget.daily_token_cap = tokens;
        }
        budget
    }

    /// A role's current budget verdict (the active role reads the live
    /// state, a parked one its parked state, else today's saved window).
    pub fn budget_verdict_for(&self, role: &str) -> BudgetVerdict {
        if self.active_role() == role {
            return self.lock().budget.verdict();
        }
        if let Some(parked) = self
            .sh
            .parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(role)
        {
            return parked.budget.verdict();
        }
        self.load_budget(role).verdict()
    }

    /// Switch the live session to `role`. Returns whether it switched.
    /// The outgoing role's conversation is parked whole (session, session
    /// allows, budget, crash history, effort); the target's is restored,
    /// else adopted from its latest session row, else created fresh.
    pub(super) async fn switch_role(&self, role: &str) -> Result<bool, Error> {
        if self.lock().role == role {
            return Ok(false);
        }
        let parked_in = self
            .sh
            .parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(role);
        let (session, session_allow, session_mode, budget, crashes, restart_attempt, effort) =
            match parked_in {
                Some(p) => (
                    p.session,
                    p.session_allow,
                    p.session_mode,
                    p.budget,
                    p.crashes,
                    p.restart_attempt,
                    p.effort,
                ),
                None => {
                    let mut session = self.session_for_role(role)?;
                    // A turn that died with a previous process must not leave
                    // the role looking busy (same policy as boot adoption).
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
                    (
                        session,
                        std::collections::HashSet::new(),
                        None,
                        self.load_budget(role),
                        Vec::new(),
                        0,
                        None,
                    )
                }
            };
        let status = session.status;
        let private = session.private;
        let live_port = {
            let mut g = self.lock();
            if g.role == role {
                return Ok(false);
            }
            if let Some(live) = g.live.as_mut() {
                live.deliberate_close = true;
            }
            let port = g.live.take().map(|l| l.port);
            let outgoing_effort = self
                .sh
                .effort
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let outgoing = ParkedRole {
                session: std::mem::replace(&mut g.session, session),
                session_allow: std::mem::take(&mut g.session_allow),
                session_mode: g.session_mode.take(),
                budget: std::mem::replace(&mut g.budget, budget),
                crashes: std::mem::take(&mut g.crashes),
                restart_attempt: std::mem::replace(&mut g.restart_attempt, 0),
                effort: outgoing_effort,
            };
            let outgoing_role = g.role.clone();
            self.sh
                .parked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(outgoing_role, outgoing);
            g.role = role.to_string();
            g.session_allow = session_allow;
            g.session_mode = session_mode;
            g.crashes = crashes;
            g.restart_attempt = restart_attempt;
            *self.sh.effort.lock().unwrap_or_else(|e| e.into_inner()) = effort;
            port
        };
        if let Some(port) = live_port {
            port.kill().await;
        }
        self.sh.emit(AgentEvent::Settings {
            model: None,
            mode: Some(self.effective_mode()),
            effort: self.effort(),
            private: Some(private),
        });
        self.sh.emit(AgentEvent::Session {
            status: format!("{status:?}"),
        });
        Ok(true)
    }

    /// Switch to one role by name (admin UI session picker). Refuses while
    /// another role's turn owns the core.
    pub async fn use_role(&self, role: &str) -> bool {
        if self.lock().role == role {
            return true;
        }
        if matches!(
            self.session_status(),
            SessionStatus::Running | SessionStatus::AwaitingApproval | SessionStatus::Stopping
        ) {
            return false;
        }
        self.switch_role(role).await.is_ok()
    }

    /// Make `source`'s role the active one, when the live session is not
    /// mid-turn. Returns `false` while another role's turn owns the
    /// single-session core (the caller says so instead of disturbing it).
    pub async fn activate_role(&self, source: &str) -> bool {
        let role = self.role_for_source(source);
        if self.lock().role == role {
            return true;
        }
        if matches!(
            self.session_status(),
            SessionStatus::Running | SessionStatus::AwaitingApproval | SessionStatus::Stopping
        ) {
            return false;
        }
        self.switch_role(&role).await.is_ok()
    }

    /// After a turn settles: hand the core to a role with queued messages.
    pub(super) async fn yield_to_waiting_role(&self) {
        if self.session_status() != SessionStatus::Idle {
            return;
        }
        let active = self.active_role();
        let mut roles = vec![ka_agent_core::ADMIN_ROLE.to_string()];
        roles.extend(self.cfg().roles.iter().map(|r| r.name.clone()));
        for role in roles {
            if role == active {
                continue;
            }
            let Ok(Some(session)) = self.sh.store.latest_session_for_role(&role) else {
                continue;
            };
            if self.sh.store.queue_len(&session.id).unwrap_or(0) == 0 {
                continue;
            }
            if self.switch_role(&role).await.is_err() {
                continue;
            }
            {
                let mut g = self.lock();
                if g.session.status != SessionStatus::Idle {
                    return;
                }
                if g.session.transition(SessionStatus::Running).is_err() {
                    return;
                }
            }
            self.persist_session_status(SessionStatus::Running);
            if !drain_queue(&self.sh).await {
                {
                    let mut g = self.lock();
                    if g.session.status == SessionStatus::Running {
                        g.session.transition(SessionStatus::Idle).ok();
                    }
                }
                self.persist_session_status(SessionStatus::Idle);
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
    use ka_agent_core::DaemonConfig;
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
    fn sources_resolve_to_their_linked_role() {
        let rt = runtime(cfg_with_guest());
        rt.store()
            .link_identity_as("telegram", "42", "cli", "guest")
            .unwrap();
        assert_eq!(rt.role_for_source("telegram:42"), "guest");
        assert_eq!(rt.role_for_source("telegram:99"), "admin", "unlinked");
        assert_eq!(rt.role_for_source("matrix:@a:b"), "admin");
        // The web relay's role logins resolve like any other identity;
        // bare `web` (the loopback panel) stays admin.
        rt.store()
            .link_identity_as("web", "u1", "pair", "guest")
            .unwrap();
        assert_eq!(rt.role_for_source("web:u1"), "guest");
        assert_eq!(rt.role_for_source("web"), "admin");
        for local in ["console", "cli", "web", "api", "cli-bg", "api-bg"] {
            assert_eq!(rt.role_for_source(local), "admin", "{local}");
        }
        assert_eq!(rt.role_for_source("cron"), "admin");
        assert_eq!(rt.role_for_source("briefing"), "admin");
        assert_eq!(rt.role_for_source("retry"), "admin");
    }

    #[test]
    fn unknown_roles_fail_closed() {
        let rt = runtime(cfg_with_guest());
        let spec = rt.effective_role("ghost");
        assert_eq!(spec.mode_canonical().as_deref(), Some("readonly"));
        assert!(spec.disable.iter().any(|s| s == "tool:bash"));
        assert!(rt.role_spec("admin").is_none(), "admin is implicit");
        assert_eq!(rt.effective_mode(), "ask");
    }

    #[test]
    fn guard_lists_separate_role_trees() {
        let rt = runtime(cfg_with_guest());
        let root = rt.sh.paths.root().to_path_buf();
        let admin_write = rt.guard_write_paths("admin");
        assert!(admin_write.contains(&root.join("roles")));
        let guest_write = rt.guard_write_paths("guest");
        assert!(!guest_write.contains(&root.join("roles")));
        let own_ws = rt.workspace_for_role("guest");
        assert!(
            !guest_write.iter().any(|p| own_ws.starts_with(p)),
            "a role may write its own workspace: {guest_write:?}"
        );
        let admin_ws = rt.workspace_for_role("admin");
        assert!(
            guest_write.iter().any(|p| admin_ws.starts_with(p)),
            "a role may not write the admin workspace"
        );
        let guest_read = rt.guard_read_paths("guest");
        for blocked in ["memory", "notes", "ka", "engine-home", "state.db"] {
            assert!(
                guest_read.contains(&root.join(blocked)),
                "role read guard misses {blocked}"
            );
        }
        assert!(
            !guest_read.contains(&root.join("roles/guest")),
            "a role reads its own tree"
        );
        let admin_read = rt.guard_read_paths("admin");
        assert!(admin_read.contains(&root.join("roles/guest")));
    }

    #[test]
    fn role_budget_caps_follow_the_config() {
        let mut cfg = cfg_with_guest();
        cfg.budgets.daily_usd = Some(5.0);
        cfg.budgets.daily_tokens = Some(1000);
        cfg.roles[0].budget_daily_usd = Some(2.0);
        let guest = budget_caps_of(&cfg, "guest");
        assert_eq!(guest, (Some(2.0), Some(1000)));
        let admin = budget_caps_of(&cfg, admin());
        assert_eq!(admin, (Some(5.0), Some(1000)));
        cfg.roles[0].budget_daily_usd = None;
        assert_eq!(budget_caps_of(&cfg, "guest"), (Some(5.0), Some(1000)));
    }

    #[test]
    fn cron_tools_only_narrow_the_role_toolset() {
        let mut cfg = cfg_with_guest();
        cfg.roles[0].mode = "allow".into();
        cfg.roles[0].allow = vec!["read".into(), "grep".into()];
        let rt = runtime(cfg);
        rt.validate_cron_tools("guest", &["read".into()]).unwrap();
        let err = rt
            .validate_cron_tools("guest", &["read".into(), "bash".into()])
            .unwrap_err();
        assert!(err.contains("bash"), "{err}");
        assert_eq!(
            rt.clamp_cron_tools("guest", &["read".into(), "bash".into()]),
            vec!["read"]
        );
        // Disable-style roles reject a tool their specs mute.
        let mut cfg = cfg_with_guest();
        cfg.roles[0].allow.clear();
        cfg.roles[0].disable = vec!["tool:bash".into()];
        let rt = runtime(cfg);
        assert!(rt.validate_cron_tools("guest", &["read".into()]).is_ok());
        assert!(rt.validate_cron_tools("guest", &["bash".into()]).is_err());
        assert!(rt.clamp_cron_tools("guest", &["bash".into()]).is_empty());
    }

    #[tokio::test]
    async fn crons_are_scoped_to_their_role() {
        let rt = runtime(cfg_with_guest());
        let now = rt.now_zoned();
        let (spec, at) =
            ka_agent_core::CronSpec::from_words(&["every", "1h", "admin job"], "operator", &now)
                .unwrap();
        let admin_id = rt.add_cron(spec, at).unwrap();
        let (mut guest_spec, at) =
            ka_agent_core::CronSpec::from_words(&["every", "1h", "guest job"], "operator", &now)
                .unwrap();
        guest_spec.role = Some("guest".into());
        let guest_id = rt.add_cron(guest_spec, at).unwrap();

        let guest = rt.list_crons_for("guest").unwrap();
        assert_eq!(guest.len(), 1, "{guest:?}");
        assert_eq!(guest[0].id, guest_id);
        assert_eq!(rt.list_crons_for("admin").unwrap().len(), 2);
        // A subject cannot cancel (or even see) the operator's job.
        assert!(matches!(
            rt.cancel_scheduled_for("guest", admin_id.as_str()).unwrap(),
            CancelOutcome::NotFound
        ));
        assert!(matches!(
            rt.cancel_scheduled_for("guest", guest_id.as_str()).unwrap(),
            CancelOutcome::Cancelled(_)
        ));
    }

    #[tokio::test]
    async fn use_role_switches_when_idle() {
        let rt = runtime(cfg_with_guest());
        assert!(rt.use_role("guest").await);
        assert_eq!(rt.active_role(), "guest");
        assert!(rt.use_role("admin").await);
        assert_eq!(rt.active_role(), "admin");
    }

    #[tokio::test]
    async fn role_budget_state_is_parked_and_restored() {
        let mut cfg = cfg_with_guest();
        cfg.roles[0].budget_daily_usd = Some(2.0);
        let rt = runtime(cfg);
        assert!(rt.switch_role("guest").await.unwrap());
        {
            let mut g = rt.lock();
            g.budget.record(3.0, 10, ka_agent_core::now_ms());
        }
        assert_eq!(
            rt.budget_verdict_for("guest"),
            ka_agent_core::BudgetVerdict::Exceeded
        );
        assert_ne!(
            rt.budget_verdict_for("admin"),
            ka_agent_core::BudgetVerdict::Exceeded,
            "the admin's own budget is untouched"
        );
        // The spent window rides the switch away and back.
        assert!(rt.switch_role("admin").await.unwrap());
        assert_eq!(
            rt.budget_verdict_for("guest"),
            ka_agent_core::BudgetVerdict::Exceeded
        );
        assert_ne!(
            rt.budget_verdict_for("admin"),
            ka_agent_core::BudgetVerdict::Exceeded
        );
    }

    #[tokio::test]
    async fn subject_mode_is_clamped_to_the_ceiling() {
        let rt = runtime(cfg_with_guest());
        assert!(rt.switch_role("guest").await.unwrap());
        let reply = rt.set_mode("allow", "telegram:77").await.unwrap();
        assert!(reply.contains("readonly"), "{reply}");
        assert_eq!(rt.effective_mode(), "readonly");
        // Stricter than the ceiling is accepted as-is.
        let reply = rt.set_mode("readonly", "telegram:77").await.unwrap();
        assert_eq!(reply, "readonly");
    }

    fn admin() -> &'static str {
        ka_agent_core::ADMIN_ROLE
    }

    #[tokio::test]
    async fn switching_parks_and_restores_each_roles_conversation() {
        let rt = runtime(cfg_with_guest());
        let admin_session = rt.session_id();
        assert_eq!(rt.active_role(), "admin");

        assert!(rt.switch_role("guest").await.unwrap());
        assert_eq!(rt.active_role(), "guest");
        assert_eq!(rt.session_status(), SessionStatus::Idle);
        let guest_session = rt.session_id();
        assert_ne!(guest_session, admin_session);
        assert_eq!(rt.effective_mode(), "readonly");
        assert_eq!(
            rt.workspace_for_role("guest"),
            rt.sh.paths.root().join("roles/guest/workspace")
        );
        // The guest session row carries the role.
        let row = rt.store().get_session(&guest_session).unwrap();
        assert_eq!(row.role, "guest");

        // Back to admin: the original conversation returns.
        assert!(rt.switch_role("admin").await.unwrap());
        assert_eq!(rt.session_id(), admin_session);
        assert_eq!(rt.effective_mode(), "ask");

        // And to guest again: the same guest session (adopted/parked).
        assert!(rt.switch_role("guest").await.unwrap());
        assert_eq!(rt.session_id(), guest_session);
    }
}
