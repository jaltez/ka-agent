//! The Socket Mode session loop: ack every envelope, dispatch by type,
//! reopen on `disconnect`.
//!
//! Frames (verified against docs.slack.dev/apis/events-api/using-socket-mode):
//! `{type:"hello"}`, `{type:"events_api", envelope_id, payload:{team_id,
//! event}}`, `{type:"interactive", envelope_id, payload:{type:"block_actions",
//! team, user, channel, message, actions:[{action_id}]}}`,
//! `{type:"slash_commands", envelope_id, payload:{team_id, user_id, command,
//! text, channel_id}}` and `{type:"disconnect", reason}`. Every non-disconnect
//! envelope must be acked with `{"envelope_id": …}` or Slack retries it.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use ka_agent_core::Principal;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use super::{ReplyTarget, SlackChannel};

/// Why a session ended.
pub enum SessionEnd {
    /// The daemon is shutting down.
    Shutdown,
    /// Slack asked to refresh the connection (`disconnect`).
    Disconnected,
    /// Transport or protocol failure (reconnect with backoff).
    Failed(String),
}

pub(super) async fn run_session(
    url: &str,
    channel: &Arc<SlackChannel>,
    shutdown: &mut watch::Receiver<bool>,
) -> SessionEnd {
    let (mut ws, _) = match tokio_tungstenite::connect_async(url).await {
        Ok(pair) => pair,
        Err(e) => return SessionEnd::Failed(format!("connect: {e}")),
    };
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                let _ = ws.close(None).await;
                return SessionEnd::Shutdown;
            }
            frame = ws.next() => {
                let frame = match frame {
                    Some(Ok(f)) => f,
                    Some(Err(e)) => return SessionEnd::Failed(format!("socket: {e}")),
                    None => return SessionEnd::Failed("socket closed".into()),
                };
                match frame {
                    Message::Text(text) => {
                        let Ok(value) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        // Ack FIRST: Slack retries an unacked envelope, and a
                        // slow handler must not cause a duplicate event.
                        if let Some(envelope) = value["envelope_id"].as_str() {
                            let ack = json!({"envelope_id": envelope}).to_string();
                            if ws.send(Message::Text(ack.into())).await.is_err() {
                                return SessionEnd::Failed("ack failed".into());
                            }
                        }
                        match value["type"].as_str().unwrap_or("") {
                            "hello" => {}
                            "disconnect" => return SessionEnd::Disconnected,
                            "events_api" => channel.handle_event(&value["payload"]).await,
                            "interactive" => channel.handle_interactive(&value["payload"]).await,
                            "slash_commands" => channel.handle_slash(&value["payload"]).await,
                            _ => {}
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
                    Message::Close(_) => return SessionEnd::Disconnected,
                    Message::Frame(_) => {}
                }
            }
        }
    }
}

impl SlackChannel {
    /// One `events_api` payload.
    pub(super) async fn handle_event(&self, payload: &Value) {
        let team = payload["team_id"].as_str().unwrap_or("");
        let event = &payload["event"];
        let kind = event["type"].as_str().unwrap_or("");
        if !self.cfg.teams.iter().any(|t| t == team) {
            self.audit(
                "slack.unauthorized_workspace",
                team,
                Some("events ignored: workspace not in slack.teams".into()),
            );
            return;
        }
        match kind {
            "app_uninstalled" | "tokens_revoked" => {
                self.audit(kind, team, Some("the app was removed; data kept".into()));
                self.runtime.emit(ka_agent_runtime::AgentEvent::Notice {
                    text: format!(
                        "Slack: the app was removed from workspace {team} — the adapter stops answering; data is kept"
                    ),
                });
                return;
            }
            "message" | "app_mention" => {}
            _ => return,
        }
        // Bot echoes and edits are never turns (the bot would talk to
        // itself).
        if event["bot_id"].is_string() || event["subtype"].is_string() {
            return;
        }
        let user = event["user"].as_str().unwrap_or("");
        if user.is_empty() {
            return;
        }
        let channel_id = event["channel"].as_str().unwrap_or("");
        let is_dm = event["channel_type"].as_str() == Some("im");
        let source = format!("slack:{team}:{user}");
        let Some(own) = self.principal_for(team, user).await else {
            self.audit(
                "slack.unauthorized",
                &source,
                Some(if self.cfg.auto_provision {
                    "unknown sender (auto_provision failed)".into()
                } else {
                    "unknown sender: link the identity or enable slack.auto_provision".into()
                }),
            );
            return;
        };
        // A shared channel answers only on a direct mention, and the turn
        // runs under the channel role: no personal memory or grants leak
        // into it. DMs are the user's own private session.
        let (principal, raw_text) = if is_dm {
            (own.clone(), event["text"].as_str().unwrap_or("").to_string())
        } else {
            if kind != "app_mention" {
                return;
            }
            let mention = self.mention_token().await;
            let text = event["text"].as_str().unwrap_or("").to_string();
            let text = text.replace(&mention, "").trim().to_string();
            (self.channel_principal(team, channel_id).await, text)
        };
        if raw_text.trim().is_empty() {
            return;
        }
        let thread = if is_dm {
            None
        } else {
            event["thread_ts"]
                .as_str()
                .or_else(|| event["ts"].as_str())
                .map(str::to_string)
        };
        self.remember_target(
            &format!("slack:{team}:{user}"),
            ReplyTarget {
                channel: channel_id.to_string(),
                thread_ts: thread.clone(),
                shared: !is_dm,
                username: principal.username.clone(),
            },
        );
        // Files ride along as attachment references, like Telegram's.
        let mut text = raw_text;
        if let Some(files) = event["files"].as_array() {
            for file in files.iter().take(10) {
                if let Some(path) = self.save_file(file, &own).await {
                    text.push_str(&format!("\n@{path}"));
                }
            }
        }
        let (text, refs) = crate::refs::expand(
            &self.store,
            &self.runtime.workspace_for(&principal),
            &principal,
            &text,
        )
        .await;
        crate::refs::stage_sources(&self.store, &source, &refs);
        if let Err(e) = self.runtime.handle_message_as(&text, &source, &principal).await {
            eprintln!("slack: submit: {e}");
        }
    }

    /// One `interactive` payload: the Block Kit approval buttons.
    pub(super) async fn handle_interactive(&self, payload: &Value) {
        if payload["type"].as_str() != Some("block_actions") {
            return;
        }
        let team = payload["team"]["id"].as_str().unwrap_or("");
        let user = payload["user"]["id"].as_str().unwrap_or("");
        let channel = payload["channel"]["id"].as_str().unwrap_or("");
        let message_ts = payload["message"]["ts"].as_str().unwrap_or("");
        let Some(action) = payload["actions"]
            .as_array()
            .and_then(|a| a.first())
        else {
            return;
        };
        let action_id = action["action_id"].as_str().unwrap_or("");
        let (decision, is_approve) = if action_id.starts_with("ka_approve:") {
            (ka_agent_core::ApprovalDecision::Allow, true)
        } else if action_id.starts_with("ka_deny:") {
            (ka_agent_core::ApprovalDecision::Deny, false)
        } else {
            return;
        };
        let approval = action_id
            .split_once(':')
            .map(|(_, id)| id.to_string())
            .unwrap_or_default();
        let source = format!("slack:{team}:{user}");
        match self.runtime.decide(&approval, decision, &source).await {
            Ok(()) => {
                let label = if is_approve { "✅ approved" } else { "❌ denied" };
                let blocks = json!([{
                    "type": "section",
                    "text": {"type": "mrkdwn", "text": label}
                }]);
                let _ = self
                    .client
                    .update_message(channel, message_ts, label, blocks)
                    .await;
            }
            Err(e) => {
                // Only the clicker learns why (an unauthorized click must
                // not settle or expose the card to the channel).
                let _ = self
                    .client
                    .post_ephemeral(channel, user, &format!("not applied: {e}"))
                    .await;
                self.audit(
                    "approval.refused",
                    &approval,
                    Some(format!("{source}: {e}")),
                );
            }
        }
    }

    /// One `slash_commands` payload: the same command router as every other
    /// surface, answered in-channel.
    pub(super) async fn handle_slash(&self, payload: &Value) {
        let team = payload["team_id"].as_str().unwrap_or("");
        let user = payload["user_id"].as_str().unwrap_or("");
        let channel = payload["channel_id"].as_str().unwrap_or("");
        let command = payload["command"].as_str().unwrap_or("");
        let text = payload["text"].as_str().unwrap_or("");
        let source = format!("slack:{team}:{user}");
        let Some(_) = self.principal_for(team, user).await else {
            self.audit("slack.unauthorized", &source, Some("slash command".into()));
            return;
        };
        let line = format!("{command} {text}").trim().to_string();
        match crate::commands::route(&self.runtime, &source, &line).await {
            crate::commands::Outcome::Reply(reply) => {
                let _ = self.client.post_message(channel, &reply, None).await;
            }
            crate::commands::Outcome::Silent => {}
            crate::commands::Outcome::NotACommand => {}
        }
    }

    /// Save one Slack file into the user's attachments directory and return
    /// the `attachments/…` reference.
    async fn save_file(&self, file: &Value, who: &Principal) -> Option<String> {
        let id = file["id"].as_str()?;
        let name = file["name"].as_str().unwrap_or("file");
        // `files.info` carries the private download URL and the size guard.
        let info = self.client.file_info(id).await.ok()?;
        let entry = &info["file"];
        let size = entry["size"].as_u64().unwrap_or(0);
        // 8 MiB, the same cap the web upload path enforces.
        if size > 8 * 1024 * 1024 {
            return None;
        }
        let url = entry["url_private_download"]
            .as_str()
            .or_else(|| entry["url_private"].as_str())?;
        let bytes = self.client.download(url).await.ok()?;
        let dir = self.runtime.workspace_for(who).join("attachments");
        std::fs::create_dir_all(&dir).ok()?;
        let safe = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
            .collect::<String>();
        std::fs::write(dir.join(&safe), &bytes).ok()?;
        Some(format!("attachments/{safe}"))
    }

    fn audit(&self, action: &str, subject: &str, detail: Option<String>) {
        let mut rec = ka_agent_core::AuditRecord::new("slack", action, subject, detail);
        let _ = self.store.append_audit(&mut rec);
    }
}
