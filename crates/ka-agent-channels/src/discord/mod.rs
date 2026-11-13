//! The Discord bot: gateway WebSocket (no public endpoint), DMs as the
//! user's private session, `@mention` in guild channels under the read-only
//! channel account, button approvals, file exchange.
//!
//! Lifecycle (docs.discord.com/developers/docs/events/gateway): Hello (op
//! 10) → Heartbeat (op 1, ACK op 11) → Identify (op 2) → Ready (op 0, cache
//! `session_id` + `resume_gateway_url`) → Resume (op 6, seq) after a drop.
//! Identify is capped at 1000/24 h globally, so a reconnect resumes.

pub mod client;
pub mod gateway;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ka_agent_core::{DaemonConfig, Principal};
use ka_agent_runtime::{AgentEvent, Runtime, Surface};
use ka_agent_store::Store;
use serde_json::json;
use tokio::sync::watch;

use crate::ratelimit::RateLimiter;
use client::DiscordClient;

/// Privileged `MESSAGE_CONTENT` (1 << 15) plus guilds, guild messages and
/// DMs: the minimum that can read a message and a mention.
pub const INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15);

#[derive(Clone)]
pub struct DiscordConfig {
    pub api_base: String,
    pub bot_token: String,
    pub guilds: Vec<String>,
    pub auto_provision: bool,
    pub shared_channel_role: String,
    pub default_role: String,
}

impl DiscordConfig {
    pub fn from_config(
        cfg: &DaemonConfig,
        secrets: &std::path::Path,
    ) -> Result<Option<Self>, String> {
        if !cfg.discord.enabled {
            return Ok(None);
        }
        let name = cfg
            .discord
            .bot_token_secret
            .as_deref()
            .ok_or_else(|| "discord: bot_token_secret is missing".to_string())?;
        let bot_token = ka_agent_core::secret::resolve(name, secrets)
            .map_err(|e| format!("discord secret {name}: {e}"))?;
        Ok(Some(Self {
            api_base: cfg.discord.api_base().to_string(),
            bot_token,
            guilds: cfg.discord.guilds.clone(),
            auto_provision: cfg.discord.auto_provision,
            shared_channel_role: cfg.channels.shared_channel_role.clone(),
            default_role: cfg.channels.default_role.clone(),
        }))
    }
}

/// Where a reply goes: the channel and (for guild mentions) its thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTarget {
    pub channel: String,
    pub thread_name: Option<String>,
    /// A shared (guild) channel: the answer's provenance is re-checked
    /// against the audience before posting.
    pub shared: bool,
    /// The account the turn ran as (the audience's groups come from it).
    pub username: String,
}

/// The Discord surface: posts final answers and approval cards.
struct DiscordSurface {
    client: Arc<DiscordClient>,
    targets: Mutex<HashMap<String, ReplyTarget>>,
    /// The runtime, for the share-time provenance check (M3).
    runtime: Mutex<Option<Runtime>>,
}

impl Surface for DiscordSurface {
    fn emit(&self, env: &ka_agent_runtime::Envelope) {
        let Some(source) = env.source.clone() else {
            return;
        };
        let target = {
            let map = self.targets.lock().unwrap_or_else(|e| e.into_inner());
            map.get(&source).cloned()
        };
        let Some(target) = target else {
            return;
        };
        let client = Arc::clone(&self.client);
        match &env.ev {
            AgentEvent::FinalChunk { text, turn, .. } => {
                if text.trim().is_empty() {
                    return;
                }
                if target.shared
                    && let Some(runtime) = self
                        .runtime
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                {
                    let audience = runtime.groups_of_user(&target.username);
                    if let Some(conflict) = runtime.provenance_conflict(turn, &audience) {
                        runtime.emit(AgentEvent::Notice {
                            text: format!(
                                "blocked a shared-channel answer: its source {} is limited to group(s) {} the channel cannot see",
                                conflict.reference,
                                conflict.group_names.join(", ")
                            ),
                        });
                        let mut rec = ka_agent_core::AuditRecord::new(
                            "discord",
                            "provenance.blocked",
                            &conflict.reference,
                            Some(format!("turn {turn} → channel {}", target.channel)),
                        );
                        let _ = runtime.store().append_audit(&mut rec);
                        return;
                    }
                }
                let text = text.clone();
                tokio::spawn(async move {
                    if let Err(e) = client
                        .send_message(&target.channel, json!({"content": text}))
                        .await
                    {
                        eprintln!("discord: {e}");
                    }
                });
            }
            AgentEvent::Approval {
                id,
                question,
                detail,
                expires_at_ms,
                ..
            } => {
                let (id, question, detail, expires) = (
                    id.clone(),
                    question.clone(),
                    detail.clone(),
                    *expires_at_ms,
                );
                tokio::spawn(async move {
                    let mut content = format!("**Approval needed**\n{question}");
                    if let Some(d) = detail.filter(|d| !d.trim().is_empty()) {
                        let short: String = d.lines().take(20).collect::<Vec<_>>().join("\n");
                        content.push_str(&format!("\n```{short}```"));
                    }
                    content.push_str(&format!(
                        "\nexpires {} · only the person this turn belongs to (or an admin) can decide",
                        ka_agent_core::fmt_interval(expires - ka_agent_core::now_ms())
                    ));
                    let body = json!({
                        "content": content,
                        "components": [{
                            "type": 1,
                            "components": [
                                {"type": 2, "style": 3, "label": "Approve",
                                 "custom_id": format!("ka_approve:{id}")},
                                {"type": 2, "style": 4, "label": "Deny",
                                 "custom_id": format!("ka_deny:{id}")}
                            ]
                        }]
                    });
                    if let Err(e) = client.send_message(&target.channel, body).await {
                        eprintln!("discord: approval card: {e}");
                    }
                });
            }
            AgentEvent::Notice { text } | AgentEvent::Note { text } => {
                let text = text.clone();
                tokio::spawn(async move {
                    let _ = client
                        .send_message(&target.channel, json!({"content": text}))
                        .await;
                });
            }
            _ => {}
        }
    }
}

/// The connector's runtime-free parts (surface joins the fan-out first).
pub struct DiscordParts {
    pub client: Arc<DiscordClient>,
    surface: Arc<DiscordSurface>,
    limiter: Arc<RateLimiter>,
}

impl DiscordParts {
    pub fn new(cfg: &DiscordConfig) -> Self {
        let limiter = Arc::new(RateLimiter::new());
        let client = Arc::new(DiscordClient::new(
            &cfg.api_base,
            cfg.bot_token.clone(),
            Arc::clone(&limiter),
        ));
        let surface = Arc::new(DiscordSurface {
            client: Arc::clone(&client),
            targets: Mutex::new(HashMap::new()),
            runtime: Mutex::new(None),
        });
        Self {
            client,
            surface,
            limiter,
        }
    }

    pub fn surface(&self) -> Arc<dyn Surface> {
        self.surface.clone()
    }

    /// Hand the surface the runtime (needed only for the provenance check).
    pub fn attach_runtime(&self, runtime: &Runtime) {
        *self
            .surface
            .runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(runtime.clone());
    }
}

pub struct DiscordChannel {
    cfg: DiscordConfig,
    runtime: Runtime,
    store: Arc<Store>,
    client: Arc<DiscordClient>,
    surface: Arc<DiscordSurface>,
    #[allow(dead_code)]
    limiter: Arc<RateLimiter>,
    /// The bot's own user id (Ready), for stripping mentions.
    bot_user: Mutex<Option<String>>,
}

impl DiscordChannel {
    pub fn new(cfg: DiscordConfig, runtime: Runtime, store: Arc<Store>, parts: DiscordParts) -> Self {
        Self {
            cfg,
            runtime,
            store,
            client: parts.client,
            surface: parts.surface,
            limiter: parts.limiter,
            bot_user: Mutex::new(None),
        }
    }

    pub fn surface(&self) -> Arc<dyn Surface> {
        self.surface.clone()
    }

    /// Gateway loop with resume: Identify once, then resume after drops.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut session: Option<gateway::Session> = None;
        let backoff = ka_agent_runtime::runtime::Backoff::new(1_000, 60_000);
        let mut attempt = 0u32;
        loop {
            if *shutdown.borrow() {
                return;
            }
            let url = match &session {
                Some(s) if s.can_resume() => s.resume_url.clone(),
                _ => match self.client.gateway_bot().await {
                    Ok(v) => format!(
                        "{}/?v=10&encoding=json",
                        v["url"].as_str().unwrap_or("wss://gateway.discord.gg")
                    ),
                    Err(e) => {
                        eprintln!("discord: {e}; retrying");
                        let wait = backoff.delay(attempt);
                        attempt = attempt.saturating_add(1);
                        tokio::select! {
                            _ = shutdown.changed() => return,
                            _ = tokio::time::sleep(wait) => {}
                        }
                        continue;
                    }
                },
            };
            match gateway::run_gateway(&url, &self, session.take(), &mut shutdown).await {
                gateway::GatewayEnd::Shutdown => return,
                gateway::GatewayEnd::Reconnect(next) => {
                    attempt = 0;
                    session = next;
                }
                gateway::GatewayEnd::Failed(e, keep) => {
                    eprintln!("discord: gateway: {e}");
                    session = keep;
                    let wait = backoff.delay(attempt);
                    attempt = attempt.saturating_add(1);
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            }
        }
    }

    fn set_bot_user(&self, id: &str) {
        *self.bot_user.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_string());
    }

    fn bot_user(&self) -> Option<String> {
        self.bot_user
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn remember_target(&self, source: &str, target: ReplyTarget) {
        self.surface
            .targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source.to_string(), target);
    }

    /// Resolve the user a Discord sender acts as (provisioning when the
    /// config allows it). `None` = refuse.
    fn principal_for(&self, guild: &str, user: &str) -> Option<Principal> {
        let address = if guild.is_empty() {
            format!("dm:{user}")
        } else {
            format!("{guild}:{user}")
        };
        if let Ok(Some((username, role))) = self.store.identity_principal("discord", &address) {
            return Some(Principal::new(&username, &role));
        }
        if !self.cfg.auto_provision || guild.is_empty() {
            return None;
        }
        let username = super::slack::surface_username("discord", guild, user);
        let user_row = ka_agent_store::UserRow {
            username: username.clone(),
            display_name: username.clone(),
            pw_hash: None,
            role: self.cfg.default_role.clone(),
            quota_daily_usd: None,
            disabled: false,
            created_ms: ka_agent_core::now_ms(),
            created_by: Some("discord".into()),
        };
        self.store.create_user(&user_row).ok()?;
        self.store
            .link_identity_to_user("discord", &address, "discord", &self.cfg.default_role, &username)
            .ok()?;
        let mut rec = ka_agent_core::AuditRecord::new(
            "discord",
            "user.auto_provision",
            &username,
            Some(format!("guild {guild} user {user}")),
        )
        .with_role(&self.cfg.default_role)
        .with_user(&username);
        let _ = self.store.append_audit(&mut rec);
        Some(Principal::new(&username, &self.cfg.default_role))
    }

    /// The per-channel surface account of a guild channel (never the
    /// person's own user).
    fn channel_principal(&self, guild: &str, channel: &str) -> Principal {
        let username = super::slack::surface_username("discord", guild, channel);
        if self.store.get_user(&username).ok().flatten().is_none() {
            let user = ka_agent_store::UserRow {
                username: username.clone(),
                display_name: format!("Discord channel {channel}"),
                pw_hash: None,
                role: self.cfg.shared_channel_role.clone(),
                quota_daily_usd: None,
                disabled: false,
                created_ms: ka_agent_core::now_ms(),
                created_by: Some("discord".into()),
            };
            let _ = self.store.create_user(&user);
            let mut rec = ka_agent_core::AuditRecord::new(
                "discord",
                "channel.account",
                &username,
                Some(format!("guild channel {guild}/{channel}")),
            )
            .with_role(&self.cfg.shared_channel_role)
            .with_user(&username);
            let _ = self.store.append_audit(&mut rec);
        }
        Principal::new(&username, &self.cfg.shared_channel_role)
    }

    fn audit(&self, action: &str, subject: &str, detail: Option<String>) {
        let mut rec = ka_agent_core::AuditRecord::new("discord", action, subject, detail);
        let _ = self.store.append_audit(&mut rec);
    }
}
