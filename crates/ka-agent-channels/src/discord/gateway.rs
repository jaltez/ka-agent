//! The Discord gateway session: Hello → Identify/Resume → dispatch loop,
//! with heartbeats and a resume-aware reconnect.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use super::{DiscordChannel, ReplyTarget};

/// Resume state carried across connections (≤ 1000 Identifies / 24 h).
#[derive(Debug, Clone)]
pub struct Session {
    pub session_id: String,
    pub resume_url: String,
    pub seq: Option<i64>,
}

impl Session {
    /// Whether a drop can be resumed (Discord keeps the session alive for a
    /// few minutes).
    pub fn can_resume(&self) -> bool {
        !self.session_id.is_empty() && !self.resume_url.is_empty()
    }
}

/// Why a gateway session ended.
pub enum GatewayEnd {
    Shutdown,
    /// Resume on the same session (the usual reconnect).
    Reconnect(Option<Session>),
    /// A hard failure; `keep` resumes when possible.
    Failed(String, Option<Session>),
}

pub(super) async fn run_gateway(
    url: &str,
    channel: &Arc<DiscordChannel>,
    mut resume: Option<Session>,
    shutdown: &mut watch::Receiver<bool>,
) -> GatewayEnd {
    let (mut ws, _) = match tokio_tungstenite::connect_async(url).await {
        Ok(pair) => pair,
        Err(e) => return GatewayEnd::Failed(format!("connect: {e}"), resume),
    };
    let mut seq: Option<i64> = resume.as_ref().and_then(|r| r.seq);
    let mut session: Option<Session> = resume.take();
    let mut heartbeat: tokio::time::Interval;
    let mut awaiting_ack = false;
    // Hello must arrive first.
    loop {
        let frame = tokio::select! {
            _ = shutdown.changed() => return GatewayEnd::Shutdown,
            frame = ws.next() => frame,
        };
        let Some(Ok(Message::Text(text))) = frame else {
            return GatewayEnd::Reconnect(session);
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match value["op"].as_i64() {
            Some(10) => {
                let interval = value["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250);
                let mut ticker = tokio::time::interval(Duration::from_millis(interval));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                heartbeat = ticker;
                break;
            }
            Some(7) => return GatewayEnd::Reconnect(session),
            Some(9) => {
                // Invalid session: resumable=false means identify again.
                let resumable = value["d"].as_bool().unwrap_or(false);
                let keep = resumable.then_some(session.clone()).flatten();
                return GatewayEnd::Reconnect(keep);
            }
            _ => {}
        }
    }
    // Identify or resume.
    let payload = match &session {
        Some(s) if s.can_resume() => json!({
            "op": 6,
            "d": {"token": channel.cfg.bot_token, "session_id": s.session_id, "seq": s.seq}
        }),
        _ => json!({
            "op": 2,
            "d": {
                "token": channel.cfg.bot_token,
                "intents": super::INTENTS,
                "properties": {"os": "linux", "browser": "ka-agent", "device": "ka-agent"}
            }
        }),
    };
    if ws.send(Message::Text(payload.to_string().into())).await.is_err() {
        return GatewayEnd::Reconnect(session);
    }
    loop {
        // Cancel-safe: `Interval::tick` keeps its deadline when the select
        // drops it.
        let tick = heartbeat.tick();
        tokio::select! {
            _ = shutdown.changed() => {
                let _ = ws.close(None).await;
                return GatewayEnd::Shutdown;
            }
            _ = tick => {
                if awaiting_ack {
                    // A missing ACK means the connection is a zombie: drop it
                    // and resume from the last sequence.
                    return GatewayEnd::Reconnect(session);
                }
                awaiting_ack = true;
                let beat = json!({"op": 1, "d": seq});
                if ws.send(Message::Text(beat.to_string().into())).await.is_err() {
                    return GatewayEnd::Reconnect(session);
                }
            }
            frame = ws.next() => {
                let Some(Ok(msg)) = frame else {
                    return GatewayEnd::Reconnect(session);
                };
                let Message::Text(text) = msg else { continue };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                if let Some(s) = value["s"].as_i64() {
                    seq = Some(s);
                    if let Some(cur) = session.as_mut() {
                        cur.seq = seq;
                    }
                }
                match value["op"].as_i64() {
                    Some(11) => awaiting_ack = false,
                    Some(1) => {
                        let beat = json!({"op": 1, "d": seq});
                        let _ = ws.send(Message::Text(beat.to_string().into())).await;
                    }
                    Some(7) => return GatewayEnd::Reconnect(session),
                    Some(9) => {
                        let resumable = value["d"].as_bool().unwrap_or(false);
                        return GatewayEnd::Reconnect(resumable.then_some(session.clone()).flatten());
                    }
                    Some(0) => {
                        let kind = value["t"].as_str().unwrap_or("");
                        match kind {
                            "READY" => {
                                let d = &value["d"];
                                let id = d["session_id"].as_str().unwrap_or("").to_string();
                                let resume_url = format!(
                                    "{}/?v=10&encoding=json",
                                    d["resume_gateway_url"]
                                        .as_str()
                                        .unwrap_or("wss://gateway.discord.gg")
                                );
                                if let Some(user) = d["user"]["id"].as_str() {
                                    channel.set_bot_user(user);
                                }
                                session = Some(Session { session_id: id, resume_url, seq });
                            }
                            "RESUMED" => {
                                awaiting_ack = false;
                            }
                            "MESSAGE_CREATE" => {
                                channel.handle_message(&value["d"]).await;
                            }
                            "INTERACTION_CREATE" => {
                                channel.handle_interaction(&value["d"]).await;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

impl DiscordChannel {
    /// One `MESSAGE_CREATE`.
    pub(super) async fn handle_message(&self, d: &Value) {
        let guild = d["guild_id"].as_str().unwrap_or("");
        let author = d["author"]["id"].as_str().unwrap_or("");
        if author.is_empty() || d["author"]["bot"].as_bool() == Some(true) {
            return;
        }
        let channel_id = d["channel_id"].as_str().unwrap_or("");
        // A DM has no guild id and channel type 1.
        let is_dm = guild.is_empty();
        if !is_dm && !self.cfg.guilds.iter().any(|g| g == guild) {
            self.audit(
                "discord.unauthorized_guild",
                guild,
                Some("events ignored: guild not in discord.guilds".into()),
            );
            return;
        }
        let source = if is_dm {
            format!("discord:dm:{author}")
        } else {
            format!("discord:{guild}:{author}")
        };
        let Some(own) = self.principal_for(guild, author) else {
            self.audit(
                "discord.unauthorized",
                &source,
                Some("unknown sender: link the identity or enable discord.auto_provision".into()),
            );
            return;
        };
        let content = d["content"].as_str().unwrap_or("").to_string();
        let bot = self.bot_user();
        let mentions_bot = bot
            .as_ref()
            .is_some_and(|b| content.contains(&format!("<@{b}>")) || content.contains(&format!("<@!{b}>")));
        let (principal, text) = if is_dm {
            (own.clone(), content)
        } else {
            // A guild channel answers only on a mention, under the channel
            // account.
            if !mentions_bot {
                return;
            }
            let mut text = content;
            if let Some(b) = &bot {
                text = text
                    .replace(&format!("<@{b}>"), "")
                    .replace(&format!("<@!{b}>"), "");
            }
            (
                self.channel_principal(guild, channel_id),
                text.trim().to_string(),
            )
        };
        if text.trim().is_empty() {
            return;
        }
        let mut text = text;
        // Attachments land in the acting account's workspace.
        if let Some(files) = d["attachments"].as_array() {
            for file in files.iter().take(10) {
                if let (Some(url), Some(name)) =
                    (file["url"].as_str(), file["filename"].as_str())
                    && let Ok(bytes) = self.client.download(url).await
                        && bytes.len() <= 8 * 1024 * 1024
                    {
                        let dir = self.runtime.workspace_for(&principal).join("attachments");
                        let _ = std::fs::create_dir_all(&dir);
                        let safe: String = name
                            .chars()
                            .map(|c| {
                                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                                    c
                                } else {
                                    '_'
                                }
                            })
                            .collect();
                        if std::fs::write(dir.join(&safe), &bytes).is_ok() {
                            text.push_str(&format!("\n@attachments/{safe}"));
                        }
                    }
            }
        }
        self.remember_target(
            &source,
            ReplyTarget {
                channel: channel_id.to_string(),
                thread_name: None,
                shared: !is_dm,
                username: principal.username.clone(),
            },
        );
        let (text, refs) = crate::refs::expand(
            &self.store,
            &self.runtime.workspace_for(&principal),
            &principal,
            &text,
        )
        .await;
        crate::refs::stage_sources(&self.store, &source, &refs);
        if let Err(e) = self
            .runtime
            .handle_message_as(&text, &source, &principal)
            .await
        {
            eprintln!("discord: submit: {e}");
        }
    }

    /// One `INTERACTION_CREATE` (the approval buttons).
    pub(super) async fn handle_interaction(&self, d: &Value) {
        let custom = d["data"]["custom_id"].as_str().unwrap_or("");
        let (decision, approve) = if let Some(_id) = custom.strip_prefix("ka_approve:") {
            (ka_agent_core::ApprovalDecision::Allow, true)
        } else if let Some(_id) = custom.strip_prefix("ka_deny:") {
            (ka_agent_core::ApprovalDecision::Deny, false)
        } else {
            return;
        };
        let approval = custom.split_once(':').map(|(_, id)| id).unwrap_or("");
        let guild = d["guild_id"].as_str().unwrap_or("");
        let user = d["member"]["user"]["id"]
            .as_str()
            .or_else(|| d["user"]["id"].as_str())
            .unwrap_or("");
        let source = if guild.is_empty() {
            format!("discord:dm:{user}")
        } else {
            format!("discord:{guild}:{user}")
        };
        let id = d["id"].as_str().unwrap_or("");
        let token = d["token"].as_str().unwrap_or("");
        match self.runtime.decide(approval, decision, &source).await {
            Ok(()) => {
                let content = if approve { "✅ approved" } else { "❌ denied" };
                let _ = self
                    .client
                    .interaction_callback(
                        id,
                        token,
                        json!({
                            "type": 7,
                            "data": {"content": content, "components": []}
                        }),
                    )
                    .await;
            }
            Err(e) => {
                self.audit("approval.refused", approval, Some(format!("{source}: {e}")));
                let _ = self
                    .client
                    .interaction_callback(
                        id,
                        token,
                        json!({
                            "type": 4,
                            "data": {"content": format!("not applied: {e}"), "flags": 64}
                        }),
                    )
                    .await;
            }
        }
    }
}

