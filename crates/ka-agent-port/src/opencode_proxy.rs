//! The OpenCode Go endpoint requires `x-opencode-session` on every request
//! for routing ("MissingSessionID", HTTP 400), and the pinned ka-dialect
//! (0.3.0) OpenAI-chat wire sends only `content-type` + `authorization` —
//! extra headers are not expressible in a dialect row. The worker owns the
//! catalog it spawns the engine with, so `opencode-go` dialects are pointed
//! at a loopback proxy (one per worker) that adds the header and forwards
//! everything else untouched, streaming included.

use ka_dialect::Catalog;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The endpoint that demands the header.
const OPENCODE_GO_BASE: &str = "https://opencode.ai/zen/go/v1";

/// Rewrite the `opencode-go` dialects in `catalog` through a proxy bound to
/// loopback. One proxy serves them all (same upstream, same session id);
/// the id is stable for the worker's lifetime — one worker, one strand.
pub fn proxied_catalog(mut catalog: Catalog, session_id: &str) -> Catalog {
    let needs = catalog
        .dialects
        .values()
        .any(|d| d.base_url.as_deref() == Some(OPENCODE_GO_BASE));
    if !needs {
        return catalog;
    }
    let base = match spawn(OPENCODE_GO_BASE, session_id) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("ka-agent: opencode proxy not started ({e}); opencode-go may be refused");
            return catalog;
        }
    };
    for d in catalog.dialects.values_mut() {
        if d.base_url.as_deref() == Some(OPENCODE_GO_BASE) {
            d.base_url = Some(base.clone());
        }
    }
    catalog
}

/// Serve on an ephemeral loopback port; returns the `http://127.0.0.1:P`
/// base standing in for `upstream`.
pub fn spawn(upstream: &str, session_id: &str) -> Result<String, String> {
    let upstream = upstream.trim_end_matches('/').to_string();
    // Bind synchronously (spawn is called from sync catalog wiring); hand
    // the socket to tokio inside the worker's runtime.
    let std_listener =
        std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind 127.0.0.1:0: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("nonblocking: {e}"))?;
    let listener = tokio::net::TcpListener::from_std(std_listener).map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let session = session_id.to_string();
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let (upstream, session, client) = (upstream.clone(), session.clone(), client.clone());
            tokio::spawn(async move {
                if let Err(e) = serve(&mut socket, &upstream, &session, &client).await {
                    let _ = socket
                        .write_all(
                            format!(
                                "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\nka-agent proxy: {e}",
                                e.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                }
            });
        }
    });
    Ok(format!("http://{addr}"))
}

/// One request per connection: read it whole (the engine sends JSON with
/// `content-length`), forward with the header added, stream the answer back
/// (SSE chunks are written as they arrive).
async fn serve(
    socket: &mut tokio::net::TcpStream,
    upstream: &str,
    session: &str,
    client: &reqwest::Client,
) -> Result<(), String> {
    // One buffered read loop: head and body can arrive in the same packet,
    // so splitting them across two readers would lose bytes.
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let (head, body) = loop {
        let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
        if let Some(h) = head_end {
            let head_text = String::from_utf8_lossy(&buf[..h]).into_owned();
            let want = content_length_of(&head_text);
            if buf.len() >= h + want {
                break (head_text, buf[h..].to_vec());
            }
        }
        if buf.len() > 64 * 1024 * 1024 {
            return Err("request too large".into());
        }
        let n = socket
            .read(&mut tmp)
            .await
            .map_err(|e| format!("read request: {e}"))?;
        if n == 0 {
            return Err("connection closed mid-request".into());
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let (method, target, headers, _) = parse_head(&head)?;

    let url = format!("{upstream}{target}");
    let mut req = client
        .request(
            method
                .parse::<reqwest::Method>()
                .map_err(|e| format!("method: {e}"))?,
            &url,
        )
        .timeout(std::time::Duration::from_secs(600));
    for (k, v) in &headers {
        if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        req = req.header(k, v);
    }
    // The one job: a stable session id for routing. An explicit value from
    // the caller (if the engine ever sends one) wins.
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("x-opencode-session"))
    {
        req = req.header("x-opencode-session", session);
    }
    let resp = if !body.is_empty() {
        req.body(body).send().await
    } else {
        req.send().await
    }
    .map_err(|e| format!("upstream: {e}"))?;

    let status = resp.status();
    let mut head_out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream"),
    );
    for (k, v) in resp.headers() {
        if matches!(
            k.as_str(),
            "content-length" | "transfer-encoding" | "connection" | "content-type"
        ) {
            continue;
        }
        if let (Ok(k), Ok(v)) = (
            k.as_str().parse::<reqwest::header::HeaderName>(),
            v.to_str(),
        ) {
            head_out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    head_out.push_str("\r\n");
    socket
        .write_all(head_out.as_bytes())
        .await
        .map_err(|e| format!("write head: {e}"))?;

    // Chunked transfer so the bytes we have can go out as they arrive.
    let mut stream = resp;
    loop {
        match stream.chunk().await {
            Ok(Some(chunk)) => {
                socket
                    .write_all(format!("{chunk_len:x}\r\n", chunk_len = chunk.len()).as_bytes())
                    .await
                    .map_err(|e| format!("write chunk head: {e}"))?;
                socket
                    .write_all(&chunk)
                    .await
                    .map_err(|e| format!("write chunk: {e}"))?;
                socket
                    .write_all(b"\r\n")
                    .await
                    .map_err(|e| format!("write chunk end: {e}"))?;
            }
            Ok(None) => break,
            Err(e) => {
                let _ = socket.write_all(b"0\r\n\r\n").await;
                return Err(format!("upstream stream: {e}"));
            }
        }
    }
    socket
        .write_all(b"0\r\n\r\n")
        .await
        .map_err(|e| format!("write final chunk: {e}"))?;
    socket.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

/// `Content-Length` of a request head (0 when absent or malformed).
fn content_length_of(head: &str) -> usize {
    head.lines()
        .skip(1)
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
}

type ParsedHead = (String, String, Vec<(String, String)>, usize);

fn parse_head(head: &str) -> Result<ParsedHead, String> {
    let mut lines = head.split("\r\n");
    let request = lines.next().unwrap_or_default();
    let mut parts = request.split_whitespace();
    let method = parts.next().ok_or("no method")?.to_string();
    let target = parts.next().ok_or("no path")?.to_string();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_string(), v.trim().to_string());
        if k.eq_ignore_ascii_case("content-length") {
            content_length = v.parse().map_err(|_| format!("bad content-length {v:?}"))?;
        }
        headers.push((k, v));
    }
    Ok((method, target, headers, content_length))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::io::{Read as _, Write as _};
    use std::sync::{Arc, Mutex};

    type Captured = (String, Vec<(String, String)>, Vec<u8>);

    /// A fake upstream: captures one request, answers with one SSE chunk.
    fn upstream() -> (Arc<Mutex<Option<Captured>>>, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let slot: Arc<Mutex<Option<Captured>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&slot);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = Vec::new();
                let mut head_end = None;
                loop {
                    let mut tmp = [0u8; 8192];
                    let n = match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if head_end.is_none() {
                        head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                    }
                    if let Some(h) = head_end {
                        let head = String::from_utf8_lossy(&buf[..h]).to_lowercase();
                        let want = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= h + want {
                            break;
                        }
                    }
                }
                let h = head_end.unwrap_or(buf.len());
                let head_text = String::from_utf8_lossy(&buf[..h]).into_owned();
                let headers: Vec<(String, String)> = head_text
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();
                let first = head_text.lines().next().unwrap_or_default().to_string();
                *sink.lock().unwrap() = Some((first, headers, buf[h..].to_vec()));
                let body = b"data: {\"delta\":\"hi\"}\n\n";
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.write_all(body);
                let _ = stream.write_all(b"\r\n0\r\n\r\n");
            }
        });
        (slot, format!("http://{addr}"))
    }

    #[tokio::test]
    async fn adds_the_session_header_and_streams_the_answer() {
        let (slot, base) = upstream();
        let proxy = super::spawn(&base, "sess-42").unwrap();
        let resp = reqwest::Client::new()
            .post(format!("{proxy}/chat/completions"))
            .header("authorization", "Bearer tok")
            .json(&serde_json::json!({"model": "beam-free"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        assert!(text.contains("delta"), "{text}");
        // The upstream saw the header and the original authorization.
        for _ in 0..100 {
            if slot.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let (first, headers, body) = slot.lock().unwrap().clone().unwrap();
        assert!(first.starts_with("POST /chat/completions"), "{first}");
        let get = |k: &str| {
            headers
                .iter()
                .find(|(h, _)| h.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("x-opencode-session").as_deref(), Some("sess-42"));
        assert_eq!(get("authorization").as_deref(), Some("Bearer tok"));
        assert!(String::from_utf8_lossy(&body).contains("beam-free"));
    }

    #[tokio::test]
    async fn catalog_rows_for_the_go_endpoint_are_rewritten_others_untouched() {
        let catalog = crate::catalog::full_catalog();
        let proxied = super::proxied_catalog(catalog.clone(), "s");
        let go = proxied.dialects.get("opencode-go/beam-free").unwrap();
        assert!(
            go.base_url
                .as_deref()
                .unwrap()
                .starts_with("http://127.0.0.1:"),
            "{:?}",
            go.base_url
        );
        let zen = proxied.dialects.get("opencode/beam-free");
        assert_eq!(
            zen.and_then(|d| d.base_url.clone()),
            catalog
                .dialects
                .get("opencode/beam-free")
                .and_then(|d| d.base_url.clone())
        );
        // Idempotent shape: a catalog without go rows is returned unchanged.
        let mut none = catalog;
        none.dialects.retain(|k, _| !k.starts_with("opencode-go/"));
        let before = none.dialects.clone();
        let out = super::proxied_catalog(none, "s");
        assert_eq!(out.dialects, before);
    }
}
