// MatrixChannel: the /sync loop, gate, dispatch and the sender task. Split from matrix.rs (moves only).

use super::surface::{MatrixSurface, sender_loop};
use super::*;
use crate::channel::Channel;
use crate::commands::{self, Outcome};
use ka_agent_core::AuditRecord;
use ka_agent_runtime::Runtime;
use ka_agent_store::Store;
use serde_json::Value;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

pub struct MatrixChannel {
    cfg: MatrixConfig,
    runtime: Runtime,
    store: Arc<Store>,
    surface: Arc<MatrixSurface>,
    encrypted_noticed: Mutex<HashSet<String>>,
}

impl MatrixChannel {
    pub fn new(
        cfg: MatrixConfig,
        runtime: Runtime,
        store: Arc<Store>,
        surface: Arc<MatrixSurface>,
    ) -> Self {
        Self {
            cfg,
            runtime,
            store,
            surface,
            encrypted_noticed: Mutex::new(HashSet::new()),
        }
    }

    /// Point delivery at `room`, persisting it so proactive notices reach
    /// the operator after a restart.
    fn set_room(&self, room: &str) {
        if self.surface.set_room(room)
            && let Err(e) = self.store.meta_set(ROOM_KEY, room)
        {
            eprintln!("matrix: persist room: {e}");
        }
    }

    fn audit_unauthorized(&self, sender: &str, detail: String) {
        let mut record = AuditRecord::new(
            &format!("matrix:{sender}"),
            "matrix.unauthorized",
            "",
            Some(detail),
        );
        let _ = self.store.append_audit(&mut record);
    }

    fn is_linked(&self, sender: &str) -> bool {
        self.store.is_linked("matrix", sender).unwrap_or(false)
    }

    /// One pending invite: join when a linked user invited us.
    async fn handle_invite(&self, room: &str, state: &Value) {
        let events = state["invite_state"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let Some(sender) = events.iter().find_map(|e| {
            (e["type"].as_str() == Some("m.room.member")
                && e["state_key"].as_str() == Some(self.cfg.user_id.as_str())
                && e["content"]["membership"].as_str() == Some("invite"))
            .then(|| e["sender"].as_str().unwrap_or("").to_string())
        }) else {
            return;
        };
        if sender.is_empty() || !self.is_linked(&sender) {
            eprintln!("matrix: ignoring invite to {room} from {sender}");
            self.audit_unauthorized(&sender, format!("invite room={room}"));
            return;
        }
        match self.surface.client.join(room).await {
            Ok(()) => self.set_room(room),
            Err(e) => eprintln!("matrix: join {room}: {e}"),
        }
    }

    /// One joined room's timeline event.
    async fn handle_event(&self, room: &str, event: &Value) {
        let kind = event["type"].as_str().unwrap_or("");
        let sender = event["sender"].as_str().unwrap_or("");
        if sender == self.cfg.user_id || sender.is_empty() {
            return;
        }
        if kind == "m.room.encrypted" {
            if self.is_linked(sender) {
                let first = self
                    .encrypted_noticed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(room.to_string());
                if first {
                    self.surface.client.send_split(room, ENCRYPTED_NOTICE).await;
                }
            }
            return;
        }
        if !matches!(kind, "m.room.message" | "m.reaction") {
            return;
        }
        let body = event["content"]["body"].as_str().unwrap_or("").trim();
        // §8.1: linked identities only; everyone else is audited, ignored,
        // never answered — except a pairing code (§5.2), silent unless it
        // links.
        if !self.is_linked(sender) {
            if kind == "m.room.message"
                && body
                    .get(..5)
                    .is_some_and(|p| p.eq_ignore_ascii_case("/pair"))
            {
                if crate::pairing::redeem(&self.store, "matrix", sender, body)
                    == crate::pairing::Redeem::Linked
                {
                    self.set_room(room);
                    self.surface.client.send_split(room, PAIRED_REPLY).await;
                }
                return;
            }
            eprintln!("matrix: unauthorized sender {sender}");
            self.audit_unauthorized(sender, format!("room={room}"));
            return;
        }
        self.set_room(room);
        // A reaction is an approval decision on a card we posted.
        if kind == "m.reaction" {
            self.handle_reaction(room, event).await;
            return;
        }
        // 👀 receipt: received and accepted, before any engine start-up or
        // queueing makes the room look dead.
        if let Some(event_id) = event["event_id"].as_str() {
            let client = Arc::clone(&self.surface.client);
            let room = room.to_string();
            let event_id = event_id.to_string();
            tokio::spawn(async move {
                let _ = client.react(&room, &event_id, "👀").await;
            });
        }
        // Files land in the workspace attachments inbox, like Telegram's.
        let msgtype = event["content"]["msgtype"].as_str().unwrap_or("");
        if matches!(msgtype, "m.image" | "m.file" | "m.video") {
            let reply = match self.save_media(&event["content"]).await {
                Ok(rel) => format!("saved {rel} — reference it with @{rel} in your next message"),
                Err(e) => format!("! [attachment] {e}"),
            };
            self.surface.client.send_split(room, &reply).await;
            return;
        }
        let transcript;
        let body = if msgtype == "m.audio" {
            match self.transcribe_audio(room, &event["content"]).await {
                Some(t) => {
                    transcript = t;
                    transcript.as_str()
                }
                None => return,
            }
        } else {
            if body.is_empty() {
                return;
            }
            body
        };
        match commands::route(&self.runtime, &format!("matrix:{sender}"), body).await {
            Outcome::Reply(reply) => self.surface.client.send_split(room, &reply).await,
            Outcome::Silent => {}
            Outcome::NotACommand => {
                let body = crate::refs::expand(&self.store, &self.runtime.workspace(), body).await;
                if let Err(e) = self
                    .runtime
                    .handle_message(&body, &format!("matrix:{sender}"))
                    .await
                {
                    eprintln!("matrix: handle_message: {e}");
                }
            }
        }
    }

    /// ✅/❌ on an approval card decides it through the same runtime call
    /// `/y`/`/n` use. A card is single-use: its mapping is dropped before
    /// the decision so a duplicate reaction decides nothing.
    async fn handle_reaction(&self, room: &str, event: &Value) {
        let rel = &event["content"]["m.relates_to"];
        if rel["rel_type"].as_str() != Some("m.annotation") {
            return;
        }
        let allow = match rel["key"].as_str().unwrap_or("") {
            "✅" => true,
            "❌" => false,
            _ => return,
        };
        let target = rel["event_id"].as_str().unwrap_or("");
        let Some(id) = self.surface.approval_for_card(target) else {
            return;
        };
        self.surface.forget_card(target);
        let decision = if allow {
            ka_agent_core::ApprovalDecision::Allow
        } else {
            ka_agent_core::ApprovalDecision::Deny
        };
        let reply = match self.runtime.decide(&id, decision).await {
            Ok(()) => "decided".to_string(),
            Err(e) => format!("! [approval] {e}"),
        };
        self.surface.client.send_split(room, &reply).await;
    }

    /// An `m.image`/`m.file`/`m.video` message → a workspace attachment.
    async fn save_media(&self, content: &Value) -> Result<String, String> {
        let mxc = content["url"]
            .as_str()
            .filter(|u| !u.is_empty())
            .or_else(|| content["file"]["url"].as_str())
            .ok_or_else(|| "message has no media url".to_string())?;
        let name = attachment_name(content, mxc);
        let bytes = self.surface.client.download_media(mxc).await?;
        crate::attachments::save_attachment_bytes(&self.runtime.workspace(), &name, &bytes)
    }

    /// An `m.audio` message → its transcript (echoed back); `None` when
    /// nothing should be submitted (voice off, too long, failure — each
    /// answered in the room).
    async fn transcribe_audio(&self, room: &str, content: &Value) -> Option<String> {
        let client = &self.surface.client;
        let Some(t) = &self.cfg.transcriber else {
            client
                .send_split(
                    room,
                    "🎙 voice messages are off — `ka-agent setup voice` turns them on",
                )
                .await;
            return None;
        };
        let ms = content["info"]["duration"].as_i64().unwrap_or(0);
        if ms / 1000 > crate::voice::MAX_SECONDS {
            client
                .send_split(room, "🎙 too long — voice messages up to 10 minutes")
                .await;
            return None;
        }
        let heard = match client
            .download_media(content["url"].as_str().unwrap_or(""))
            .await
        {
            Ok(bytes) => t.transcribe(bytes, "voice.ogg").await,
            Err(e) => Err(e),
        };
        match heard {
            Ok(text) if !text.is_empty() => {
                client.send_split(room, &format!("🎙 “{text}”")).await;
                Some(text)
            }
            Ok(_) => {
                client.send_split(room, "🎙 (nothing heard)").await;
                None
            }
            Err(e) => {
                client.send_split(room, &format!("! [voice] {e}")).await;
                None
            }
        }
    }

    async fn handle_sync(&self, value: &Value, first: bool, boot_ms: i64) {
        if let Some(invites) = value["rooms"]["invite"].as_object() {
            for (room, state) in invites {
                self.handle_invite(room, state).await;
            }
        }
        let Some(rooms) = value["rooms"]["join"].as_object() else {
            return;
        };
        for (room, state) in rooms {
            let Some(events) = state["timeline"]["events"].as_array() else {
                continue;
            };
            for event in events {
                // First-ever sync: the timeline is history, not new mail.
                if first
                    && event["origin_server_ts"]
                        .as_i64()
                        .is_some_and(|ts| ts < boot_ms - FIRST_SYNC_GRACE_MS)
                {
                    continue;
                }
                self.handle_event(room, event).await;
            }
        }
    }
}

impl Channel for MatrixChannel {
    fn name(&self) -> &'static str {
        "matrix"
    }

    fn run(self, shutdown: watch::Receiver<bool>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            let boot_ms = ka_agent_core::now_ms();
            let mut since = self
                .store
                .meta_get(NEXT_BATCH_KEY)
                .ok()
                .flatten()
                .filter(|s| !s.is_empty());
            let mut shutdown = shutdown;
            let Some(outbound) = self.surface.take_receiver() else {
                eprintln!("matrix: outbound queue already taken; not starting");
                return;
            };
            // Crash recovery (SEED §3.7): undelivered chunks go out before
            // any new traffic, once a room is known (until then they wait in
            // the sender's backlog, not dropped).
            let recover = self
                .store
                .pending_chunks_for_session(&self.runtime.session_id())
                .unwrap_or_default();
            for chunk in &recover {
                self.surface.enqueue(&chunk.text);
                self.store
                    .mark_sent(&chunk.turn, chunk.chunk_index, chunk.chunk_index as i64)
                    .ok();
            }
            if !recover.is_empty() {
                eprintln!("matrix: recovered {} queued chunk(s)", recover.len());
            }
            let sender = tokio::spawn(sender_loop(
                Arc::clone(&self.surface.client),
                Arc::clone(&self.surface.room),
                Arc::clone(&self.surface.room_known),
                Arc::clone(&self.surface.cards),
                outbound,
                shutdown.clone(),
            ));
            eprintln!(
                "matrix: syncing as {} (since {:?})",
                self.cfg.user_id, since
            );
            loop {
                let sync = tokio::select! {
                    _ = shutdown.changed() => break,
                    r = self.surface.client.sync(since.as_deref()) => r,
                };
                match sync {
                    Ok(value) => {
                        let first = since.is_none();
                        if let Some(next) = value["next_batch"].as_str() {
                            since = Some(next.to_string());
                        }
                        self.handle_sync(&value, first, boot_ms).await;
                        // Persist only after dispatch (at-least-once).
                        if let Some(next) = &since
                            && let Err(e) = self.store.meta_set(NEXT_BATCH_KEY, next)
                        {
                            eprintln!("matrix: persist next_batch: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("matrix: sync error: {e}; backing off");
                        tokio::select! {
                            _ = shutdown.changed() => break,
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        }
                    }
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), sender).await;
            eprintln!("matrix: channel stopped");
        })
    }
}

/// The filename for inbound Matrix media: the operator's `body` when it is
/// usable, else `{kind}-{id tail}.{ext}`. Either way the result is
/// sanitized — an mxc id tail can carry `/` or `..`, and the name decides
/// where the bytes land.
fn attachment_name(content: &Value, mxc: &str) -> String {
    let mime = content["info"]["mimetype"].as_str().unwrap_or("");
    crate::attachments::sanitize_attachment_name(content["body"].as_str().unwrap_or(""))
        .or_else(|| {
            let kind = content["msgtype"]
                .as_str()
                .unwrap_or("m.file")
                .trim_start_matches("m.");
            let minted = format!(
                "{kind}-{}.{}",
                ka_agent_core::ids::short_id(mxc),
                media_ext(mime)
            );
            crate::attachments::sanitize_attachment_name(&minted)
        })
        .unwrap_or_else(|| format!("file.{}", media_ext(mime)))
}

/// A filename extension for an inbound media type, for unnamed files.
fn media_ext(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "application/pdf" => "pdf",
        "application/json" => "json",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "application/zip" => "zip",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::attachment_name;
    use serde_json::json;

    #[test]
    fn the_operator_name_wins_when_usable() {
        let content = json!({"msgtype": "m.file", "body": "report.txt"});
        assert_eq!(
            attachment_name(&content, "mxc://example.org/abc"),
            "report.txt"
        );
    }

    #[test]
    fn a_minted_fallback_name_is_sanitized() {
        // The tail of this id is `/../../x`: unsanitized it is a path.
        let content = json!({"msgtype": "m.file", "body": ""});
        let name = attachment_name(&content, "mxc://example.org/../../x");
        assert!(name.starts_with("file-"), "{name}");
        assert!(!name.contains('/') && !name.contains(".."), "{name}");
        assert_eq!(name, "file-_._._x.bin");
    }

    #[test]
    fn an_unusable_operator_name_falls_back() {
        let content =
            json!({"msgtype": "m.image", "body": "..", "info": {"mimetype": "image/png"}});
        let name = attachment_name(&content, "mxc://example.org/abc");
        assert!(
            name.starts_with("image-") && name.ends_with(".png"),
            "{name}"
        );
    }
}
