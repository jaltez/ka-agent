// Voice-note transcription for the Telegram channel. Split from telegram.rs (moves only).

use super::channel::TelegramChannel;
use super::client::TelegramClient;

use serde_json::Value;

impl TelegramChannel {
    /// A voice note (or audio file) → its transcript, echoed back so the
    /// operator sees what was heard. `None` when there is nothing to submit
    /// (voice off, too long, transcription failed — each answered).
    pub(super) async fn transcribe_voice(
        &self,
        client: &TelegramClient,
        chat_id: i64,
        voice: &Value,
    ) -> Option<String> {
        let Some(t) = &self.cfg.transcriber else {
            let _ = client
                .send_message(
                    chat_id,
                    "🎙 voice notes are off — `ka-agent setup voice` turns them on",
                    None,
                )
                .await;
            return None;
        };
        let duration = voice["duration"].as_i64().unwrap_or(0);
        let size = voice["file_size"].as_u64().unwrap_or(0) as usize;
        if duration > crate::voice::MAX_SECONDS || size > crate::voice::MAX_BYTES {
            let _ = client
                .send_message(chat_id, "🎙 too long — voice notes up to 10 minutes", None)
                .await;
            return None;
        }
        let file_id = voice["file_id"].as_str().unwrap_or("");
        let audio = match client.get_file(file_id).await {
            Ok(path) => client.download_file(&path).await,
            Err(e) => Err(e),
        };
        let heard = match audio {
            Ok(bytes) => t.transcribe(bytes, "voice.ogg").await,
            Err(e) => Err(format!("download: {e}")),
        };
        match heard {
            Ok(text) if !text.is_empty() => {
                let _ = client
                    .send_message(chat_id, &format!("🎙 “{text}”"), None)
                    .await;
                Some(text)
            }
            Ok(_) => {
                let _ = client
                    .send_message(chat_id, "🎙 (nothing heard)", None)
                    .await;
                None
            }
            Err(e) => {
                let _ = client
                    .send_message(chat_id, &format!("! [voice] {e}"), None)
                    .await;
                None
            }
        }
    }
}
