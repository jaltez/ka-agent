//! The Slack workspace app: Socket Mode (one outbound WSS), DMs as the
//! user's private session, `@mention` in shared channels under the
//! read-only `channel` role, Block Kit approval buttons, file exchange.
//!
//! Why Socket Mode: a VPS behind the existing web reverse proxy must not
//! expose a second public endpoint. `apps.connections.open` (app-level
//! token) returns a short-lived WSS URL; every inbound frame is an envelope
//! that must be acked immediately, then dispatched by `type`
//! (`events_api`, `interactive`, `slash_commands`, `disconnect`).

pub mod client;
pub mod socket;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ka_agent_core::{DaemonConfig, Principal};
use ka_agent_runtime::{AgentEvent, Runtime, Surface};
use ka_agent_store::Store;
use serde_json::{Value, json};
use tokio::sync::watch;

use client::SlackClient;
use crate::ratelimit::RateLimiter;

/// Everything the connector needs, resolved once at startup.
#[derive(Clone)]
pub struct SlackConfig {
    pub api_base: String,
    pub app_token: String,
    pub bot_token: String,
    pub teams: Vec<String>,
    pub auto_provision: bool,
    pub shared_channel_role: String,
    pub default_role: String,
}

impl SlackConfig {
    /// Resolve `[slack]` + the secret store; `None` when disabled.
    pub fn from_config(cfg: &DaemonConfig, secrets: &std::path::Path) -> Result<Option<Self>, String> {
        if !cfg.slack.enabled {
            return Ok(None);
        }
        let secret = |name: &Option<String>| -> Result<String, String> {
            let name = name
                .as_deref()
                .ok_or_else(|| "slack: a secret name is missing".to_string())?;
            ka_agent_core::secret::resolve(name, secrets)
                .map_err(|e| format!("slack secret {name}: {e}"))
        };
        Ok(Some(Self {
            api_base: cfg.slack.api_base().to_string(),
            app_token: secret(&cfg.slack.app_token_secret)?,
            bot_token: secret(&cfg.slack.bot_token_secret)?,
            teams: cfg.slack.teams.clone(),
            auto_provision: cfg.slack.auto_provision,
            shared_channel_role: cfg.channels.shared_channel_role.clone(),
            default_role: cfg.channels.default_role.clone(),
        }))
    }
}

/// A valid account name for a shared surface: `slack-T1-C01ABC` (lowercase,
/// `[a-z0-9._-]`), stable for a given workspace channel.
pub fn surface_username(platform: &str, team: &str, channel: &str) -> String {
    let raw = format!("{platform}-{team}-{channel}").to_ascii_lowercase();
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    name.truncate(64);
    name
}

/// Where a reply goes: the conversation and (for mentions) its thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTarget {
    pub channel: String,
    pub thread_ts: Option<String>,
    /// A shared (workspace) channel: the answer's provenance is re-checked
    /// against the audience before posting.
    pub shared: bool,
    /// The account the turn ran as (the audience's groups come from it).
    pub username: String,
}

/// The Slack surface: delivers the runtime's chunks to the conversation the
/// turn came from and renders approval cards with Block Kit buttons.
struct SlackSurface {
    client: Arc<SlackClient>,
    /// source (`slack:T123:U1`) → where its turn replies go.
    targets: Mutex<HashMap<String, ReplyTarget>>,
    /// The runtime, for the share-time provenance check (M3).
    runtime: Mutex<Option<Runtime>>,
}

impl Surface for SlackSurface {
    fn emit(&self, env: &ka_agent_runtime::Envelope) {
        let Some(source) = env.source.clone() else {
            // Escalated cards have no source: they belong to the admin
            // queue, which the panel and the operator's own channels show.
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
            // Final answers only: Slack messages are not edited while a turn
            // streams (chat.update churn would reorder threads).
            AgentEvent::FinalChunk { text, turn, .. } => {
                if text.trim().is_empty() {
                    return;
                }
                // Share-time provenance (M3): a shared channel only receives
                // an answer whose sources its audience may see.
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
                            "slack",
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
                        .post_message(&target.channel, &text, target.thread_ts.as_deref())
                        .await
                    {
                        eprintln!("slack: {e}");
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
                    let blocks = approval_blocks(&id, &question, detail.as_deref(), expires);
                    if let Err(e) = client
                        .post_blocks(&target.channel, &question, target.thread_ts.as_deref(), blocks)
                        .await
                    {
                        eprintln!("slack: approval card: {e}");
                    }
                });
            }
            AgentEvent::Notice { text } | AgentEvent::Note { text } => {
                let text = text.clone();
                tokio::spawn(async move {
                    let _ = client
                        .post_message(&target.channel, &text, target.thread_ts.as_deref())
                        .await;
                });
            }
            _ => {}
        }
    }
}

/// The Block Kit card for one approval: the question, its detail, and two
/// buttons whose action ids carry the approval id (authorization is
/// server-side in `Runtime::decide`).
pub fn approval_blocks(
    id: &str,
    question: &str,
    detail: Option<&str>,
    expires_at_ms: i64,
) -> Value {
    let mut blocks = vec![json!({
        "type": "section",
        "text": {"type": "mrkdwn", "text": format!("*Approval needed*\n{question}")}
    })];
    if let Some(detail) = detail.filter(|d| !d.trim().is_empty()) {
        let short: String = detail.lines().take(20).collect::<Vec<_>>().join("\n");
        blocks.push(json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": format!("```{short}```")}
        }));
    }
    blocks.push(json!({
        "type": "context",
        "elements": [{"type": "mrkdwn", "text": format!(
            "expires {} · only the person this turn belongs to (or an admin) can decide",
            ka_agent_core::fmt_interval(expires_at_ms - ka_agent_core::now_ms())
        )}]
    }));
    blocks.push(json!({
        "type": "actions",
        "block_id": format!("ka:{id}"),
        "elements": [
            {"type": "button", "style": "primary", "text": {"type": "plain_text", "text": "Approve"},
             "action_id": format!("ka_approve:{id}")},
            {"type": "button", "style": "danger", "text": {"type": "plain_text", "text": "Deny"},
             "action_id": format!("ka_deny:{id}")}
        ]
    }));
    Value::Array(blocks)
}

/// The connector's runtime-free parts: the API client, the surface (which
/// must join the fan-out before the runtime exists) and the rate limiter.
pub struct SlackParts {
    pub client: Arc<SlackClient>,
    surface: Arc<SlackSurface>,
}

impl SlackParts {
    pub fn new(cfg: &SlackConfig) -> Self {
        let limiter = Arc::new(RateLimiter::new());
        let client = Arc::new(SlackClient::new(
            &cfg.api_base,
            cfg.bot_token.clone(),
            Arc::clone(&limiter),
        ));
        let surface = Arc::new(SlackSurface {
            client: Arc::clone(&client),
            targets: Mutex::new(HashMap::new()),
            runtime: Mutex::new(None),
        });
        let _ = &limiter;
        Self { client, surface }
    }

    /// The surface to register in the daemon's fan-out.
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

/// The Socket Mode connector.
pub struct SlackChannel {
    cfg: SlackConfig,
    runtime: Runtime,
    store: Arc<Store>,
    client: Arc<SlackClient>,
    surface: Arc<SlackSurface>,
    /// The bot's own user id (`U…`, from `auth.test`), used to strip the
    /// mention from a channel message.
    mention: Mutex<Option<String>>,
}

impl SlackChannel {
    pub fn new(cfg: SlackConfig, runtime: Runtime, store: Arc<Store>, parts: SlackParts) -> Self {
        Self {
            cfg,
            runtime,
            store,
            client: parts.client,
            surface: parts.surface,
            mention: Mutex::new(None),
        }
    }

    /// The `<@U…>` token that addresses the bot, learned once (`auth.test`).
    async fn mention_token(&self) -> String {
        let cached = self
            .mention
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(id) = cached {
            return format!("<@{id}>");
        }
        let id = self
            .client
            .auth_test()
            .await
            .ok()
            .and_then(|v| v["user_id"].as_str().map(str::to_string))
            .unwrap_or_default();
        *self.mention.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
        format!("<@{id}>")
    }

    /// The channel's surface, for `MultiSurface`.
    pub fn surface(&self) -> Arc<dyn Surface> {
        self.surface.clone()
    }

    /// Run until shutdown: open a Socket Mode session, dispatch frames,
    /// reopen on `disconnect` or a dropped socket (backoff, capped).
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let backoff = ka_agent_runtime::runtime::Backoff::new(1_000, 60_000);
        let mut attempt = 0u32;
        loop {
            if *shutdown.borrow() {
                return;
            }
            let url = match self.client.connections_open(&self.cfg.app_token).await {
                Ok(url) => url,
                Err(e) => {
                    eprintln!("slack: {e}; retrying");
                    let wait = backoff.delay(attempt);
                    attempt = attempt.saturating_add(1);
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        _ = tokio::time::sleep(wait) => {}
                    }
                    continue;
                }
            };
            attempt = 0;
            match socket::run_session(&url, &self, &mut shutdown).await
            {
                socket::SessionEnd::Shutdown => return,
                socket::SessionEnd::Disconnected => {
                    // `disconnect` (refresh_requested) is routine: reconnect
                    // immediately.
                }
                socket::SessionEnd::Failed(e) => {
                    eprintln!("slack: socket: {e}");
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

    /// Resolve the user a Slack sender acts as, provisioning when the
    /// config allows it. `None` = refuse (unknown workspace or sender).
    async fn principal_for(&self, team: &str, user: &str) -> Option<Principal> {
        if !self.cfg.teams.iter().any(|t| t == team) {
            return None;
        }
        let address = format!("{team}:{user}");
        if let Ok(Some((username, role))) = self.store.identity_principal("slack", &address) {
            return Some(Principal::new(&username, &role));
        }
        if !self.cfg.auto_provision {
            return None;
        }
        // Name the account after the workspace handle when Slack gives us
        // one; otherwise after the id.
        let handle = self
            .client
            .call("users.info", json!({"user": user}))
            .await
            .ok()
            .and_then(|v| {
                v["user"]["profile"]["display_name"]
                    .as_str()
                    .or_else(|| v["user"]["name"].as_str())
                    .map(|s| s.to_string())
            });
        let candidate = handle
            .map(|h| {
                h.to_ascii_lowercase()
                    .chars()
                    .map(|c| {
                        if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
                        {
                            c
                        } else {
                            '-'
                        }
                    })
                    .collect::<String>()
            })
            .filter(|h| ka_agent_store::valid_username(h))
            .unwrap_or_else(|| format!("slack-{}", user.to_ascii_lowercase()));
        let username = if self.store.get_user(&candidate).ok().flatten().is_some() {
            format!("{candidate}-{}", user.chars().rev().take(4).collect::<String>())
        } else {
            candidate
        };
        let user_row = ka_agent_store::UserRow {
            username: username.clone(),
            display_name: username.clone(),
            pw_hash: None,
            role: self.cfg.default_role.clone(),
            quota_daily_usd: None,
            disabled: false,
            created_ms: ka_agent_core::now_ms(),
            created_by: Some("slack".into()),
        };
        self.store.create_user(&user_row).ok()?;
        self.store
            .link_identity_to_user("slack", &address, "slack", &self.cfg.default_role, &username)
            .ok()?;
        let mut rec = ka_agent_core::AuditRecord::new(
            "slack",
            "user.auto_provision",
            &username,
            Some(format!("team {team} user {user}")),
        )
        .with_role(&self.cfg.default_role)
        .with_user(&username);
        let _ = self.store.append_audit(&mut rec);
        Some(Principal::new(&username, &self.cfg.default_role))
    }

    /// The principal a shared-channel turn runs as: a **surface account**
    /// per channel (never the person's own user), so the conversation has
    /// its own partition and no personal memory, notes or grants can leak
    /// into a shared room.
    async fn channel_principal(&self, team: &str, channel_id: &str) -> Principal {
        let username = surface_username("slack", team, channel_id);
        if self.store.get_user(&username).ok().flatten().is_none() {
            let user = ka_agent_store::UserRow {
                username: username.clone(),
                display_name: format!("Slack channel {channel_id}"),
                pw_hash: None,
                role: self.cfg.shared_channel_role.clone(),
                quota_daily_usd: None,
                disabled: false,
                created_ms: ka_agent_core::now_ms(),
                created_by: Some("slack".into()),
            };
            let _ = self.store.create_user(&user);
            let mut rec = ka_agent_core::AuditRecord::new(
                "slack",
                "channel.account",
                &username,
                Some(format!("shared channel {team}/{channel_id}")),
            )
            .with_role(&self.cfg.shared_channel_role)
            .with_user(&username);
            let _ = self.store.append_audit(&mut rec);
        }
        Principal::new(&username, &self.cfg.shared_channel_role)
    }

    fn remember_target(&self, source: &str, target: ReplyTarget) {
        self.surface
            .targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source.to_string(), target);
    }
}
