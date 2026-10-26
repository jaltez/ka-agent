//! `http_get`: one HTTP(S) fetch behind the operator's grants, with the
//! SSRF + DNS-rebinding guard (private/link-local targets refused unless
//! granted). The blocking client runs on a blocking thread, never in the
//! reactor. Split from the old MCP bridge (moves only).

use serde_json::{Value, json};

use super::{ToolCtx, ToolFuture};

pub(crate) fn get_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "url": {"type": "string", "description": "http(s) URL to fetch"}
        },
        "required": ["url"]
    })
}

pub(crate) fn get<'a>(ctx: &'a ToolCtx, args: Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let url = args["url"].as_str().ok_or("missing 'url'")?.to_string();
        let grants = crate::grants::Grants::load(&ctx.store).map_err(|e| format!("grants: {e}"))?;
        tokio::task::spawn_blocking(move || super::http::fetch(&url, &grants))
            .await
            .map_err(|e| format!("http task: {e}"))?
    })
}

/// SSRF guard for `http_get`: refuse loopback, link-local (cloud metadata),
/// unique-local, private and unspecified targets — as IP literals in any
/// form (v4/v6, bracketed) — unless the operator granted `http:get:private`.
/// Redirects are followed manually and every hop re-checks the same guard.
/// Resolve `host` and reject private addresses before connecting (DNS
/// rebinding: a public name that resolves to 127.0.0.1 or link-local).
/// `Ok(())` when every address is public; the caller connects with
/// `resolve` so the answer cannot change between check and connect.
pub(crate) fn assert_public_host(host: &str) -> Result<std::net::IpAddr, String> {
    use std::net::ToSocketAddrs as _;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let with_port = format!("{bare}:80");
    let addrs: Vec<std::net::SocketAddr> = match with_port.to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => return Err(format!("cannot resolve {bare:?}: {e}")),
    };
    let Some(addr) = addrs.first() else {
        return Err(format!("no address for {bare:?}"));
    };
    if http_target_is_private(bare) || http_target_is_private(&addr.ip().to_string()) {
        return Err("private target refused (grant http:get:private to allow)".into());
    }
    Ok(addr.ip())
}

pub(crate) fn http_target_is_private(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_private()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
                    // 172.16/12 is `is_private`, but 169.254/16 (metadata)
                    // and 0.0.0.0/8 are covered above.
                    || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64 // 100.64/10 CGNAT
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
            }
        };
    }
    // Names: only the canonical localhost forms; a resolving name is checked
    // after resolution by the caller when possible.
    bare.eq_ignore_ascii_case("localhost")
        || bare.eq_ignore_ascii_case("localhost.localdomain")
        || bare.ends_with(".localhost")
        || bare.ends_with(".local")
        || bare.ends_with(".internal")
}

/// A blocking HTTP client for tool calls: per-call (they are rare), no
/// redirects (the SSRF guard walks each hop itself), 15 s ceiling, and the
/// product User-Agent. Runs in the `__mcp` bridge process or on
/// `spawn_blocking` threads — never inside the async reactor.
pub(crate) fn http_client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("ka-agent")
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// Blocking HTTP GET, returns the response body as text. The `http_get`
/// tool with manual redirects: each hop's host must pass the same SSRF
/// guard as the first (a public host redirecting to 169.254.169.254 must
/// not become a metadata read).
pub(crate) fn http_get_no_redirect(
    url: &url::Url,
    timeout_secs: u64,
    grants: &crate::grants::Grants,
) -> Result<String, String> {
    let allow_private = grants.check("http:get:private");
    let mut current = url.clone();
    for _ in 0..5 {
        // DNS rebinding guard: resolve now, refuse private answers, and
        // pin the connection to the address we checked — a second lookup
        // could answer differently between check and connect.
        let client = if allow_private {
            http_client(timeout_secs)?
        } else {
            let host = current.host_str().unwrap_or("").to_string();
            let ip = assert_public_host(&host)?;
            let port = current.port_or_known_default().unwrap_or(80);
            reqwest::blocking::Client::builder()
                .user_agent("ka-agent")
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .redirect(reqwest::redirect::Policy::none())
                .resolve(&host, std::net::SocketAddr::new(ip, port))
                .build()
                .map_err(|e| format!("http client: {e}"))?
        };
        let resp = client
            .get(current.as_str())
            .send()
            .map_err(|e| format!("http: {e}"))?;
        let status = resp.status().as_u16();
        if (301..=308).contains(&status) {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let next = url::Url::parse(&location)
                .or_else(|_| current.join(&location))
                .map_err(|e| format!("redirect target: {e}"))?;
            if !allow_private {
                // Name check first (literal IPs), then the resolved answer.
                let host = next.host_str().unwrap_or("");
                if http_target_is_private(host) {
                    return Err(format!(
                        "redirect to private target {host} refused (grant http:get:private to allow)"
                    ));
                }
                assert_public_host(host)?;
            }
            current = next;
            continue;
        }
        if !resp.status().is_success() {
            return Err(format!("http status {}", resp.status()));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.starts_with("text/")
            && !content_type.starts_with("application/json")
            && !content_type.starts_with("application/xml")
        {
            return Err(format!("unsupported content-type: {content_type}"));
        }
        return resp.text().map_err(|e| format!("read body: {e}"));
    }
    Err("too many redirects".into())
}

/// The `http_get` body: parse, check the grant, guard the host, fetch.
pub(crate) fn fetch(url: &str, grants: &crate::grants::Grants) -> Result<String, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("bad url: {e}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(format!("unsupported scheme: {}", parsed.scheme()));
    }
    let host = parsed.host_str().ok_or("missing host")?.to_string();
    let scope = format!("http:get:{host}");
    if !grants.check(&scope) {
        return Err(format!(
            "no grant for {scope} — ask the operator to run: ka-agent grant add {scope}"
        ));
    }
    if !grants.check("http:get:private") && http_target_is_private(&host) {
        return Err("private/loopback/link-local hosts require the http:get:private grant".into());
    }
    let body = http_get_no_redirect(&parsed, 15, grants)?;
    if body.len() > 200_000 {
        Ok(format!(
            "{}… (truncated at 200 KB)",
            ka_agent_core::truncate_at_char(&body, 200_000)
        ))
    } else {
        Ok(body)
    }
}
