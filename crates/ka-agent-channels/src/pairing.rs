//! One-time-code pairing (SEED.md §5.2): `ka-agent pair <channel>` issues a
//! short-lived code; the operator sends `/pair <code>` from the account to
//! link. The pending code lives in the `meta` table so the CLI and the
//! daemon (separate processes) share it.

use std::time::Duration;

use ka_agent_core::audit::{AuditRecord, actions};
use ka_agent_store::Store;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::telegram::TelegramClient;

/// Meta key holding the pending code as JSON `{"code","channel","expires_ms","attempts"}`.
pub const PAIRING_KEY: &str = "pairing.code";

const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
const CODE_LEN: usize = 8;
const TTL_MS: i64 = 10 * 60 * 1000;
const MAX_ATTEMPTS: u32 = 5;
const POLL_SECS: u64 = 25;

/// Reply sent to a Telegram account right after it pairs.
pub const TELEGRAM_WELCOME: &str =
    "✓ Paired. I only answer you from now on — try: what can you do?";

#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    code: String,
    channel: String,
    expires_ms: i64,
    attempts: u32,
}

/// Outcome of one `/pair <code>` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redeem {
    Linked,
    Wrong,
    Expired,
    Locked,
    NoCode,
}

/// An unknown contact seen in the audit log (`*.unauthorized` rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub channel: String,
    pub address: String,
    pub last_ms: i64,
    pub count: i64,
}

/// Issue a fresh code for `channel` (replacing any prior one), valid for
/// 10 minutes. Returned as shown to the operator: `XXXX-XXXX`.
pub fn issue(store: &Store, channel: &str) -> Result<String, String> {
    let mut code = String::with_capacity(CODE_LEN);
    let mut buf = [0u8; 32];
    while code.len() < CODE_LEN {
        OsRng
            .try_fill_bytes(&mut buf)
            .map_err(|e| format!("no randomness available: {e}"))?;
        // rejection sampling keeps the alphabet uniform (30 symbols)
        let limit = 256 - 256 % ALPHABET.len();
        for b in buf {
            if code.len() == CODE_LEN {
                break;
            }
            if (b as usize) < limit {
                code.push(ALPHABET[b as usize % ALPHABET.len()] as char);
            }
        }
    }
    let pending = Pending {
        code: code.clone(),
        channel: channel.to_string(),
        expires_ms: ka_agent_core::now_ms() + TTL_MS,
        attempts: 0,
    };
    let json = serde_json::to_string(&pending).map_err(|e| e.to_string())?;
    store
        .meta_set(PAIRING_KEY, &json)
        .map_err(|e| format!("saving the pairing code: {e}"))?;
    Ok(format!("{}-{}", &code[..4], &code[4..]))
}

/// Whether `text` is a `/pair` command (case-insensitive, optional
/// `@botname` suffix on the command word).
pub fn is_pair_command(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or("");
    let cmd = first.split('@').next().unwrap_or("");
    cmd.eq_ignore_ascii_case("/pair")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn clear(store: &Store) {
    if let Err(e) = store.meta_set(PAIRING_KEY, "") {
        eprintln!("pairing: clearing the code: {e}");
    }
}

/// Try to redeem `text` (`/pair <code>`, dash optional, any case) sent by
/// `address` on `channel`. Success links the identity, audits
/// `identity.pair` and consumes the code; the fifth wrong attempt burns it.
pub fn redeem(store: &Store, channel: &str, address: &str, text: &str) -> Redeem {
    let raw = match store.meta_get(PAIRING_KEY) {
        Ok(Some(raw)) if !raw.is_empty() => raw,
        _ => return Redeem::NoCode,
    };
    let Ok(mut pending) = serde_json::from_str::<Pending>(&raw) else {
        return Redeem::NoCode;
    };
    if pending.channel != channel {
        return Redeem::NoCode;
    }
    if ka_agent_core::now_ms() > pending.expires_ms {
        clear(store);
        return Redeem::Expired;
    }
    if pending.attempts >= MAX_ATTEMPTS {
        clear(store);
        return Redeem::Locked;
    }
    if !is_pair_command(text) {
        return Redeem::Wrong;
    }
    let given: String = text
        .split_whitespace()
        .skip(1)
        .flat_map(|w| w.chars())
        .filter(|c| *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if constant_time_eq(given.as_bytes(), pending.code.as_bytes()) {
        if let Err(e) = store.link_identity(channel, address, "pairing") {
            eprintln!("pairing: link {channel}:{address}: {e}");
            return Redeem::Wrong;
        }
        let mut record = AuditRecord::new(
            &format!("{channel}:{address}"),
            actions::IDENTITY_PAIR,
            &format!("{channel}:{address}"),
            None,
        );
        if let Err(e) = store.append_audit(&mut record) {
            eprintln!("pairing: audit: {e}");
        }
        clear(store);
        return Redeem::Linked;
    }
    pending.attempts += 1;
    if pending.attempts >= MAX_ATTEMPTS {
        clear(store);
        return Redeem::Locked;
    }
    match serde_json::to_string(&pending) {
        Ok(json) => {
            if let Err(e) = store.meta_set(PAIRING_KEY, &json) {
                eprintln!("pairing: saving attempts: {e}");
            }
        }
        Err(e) => eprintln!("pairing: {e}"),
    }
    Redeem::Wrong
}

/// Pair Telegram without the daemon: long-poll `getUpdates` (from the
/// stored `telegram.offset`, persisting it per update) until a private
/// `/pair <code>` redeems the pending code. Returns `(uid, first_name)`.
pub async fn wait_telegram_standalone(
    api_base: &str,
    token: &str,
    store: &Store,
    timeout: Duration,
) -> Result<(i64, String), String> {
    let client = TelegramClient::new(api_base, token);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut offset = store
        .meta_get("telegram.offset")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for /pair".into());
        }
        let poll = remaining.as_secs().clamp(1, POLL_SECS);
        let updates = match tokio::time::timeout(
            remaining + Duration::from_secs(1),
            client.get_updates(offset, poll),
        )
        .await
        {
            Err(_) => return Err("timed out waiting for /pair".into()),
            Ok(Err(e)) if e.is_auth_failure() => {
                return Err(format!("telegram rejected the bot token: {e}"));
            }
            Ok(Err(e)) => {
                eprintln!("telegram: poll error: {e}; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Ok(Ok(updates)) => updates,
        };
        if updates.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for update in &updates {
            let Some(update_id) = update["update_id"].as_i64() else {
                continue;
            };
            if update_id < offset {
                continue;
            }
            offset = update_id + 1;
            if let Err(e) = store.meta_set("telegram.offset", &offset.to_string()) {
                eprintln!("telegram: persist offset: {e}");
            }
            if let Some(found) = try_pair_update(&client, store, update).await? {
                return Ok(found);
            }
        }
    }
}

/// One update during standalone pairing: `Ok(Some)` when it linked,
/// `Err` when the code can no longer succeed.
async fn try_pair_update(
    client: &TelegramClient,
    store: &Store,
    update: &Value,
) -> Result<Option<(i64, String)>, String> {
    let message = &update["message"];
    if message["chat"]["type"].as_str() != Some("private") {
        return Ok(None);
    }
    let (Some(uid), Some(text)) = (message["from"]["id"].as_i64(), message["text"].as_str()) else {
        return Ok(None);
    };
    if !is_pair_command(text) {
        return Ok(None);
    }
    match redeem(store, "telegram", &uid.to_string(), text) {
        Redeem::Linked => {
            let chat_id = message["chat"]["id"].as_i64().unwrap_or(uid);
            let _ = client.send_message(chat_id, TELEGRAM_WELCOME, None).await;
            if let Err(e) = store.meta_set("telegram.chat_id", &chat_id.to_string()) {
                eprintln!("telegram: persist chat id: {e}");
            }
            let name = message["from"]["first_name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            Ok(Some((uid, name)))
        }
        Redeem::Wrong => Ok(None),
        Redeem::Expired => Err("the pairing code expired".into()),
        Redeem::Locked => Err("too many wrong codes — the pairing code was cancelled".into()),
        Redeem::NoCode => Err("no pairing code is pending".into()),
    }
}

/// Unknown contacts (audited `*.unauthorized` attempts), newest first.
pub fn unauthorized_contacts(store: &Store, limit: usize) -> Vec<Contact> {
    store
        .unauthorized_contacts(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(actor, last_ms, count)| {
            let (channel, address) = actor.split_once(':').unwrap_or((actor.as_str(), ""));
            Contact {
                channel: channel.to_string(),
                address: address.to_string(),
                last_ms,
                count,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn issued_code_shape() {
        let store = Store::open_in_memory().unwrap();
        let code = issue(&store, "telegram").unwrap();
        assert_eq!(code.len(), 9);
        assert_eq!(&code[4..5], "-");
        assert!(
            code.chars()
                .filter(|c| *c != '-')
                .all(|c| ALPHABET.contains(&(c as u8)))
        );
    }

    #[test]
    fn redeem_links_once_case_and_dash_insensitive() {
        let store = Store::open_in_memory().unwrap();
        let code = issue(&store, "telegram").unwrap();
        let typed = format!("/pair {}", code.replace('-', "").to_lowercase());
        assert_eq!(redeem(&store, "matrix", "@x:y", &typed), Redeem::NoCode);
        assert_eq!(redeem(&store, "telegram", "77", &typed), Redeem::Linked);
        assert!(store.is_linked("telegram", "77").unwrap());
        assert_eq!(redeem(&store, "telegram", "78", &typed), Redeem::NoCode);
        let audit = store.recent_audit(5).unwrap();
        assert!(audit.iter().any(|r| r.action == actions::IDENTITY_PAIR));
    }

    #[test]
    fn five_wrong_attempts_lock_the_code() {
        let store = Store::open_in_memory().unwrap();
        let code = issue(&store, "telegram").unwrap();
        for _ in 0..4 {
            assert_eq!(
                redeem(&store, "telegram", "66", "/pair WRONG-CODE"),
                Redeem::Wrong
            );
        }
        assert_eq!(
            redeem(&store, "telegram", "66", "/pair WRONG-CODE"),
            Redeem::Locked
        );
        assert_eq!(
            redeem(&store, "telegram", "66", &format!("/pair {code}")),
            Redeem::NoCode
        );
        assert!(!store.is_linked("telegram", "66").unwrap());
    }

    #[test]
    fn expired_code_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let code = issue(&store, "telegram").unwrap();
        let mut pending: Pending =
            serde_json::from_str(&store.meta_get(PAIRING_KEY).unwrap().unwrap()).unwrap();
        pending.expires_ms = 1;
        store
            .meta_set(PAIRING_KEY, &serde_json::to_string(&pending).unwrap())
            .unwrap();
        assert_eq!(
            redeem(&store, "telegram", "1", &format!("/pair {code}")),
            Redeem::Expired
        );
    }

    #[test]
    fn pair_command_detection() {
        assert!(is_pair_command("/pair ABCD-EFGH"));
        assert!(is_pair_command("/PAIR@my_bot x"));
        assert!(!is_pair_command("/pairing x"));
        assert!(!is_pair_command("pair x"));
    }

    #[test]
    fn contacts_split_actor() {
        let store = Store::open_in_memory().unwrap();
        for _ in 0..2 {
            let mut r = AuditRecord::new("telegram:42", "telegram.unauthorized", "", None);
            store.append_audit(&mut r).unwrap();
        }
        let c = unauthorized_contacts(&store, 10);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].channel, "telegram");
        assert_eq!(c[0].address, "42");
        assert_eq!(c[0].count, 2);
    }
}
