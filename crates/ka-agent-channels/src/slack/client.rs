//! Slack Web API client: the handful of methods the connector needs, with
//! the shared rate limiter and `ok:false` handling.
//!
//! Every call is JSON-in/JSON-out over `Authorization: Bearer <bot token>`;
//! errors carry Slack's own `error` string so the caller can name the
//! remedy (a missing scope, a revoked token).

use std::sync::Arc;

use serde_json::{Value, json};

use crate::ratelimit::RateLimiter;

pub struct SlackClient {
    http: reqwest::Client,
    api_base: String,
    token: String,
    limiter: Arc<RateLimiter>,
}

impl SlackClient {
    pub fn new(api_base: &str, token: String, limiter: Arc<RateLimiter>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_base: api_base.trim_end_matches('/').to_string(),
            token,
            limiter,
        }
    }

    /// One API call. `Ok(value)` when Slack answered `ok:true`; the error
    /// string is Slack's `error` code (or the transport failure).
    pub async fn call(&self, method: &str, body: Value) -> Result<Value, String> {
        let key = format!("web:{method}");
        self.limiter.wait(&key).await;
        let url = format!("{}/api/{method}", self.api_base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("{method}: {e}"))?;
        let status = resp.status();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_ascii_lowercase(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let header_refs: Vec<(&str, String)> =
            headers.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        if status.as_u16() == 429 {
            self.limiter
                .apply_headers(&key, &header_refs, ka_agent_core::now_ms());
            return Err(format!("{method}: rate limited"));
        }
        // Successful calls also carry the pacing headers.
        self.limiter
            .apply_headers(&key, &header_refs, ka_agent_core::now_ms());
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("{method}: bad json: {e}"))?;
        if value["ok"].as_bool() == Some(true) {
            return Ok(value);
        }
        let err = value["error"]
            .as_str()
            .unwrap_or("unknown_error")
            .to_string();
        Err(format!("{method}: {err}"))
    }

    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<Value, String> {
        let mut body = json!({"channel": channel, "text": text});
        if let Some(thread) = thread_ts {
            body["thread_ts"] = json!(thread);
        }
        self.call("chat.postMessage", body).await
    }

    /// `chat.postMessage` with Block Kit blocks (the approval card).
    pub async fn post_blocks(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
        blocks: Value,
    ) -> Result<Value, String> {
        let mut body = json!({"channel": channel, "text": text, "blocks": blocks});
        if let Some(thread) = thread_ts {
            body["thread_ts"] = json!(thread);
        }
        self.call("chat.postMessage", body).await
    }

    /// `chat.postEphemeral`: answer only the person who clicked.
    pub async fn post_ephemeral(&self, channel: &str, user: &str, text: &str) -> Result<Value, String> {
        self.call(
            "chat.postEphemeral",
            json!({"channel": channel, "user": user, "text": text}),
        )
        .await
    }

    /// `chat.update`: replace a message's text and blocks (the approval card
    /// settling to "approved"/"denied"/"expired").
    pub async fn update_message(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: Value,
    ) -> Result<Value, String> {
        self.call(
            "chat.update",
            json!({"channel": channel, "ts": ts, "text": text, "blocks": blocks}),
        )
        .await
    }

    /// `apps.connections.open`: the WSS URL of a Socket Mode session. The
    /// URL is short-lived; the caller reopens on `disconnect`.
    pub async fn connections_open(&self, app_token: &str) -> Result<String, String> {
        let url = format!("{}/api/apps.connections.open", self.api_base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(app_token)
            .form(&[("token", app_token)])
            .send()
            .await
            .map_err(|e| format!("apps.connections.open: {e}"))?;
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("apps.connections.open: bad json: {e}"))?;
        if value["ok"].as_bool() == Some(true) {
            return value["url"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "apps.connections.open: no url".to_string());
        }
        Err(format!(
            "apps.connections.open: {}",
            value["error"].as_str().unwrap_or("unknown_error")
        ))
    }

    /// `auth.test`: the bot's own identity (diagnostics).
    pub async fn auth_test(&self) -> Result<Value, String> {
        self.call("auth.test", json!({})).await
    }

    /// `files.info`: one shared file's metadata (its private download URL).
    pub async fn file_info(&self, file_id: &str) -> Result<Value, String> {
        self.call("files.info", json!({"file": file_id})).await
    }

    /// Download a `url_private*` file with the bot token.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("download: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("download: HTTP {}", resp.status()));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("download: {e}"))
    }

    /// Upload one file to the conversation (the modern external-upload
    /// flow). The caller falls back to a text note when this fails.
    pub async fn upload(
        &self,
        channel: &str,
        filename: &str,
        bytes: &[u8],
    ) -> Result<Value, String> {
        let info = self
            .call("files.getUploadURLExternal", json!({"filename": filename, "length": bytes.len()}))
            .await?;
        let upload_url = info["upload_url"]
            .as_str()
            .ok_or_else(|| "files.getUploadURLExternal: no upload_url".to_string())?;
        let file_id = info["file_id"]
            .as_str()
            .ok_or_else(|| "files.getUploadURLExternal: no file_id".to_string())?;
        let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new().part("file", part);
        let resp = self
            .http
            .post(upload_url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("upload: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("upload: HTTP {}", resp.status()));
        }
        self.call(
            "files.completeUploadExternal",
            json!({"files": [{"id": file_id}], "channel_id": channel}),
        )
        .await
    }
}
