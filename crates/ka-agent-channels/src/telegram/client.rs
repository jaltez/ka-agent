// TelegramClient: the Bot API HTTP surface. Split from telegram.rs (moves only).

use super::*;

use serde_json::{Value, json};

/// Telegram's caption limit for `sendPhoto`/`sendDocument`, in characters
/// (the limit counts characters, not bytes).
const MAX_CAPTION_CHARS: usize = 1024;

/// Truncate a caption to [`MAX_CAPTION_CHARS`]: an over-long one keeps
/// 1023 characters plus `…`, so the field still fits the limit.
fn capped_caption(caption: &str) -> String {
    if caption.chars().count() <= MAX_CAPTION_CHARS {
        return caption.to_string();
    }
    let mut out: String = caption.chars().take(MAX_CAPTION_CHARS - 1).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Bot API client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct TelegramClient {
    http: reqwest::Client,
    base: String,
    download_base: String,
}

impl TelegramClient {
    pub(crate) fn new(api_base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: format!("{api_base}/bot{token}"),
            download_base: format!("{api_base}/file/bot{token}"),
        }
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value, ChannelError> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.base))
            .timeout(POLL_HTTP_TIMEOUT)
            .json(&body)
            .send()
            .await?;
        let v: Value = resp.json().await?;
        if v["ok"].as_bool() == Some(true) {
            Ok(v["result"].clone())
        } else {
            Err(ChannelError::from_api_answer(&v))
        }
    }

    pub(crate) async fn get_me(&self) -> Result<Value, ChannelError> {
        self.call("getMe", json!({})).await
    }

    pub(crate) async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> Result<Vec<Value>, ChannelError> {
        let result = self
            .call(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": timeout_secs,
                    "allowed_updates": ["message", "callback_query"],
                }),
            )
            .await?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    pub(crate) async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_markup: Option<Value>,
    ) -> Result<i64, ChannelError> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if let Some(markup) = reply_markup {
            body["reply_markup"] = markup;
        }
        let result = self.call("sendMessage", body).await?;
        Ok(result["message_id"].as_i64().unwrap_or(0))
    }

    /// `sendMessage` with `parse_mode: HTML`; the caller supplies markup
    /// built by `ka_agent_core::markdown` (input already escaped).
    pub(crate) async fn send_message_html(
        &self,
        chat_id: i64,
        html: &str,
        reply_markup: Option<Value>,
    ) -> Result<i64, ChannelError> {
        let mut body = json!({ "chat_id": chat_id, "text": html, "parse_mode": "HTML" });
        if let Some(markup) = reply_markup {
            body["reply_markup"] = markup;
        }
        let result = self.call("sendMessage", body).await?;
        Ok(result["message_id"].as_i64().unwrap_or(0))
    }

    /// Bot API 9.5 `sendMessageDraft`: show `text` as the reply being
    /// written (private chats). The same `draft_id` updates in place.
    pub(crate) async fn send_message_draft(
        &self,
        chat_id: i64,
        draft_id: i64,
        text: &str,
    ) -> Result<(), ChannelError> {
        self.call(
            "sendMessageDraft",
            json!({ "chat_id": chat_id, "draft_id": draft_id, "text": text }),
        )
        .await
        .map(|_| ())
    }

    /// Upload one file as `sendPhoto`/`sendDocument` (`field` is `photo`
    /// or `document`). `mime` lets Telegram accept a photo without
    /// sniffing; the caption is omitted when empty and truncated to
    /// Telegram's 1024-character cap (with `…`) when longer. Uploads get
    /// [`MEDIA_HTTP_TIMEOUT`] instead of the polling timeout: a 50 MB file
    /// does not fit 40 s.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_media(
        &self,
        chat_id: i64,
        method: &str,
        field: &str,
        file_name: &str,
        mime: Option<&str>,
        bytes: Vec<u8>,
        caption: &str,
    ) -> Result<i64, ChannelError> {
        let mut form = reqwest::multipart::Form::new().text("chat_id", chat_id.to_string());
        let caption = capped_caption(caption);
        if !caption.is_empty() {
            form = form.text("caption", caption);
        }
        let mut part = reqwest::multipart::Part::bytes(bytes).file_name(file_name.to_string());
        if let Some(mime) = mime {
            part = part.mime_str(mime)?;
        }
        let resp = self
            .http
            .post(format!("{}/{method}", self.base))
            .timeout(MEDIA_HTTP_TIMEOUT)
            .multipart(form.part(field.to_string(), part))
            .send()
            .await?;
        let v: Value = resp.json().await?;
        if v["ok"].as_bool() == Some(true) {
            Ok(v["result"]["message_id"].as_i64().unwrap_or(0))
        } else {
            Err(ChannelError::from_api_answer(&v))
        }
    }

    /// React to a message with one emoji (the receipt acknowledgement).
    pub(crate) async fn set_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: &str,
    ) -> Result<(), ChannelError> {
        self.call(
            "setMessageReaction",
            json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "reaction": [{ "type": "emoji", "emoji": emoji }],
            }),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn delete_message(
        &self,
        chat_id: i64,
        message_id: i64,
    ) -> Result<(), ChannelError> {
        self.call(
            "deleteMessage",
            json!({ "chat_id": chat_id, "message_id": message_id }),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        reply_markup: Option<Value>,
    ) -> Result<(), ChannelError> {
        let mut body = json!({ "chat_id": chat_id, "message_id": message_id, "text": text });
        if let Some(markup) = reply_markup {
            body["reply_markup"] = markup;
        }
        self.call("editMessageText", body).await.map(|_| ())
    }

    pub(crate) async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: &str,
    ) -> Result<(), ChannelError> {
        self.call(
            "answerCallbackQuery",
            json!({ "callback_query_id": callback_query_id, "text": text }),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn send_chat_action(&self, chat_id: i64) -> Result<(), ChannelError> {
        self.call(
            "sendChatAction",
            json!({ "chat_id": chat_id, "action": "typing" }),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn set_my_commands(&self) -> Result<Value, ChannelError> {
        // Built from the shared command table so the menu can never drift
        // from what `/help` says (fixes /skills and /skill missing here).
        let commands: Vec<serde_json::Value> = crate::commands::COMMANDS
            .iter()
            .filter(|c| c.telegram)
            .map(|c| {
                json!({
                    "command": c.name,
                    "description": c.summary.chars().take(256).collect::<String>(),
                })
            })
            .collect();
        self.call(
            "setMyCommands",
            json!({
                "commands": commands
            }),
        )
        .await
    }
    /// `getFile`: resolve a file_id to the API-relative `file_path`.
    pub(crate) async fn get_file(&self, file_id: &str) -> Result<String, ChannelError> {
        let result = self.call("getFile", json!({ "file_id": file_id })).await?;
        result["file_path"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| ChannelError::api("getFile: no file_path"))
    }

    /// Download one file by its API-relative path. Bot API downloads cap
    /// at 20 MB; anything bigger is refused before it is written anywhere.
    pub(crate) async fn download_file(&self, file_path: &str) -> Result<Vec<u8>, ChannelError> {
        const MAX: usize = 20 * 1024 * 1024;
        let resp = self
            .http
            .get(format!("{}/{file_path}", self.download_base))
            .timeout(POLL_HTTP_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(ChannelError::api("download failed"));
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > MAX {
            return Err(ChannelError::api("file exceeds the 20 MB download cap"));
        }
        Ok(bytes.to_vec())
    }
}

/// Validate a bot token with `getMe`; returns the bot's username.
pub async fn telegram_get_me(api_base: &str, token: &str) -> Result<String, String> {
    let me = TelegramClient::new(api_base, token)
        .get_me()
        .await
        .map_err(|e| e.to_string())?;
    me["username"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "telegram getMe: no username in the answer".to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{MAX_CAPTION_CHARS, capped_caption};

    #[test]
    fn captions_over_the_limit_are_truncated_with_an_ellipsis() {
        assert_eq!(capped_caption(""), "");
        assert_eq!(capped_caption("a caption"), "a caption");
        let exact = "x".repeat(MAX_CAPTION_CHARS);
        assert_eq!(capped_caption(&exact), exact);
        let long = "y".repeat(MAX_CAPTION_CHARS + 500);
        let capped = capped_caption(&long);
        assert_eq!(capped.chars().count(), MAX_CAPTION_CHARS);
        assert!(capped.ends_with('…'));
        // characters, not bytes: a multi-byte caption is never split mid-glyph
        let wide = "🙂".repeat(MAX_CAPTION_CHARS + 10);
        let capped = capped_caption(&wide);
        assert_eq!(capped.chars().count(), MAX_CAPTION_CHARS);
        assert!(capped.ends_with("🙂…"), "whole last glyph kept");
    }
}
