//! Discord REST client: messages, interaction callbacks, gateway discovery
//! and file uploads, all paced by the shared rate limiter.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::ratelimit::RateLimiter;

pub struct DiscordClient {
    http: reqwest::Client,
    api_base: String,
    token: String,
    limiter: Arc<RateLimiter>,
}

impl DiscordClient {
    pub fn new(api_base: &str, token: String, limiter: Arc<RateLimiter>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_base: format!("{}/api/v10", api_base.trim_end_matches('/')),
            token,
            limiter,
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("Authorization", format!("Bot {}", self.token))
    }

    async fn send(&self, key: &str, req: reqwest::RequestBuilder) -> Result<Value, String> {
        self.limiter.wait(key).await;
        let resp = self
            .auth(req)
            .send()
            .await
            .map_err(|e| format!("{key}: {e}"))?;
        let status = resp.status();
        let headers: Vec<(&str, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let header_refs: Vec<(&str, String)> = headers
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        self.limiter
            .apply_headers(key, &header_refs, ka_agent_core::now_ms());
        if status.as_u16() == 429 {
            return Err(format!("{key}: rate limited"));
        }
        let text = resp.text().await.map_err(|e| format!("{key}: {e}"))?;
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}));
        if !status.is_success() {
            return Err(format!(
                "{key}: HTTP {status}: {}",
                value["message"].as_str().unwrap_or("error")
            ));
        }
        Ok(value)
    }

    /// `GET /gateway/bot`: the WSS URL and the session-start budget.
    pub async fn gateway_bot(&self) -> Result<Value, String> {
        self.send(
            "gateway",
            self.http.get(format!("{}/gateway/bot", self.api_base)),
        )
        .await
    }

    pub async fn send_message(&self, channel: &str, body: Value) -> Result<Value, String> {
        self.send(
            "messages",
            self.http
                .post(format!("{}/channels/{channel}/messages", self.api_base))
                .json(&body),
        )
        .await
    }

    /// `POST /interactions/{id}/{token}/callback` (type 7 = update the
    /// message the button belongs to).
    pub async fn interaction_callback(
        &self,
        interaction_id: &str,
        token: &str,
        body: Value,
    ) -> Result<Value, String> {
        self.send(
            "interactions",
            self.http
                .post(format!(
                    "{}/interactions/{interaction_id}/{token}/callback",
                    self.api_base
                ))
                .json(&body),
        )
        .await
    }

    /// Upload one file into a channel (`multipart/form-data`).
    pub async fn upload(
        &self,
        channel: &str,
        filename: &str,
        bytes: &[u8],
        content: &str,
    ) -> Result<Value, String> {
        let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new()
            .text("payload_json", json!({"content": content}).to_string())
            .part("files[0]", part);
        self.send(
            "uploads",
            self.http
                .post(format!("{}/channels/{channel}/messages", self.api_base))
                .multipart(form),
        )
        .await
    }

    /// Download an attachment (the CDN URL is pre-signed; no auth needed).
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .http
            .get(url)
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
}
