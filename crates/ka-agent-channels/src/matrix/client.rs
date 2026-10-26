// MatrixClient (Client): the CS-API HTTP surface. Split from matrix.rs (moves only).

use super::*;

use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
/// A small CS-API client. URLs are built segment by segment so room ids
/// (`!abc:server`) are percent-encoded.
pub(super) struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
    txn: AtomicU64,
}

fn endpoint(base: &str, segments: &[&str]) -> Result<url::Url, String> {
    let mut url = url::Url::parse(base).map_err(|e| format!("bad homeserver url {base:?}: {e}"))?;
    url.path_segments_mut()
        .map_err(|()| format!("bad homeserver url {base:?}"))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

async fn send_json(req: reqwest::RequestBuilder) -> Result<Value, String> {
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {}: {}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("bad json: {e}"))
}

impl Client {
    pub(super) fn new(base: String, token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(SYNC_TIMEOUT_SECS + 15))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base,
            token,
            txn: AtomicU64::new(1),
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: url::Url,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let mut req = self
            .http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(b);
        }
        send_json(req).await
    }

    /// Download `mxc://<server>/<id>` (authenticated media, capped).
    pub(super) async fn download_media(&self, mxc: &str) -> Result<Vec<u8>, String> {
        let (server, id) = mxc
            .strip_prefix("mxc://")
            .and_then(|r| r.split_once('/'))
            .ok_or_else(|| format!("not an mxc url: {mxc}"))?;
        let url = endpoint(
            &self.base,
            &["_matrix", "client", "v1", "media", "download", server, id],
        )?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("media: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("media: {}", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| format!("media: {e}"))?;
        if bytes.len() > crate::voice::MAX_BYTES {
            return Err("media: too large".into());
        }
        Ok(bytes.to_vec())
    }

    pub(super) async fn sync(&self, since: Option<&str>) -> Result<Value, String> {
        let mut url = endpoint(&self.base, &["_matrix", "client", "v3", "sync"])?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("timeout", &(SYNC_TIMEOUT_SECS * 1000).to_string());
            q.append_pair("filter", r#"{"room":{"timeline":{"limit":50}}}"#);
            if let Some(s) = since {
                q.append_pair("since", s);
            }
        }
        self.request(reqwest::Method::GET, url, None).await
    }

    async fn send(&self, room: &str, text: &str) -> Result<(), String> {
        let txn = format!("kagent-{}", self.txn.fetch_add(1, Ordering::Relaxed));
        let url = endpoint(
            &self.base,
            &[
                "_matrix",
                "client",
                "v3",
                "rooms",
                room,
                "send",
                "m.room.message",
                &txn,
            ],
        )?;
        // Plain `body` for every client, rendered HTML for the ones that
        // read `formatted_body` (same renderer as Telegram and the web).
        let html = ka_agent_core::markdown::render(text, ka_agent_core::markdown::Target::Html);
        self.request(
            reqwest::Method::PUT,
            url,
            Some(&json!({
                "msgtype": "m.text",
                "body": text,
                "format": "org.matrix.custom.html",
                "formatted_body": html,
            })),
        )
        .await
        .map(|_| ())
    }

    pub(super) async fn send_split(&self, room: &str, text: &str) {
        for chunk in split_reply(text) {
            if let Err(e) = self.send(room, &chunk).await {
                eprintln!("matrix: send: {e}");
            }
        }
    }

    pub(super) async fn join(&self, room: &str) -> Result<(), String> {
        let url = endpoint(
            &self.base,
            &["_matrix", "client", "v3", "rooms", room, "join"],
        )?;
        self.request(reqwest::Method::POST, url, Some(&json!({})))
            .await
            .map(|_| ())
    }
}

/// Password login (setup wizard): returns the access token for `user`.
pub async fn login(homeserver: &str, user: &str, password: &str) -> Result<String, String> {
    let url = endpoint(homeserver, &["_matrix", "client", "v3", "login"])?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let body = json!({
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": user},
        "password": password,
        "initial_device_display_name": "Ka Agent",
    });
    let v = send_json(http.post(url).json(&body)).await?;
    v["access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "login response has no access_token".to_string())
}

fn split_reply(text: &str) -> Vec<String> {
    if text.len() <= REPLY_CAP {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > REPLY_CAP {
        let capped = ka_agent_core::truncate_at_char(rest, REPLY_CAP);
        let cut = capped.rfind('\n').map(|i| i + 1).unwrap_or(capped.len());
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_ids_are_percent_encoded() {
        let url = endpoint(
            "https://matrix.example.org/",
            &["_matrix", "client", "v3", "rooms", "!a/b?c:server", "join"],
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            url.as_str(),
            "https://matrix.example.org/_matrix/client/v3/rooms/!a%2Fb%3Fc:server/join"
        );
    }
}
