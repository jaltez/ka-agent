//! Voice-note transcription for the phone channels: one OpenAI-compatible
//! `POST {base}/audio/transcriptions` (OpenAI, Groq, a local Whisper
//! server). The daemon resolves the key from the engine `.env` tier.

use std::time::Duration;

/// Longest voice note transcribed (Telegram notes are rarely longer).
pub const MAX_SECONDS: i64 = 600;
/// Largest file sent to the transcription API.
pub const MAX_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Transcriber {
    base: String,
    model: String,
    key: Option<String>,
    http: reqwest::Client,
}

impl Transcriber {
    pub fn new(base: String, model: String, key: Option<String>) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            model,
            key,
            http: reqwest::Client::new(),
        }
    }

    /// From `[voice]` and the engine `.env` map; `Err` names the missing key.
    pub fn from_config(
        voice: &ka_agent_core::Voice,
        env: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let key = match voice.key_env() {
            Some(name) => Some(
                env.get(name)
                    .cloned()
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| format!("voice: {name} is not set — ka-agent key set {name}"))?,
            ),
            None => None,
        };
        Ok(Self::new(voice.base(), voice.model(), key))
    }

    /// Transcribe one audio file; the text, trimmed.
    pub async fn transcribe(&self, audio: Vec<u8>, filename: &str) -> Result<String, String> {
        let part = reqwest::multipart::Part::bytes(audio).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new()
            .text("model", self.model.clone())
            .text("response_format", "json")
            .part("file", part);
        let mut req = self
            .http
            .post(format!("{}/audio/transcriptions", self.base))
            .timeout(Duration::from_secs(120))
            .multipart(form);
        if let Some(key) = &self.key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("transcription: {e}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("transcription: bad answer: {e}"))?;
        if !status.is_success() {
            let msg = body["error"]["message"]
                .as_str()
                .unwrap_or("request failed");
            return Err(format!("transcription: {status}: {msg}"));
        }
        Ok(body["text"].as_str().unwrap_or("").trim().to_string())
    }
}
