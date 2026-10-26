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

    /// Download `mxc://<server>/<id>` (authenticated media, capped). The
    /// v1 (authenticated) endpoint is tried first; a 404 falls back to the
    /// legacy unauthenticated v3 path, which is all older homeservers
    /// serve.
    pub(super) async fn download_media(&self, mxc: &str) -> Result<Vec<u8>, String> {
        let (server, id) = mxc
            .strip_prefix("mxc://")
            .and_then(|r| r.split_once('/'))
            .ok_or_else(|| format!("not an mxc url: {mxc}"))?;
        let routes: [&[&str]; 2] = [
            &["_matrix", "client", "v1", "media", "download", server, id],
            &["_matrix", "media", "v3", "download", server, id],
        ];
        let mut last = String::new();
        for route in routes {
            let url = endpoint(&self.base, route)?;
            let resp = self
                .http
                .get(url)
                .bearer_auth(&self.token)
                .send()
                .await
                .map_err(|e| format!("media: {e}"))?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                last = format!("media: {}", resp.status());
                continue;
            }
            if !resp.status().is_success() {
                return Err(format!("media: {}", resp.status()));
            }
            let bytes = resp.bytes().await.map_err(|e| format!("media: {e}"))?;
            if bytes.len() > crate::voice::MAX_BYTES {
                return Err("media: too large".into());
            }
            return Ok(bytes.to_vec());
        }
        Err(last)
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

    /// `PUT …/rooms/{room}/send/{event_type}/{txn}` → the new event id.
    pub(super) async fn send_event(
        &self,
        room: &str,
        event_type: &str,
        content: &Value,
    ) -> Result<String, String> {
        let txn = format!("kagent-{}", self.txn.fetch_add(1, Ordering::Relaxed));
        let url = endpoint(
            &self.base,
            &[
                "_matrix", "client", "v3", "rooms", room, "send", event_type, &txn,
            ],
        )?;
        let resp = self
            .request(reqwest::Method::PUT, url, Some(content))
            .await?;
        resp["event_id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("send {event_type}: no event_id in the response"))
    }

    /// One text message with rendered HTML alongside the plain body.
    pub(super) async fn send(&self, room: &str, text: &str) -> Result<String, String> {
        self.send_event(room, "m.room.message", &rich_content(text))
            .await
    }

    /// One plain-text message (streaming drafts: no `formatted_body`).
    pub(super) async fn send_plain(&self, room: &str, text: &str) -> Result<String, String> {
        self.send_event(
            room,
            "m.room.message",
            &json!({"msgtype": "m.text", "body": text}),
        )
        .await
    }

    /// Replace `event_id`'s body (an `m.replace` relation). `rich` adds the
    /// renderer's HTML, which only the final edit of a stream carries.
    pub(super) async fn edit(
        &self,
        room: &str,
        event_id: &str,
        text: &str,
        rich: bool,
    ) -> Result<(), String> {
        let mut new_content = json!({"msgtype": "m.text", "body": text});
        let mut content = json!({
            "msgtype": "m.text",
            "body": format!("* {text}"),
            "m.new_content": new_content,
            "m.relates_to": { "rel_type": "m.replace", "event_id": event_id },
        });
        if rich {
            let html = ka_agent_core::markdown::render(text, ka_agent_core::markdown::Target::Html);
            new_content = json!({
                "msgtype": "m.text",
                "body": text,
                "format": "org.matrix.custom.html",
                "formatted_body": html,
            });
            content["format"] = json!("org.matrix.custom.html");
            content["formatted_body"] = json!(format!("* {html}"));
            content["m.new_content"] = new_content;
        }
        self.send_event(room, "m.room.message", &content)
            .await
            .map(|_| ())
    }

    /// An `m.annotation` reaction on `event_id` (best effort at call sites).
    pub(super) async fn react(&self, room: &str, event_id: &str, key: &str) -> Result<(), String> {
        let content = json!({
            "m.relates_to": {
                "rel_type": "m.annotation",
                "event_id": event_id,
                "key": key,
            },
        });
        self.send_event(room, "m.reaction", &content)
            .await
            .map(|_| ())
    }

    /// `PUT …/rooms/{room}/typing/{user}`; `timeout_ms = 0` stops it.
    pub(super) async fn typing(&self, room: &str, user: &str, timeout_ms: u64) {
        let url = match endpoint(
            &self.base,
            &["_matrix", "client", "v3", "rooms", room, "typing", user],
        ) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("matrix: typing: {e}");
                return;
            }
        };
        let body = json!({ "typing": timeout_ms > 0, "timeout": timeout_ms });
        if let Err(e) = self.request(reqwest::Method::PUT, url, Some(&body)).await {
            eprintln!("matrix: typing: {e}");
        }
    }

    /// `POST /_matrix/media/v3/upload` → the `mxc://` content URI.
    async fn upload(&self, name: &str, mime: &str, bytes: Vec<u8>) -> Result<String, String> {
        let mut url = endpoint(&self.base, &["_matrix", "media", "v3", "upload"])?;
        url.query_pairs_mut().append_pair("filename", name);
        let req = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header("Content-Type", mime)
            .body(bytes);
        let v = send_json(req).await?;
        v["content_uri"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "upload: no content_uri in the response".to_string())
    }

    /// Upload a file and post it as `m.image` (image extensions) or
    /// `m.file`, with `caption` as the body text.
    ///
    /// The size limit and the caption cap are applied before the bytes are
    /// read: an oversized file is refused from its metadata, and the
    /// caption is cut to one message body (it cannot be split like a reply).
    pub(super) async fn send_file(
        &self,
        room: &str,
        path: &std::path::Path,
        name: &str,
        caption: &str,
    ) -> Result<(), String> {
        const MAX_UPLOAD: u64 = 50 * 1024 * 1024;
        let caption = ka_agent_core::truncate_at_char(caption, REPLY_CAP);
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        if meta.len() > MAX_UPLOAD {
            return Err(format!("{} is larger than 50 MB", path.display()));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let size = bytes.len();
        let (msgtype, mime) = media_kind(name);
        let url = self.upload(name, mime, bytes).await?;
        let body = if caption.is_empty() { name } else { caption };
        let content = json!({
            "msgtype": msgtype,
            "body": body,
            "filename": name,
            "url": url,
            "info": { "size": size, "mimetype": mime },
        });
        self.send_event(room, "m.room.message", &content)
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

/// The `m.room.message` content for `text`: plain `body` for every client,
/// rendered HTML for the ones that read `formatted_body` (same renderer as
/// Telegram and the web).
fn rich_content(text: &str) -> Value {
    let html = ka_agent_core::markdown::render(text, ka_agent_core::markdown::Target::Html);
    json!({
        "msgtype": "m.text",
        "body": text,
        "format": "org.matrix.custom.html",
        "formatted_body": html,
    })
}

/// `(msgtype, mime)` for an outbound file, from its extension.
fn media_kind(name: &str) -> (&'static str, &'static str) {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => ("m.image", "image/png"),
        "jpg" | "jpeg" => ("m.image", "image/jpeg"),
        "gif" => ("m.image", "image/gif"),
        "webp" => ("m.image", "image/webp"),
        "svg" => ("m.image", "image/svg+xml"),
        "pdf" => ("m.file", "application/pdf"),
        "json" => ("m.file", "application/json"),
        "txt" | "md" | "log" => ("m.file", "text/plain"),
        "csv" => ("m.file", "text/csv"),
        "zip" => ("m.file", "application/zip"),
        _ => ("m.file", "application/octet-stream"),
    }
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
