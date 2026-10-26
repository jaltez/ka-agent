//! Append-only audit log with a SHA-256 hash chain.
//! Split from lib.rs (moves only).

use super::*;
use sha2::{Digest, Sha256};

/// Canonical audit serialization target. Field order below IS the canonical
/// form; see [`Store::append_audit`].
#[derive(Serialize)]
struct AuditPayload<'a> {
    seq: i64,
    at_ms: i64,
    actor: &'a str,
    action: &'a str,
    subject: &'a str,
    detail: Option<&'a str>,
}

fn audit_hash(prev_hash: &str, canonical_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(canonical_json.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

impl Store {
    /// Append one audit record to the hash chain.
    ///
    /// Assigns `seq`, sets `prev_hash` to the previous record's `hash`
    /// (empty string for the first record), and computes
    /// `hash = hex(sha256(prev_hash_utf8 || canonical_json))` where
    /// `canonical_json` is `serde_json`'s serialization of
    /// [`AuditPayload`] — a dedicated struct whose *field declaration order*
    /// (`seq, at_ms, actor, action, subject, detail`) is the canonical
    /// form. `serde_json` emits struct fields in declaration order, so the
    /// bytes are stable across builds; [`Store::verify_audit_chain`]
    /// recomputes them exactly this way.
    pub fn append_audit(&self, record: &mut AuditRecord) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let prev_hash: Option<String> = tx
            .query_row(
                "SELECT hash FROM audit ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let prev_hash = prev_hash.unwrap_or_default();
        let seq: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM audit", [], |r| {
            r.get(0)
        })?;
        record.seq = seq;
        record.prev_hash = prev_hash.clone();
        let payload = AuditPayload {
            seq,
            at_ms: record.at_ms,
            actor: &record.actor,
            action: &record.action,
            subject: &record.subject,
            detail: record.detail.as_deref(),
        };
        let canonical = serde_json::to_string(&payload).unwrap_or_default();
        let hash = audit_hash(&prev_hash, &canonical);
        tx.execute(
            "INSERT INTO audit(seq, at_ms, actor, action, subject, detail, prev_hash, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                seq,
                record.at_ms,
                record.actor,
                record.action,
                record.subject,
                record.detail,
                prev_hash,
                hash
            ],
        )?;
        record.hash = hash;
        tx.commit()?;
        Ok(())
    }

    /// Recompute every audit row's hash chain; returns the verified record
    /// count or [`StoreError::BrokenAuditChain`] at the first mismatch.
    pub fn verify_audit_chain(&self) -> Result<usize> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT seq, at_ms, actor, action, subject, detail, prev_hash, hash
             FROM audit ORDER BY seq",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut expected_prev = String::new();
        let mut count = 0usize;
        for row in rows {
            let (seq, at_ms, actor, action, subject, detail, prev_hash, hash) = row?;
            if prev_hash != expected_prev {
                return Err(StoreError::BrokenAuditChain {
                    seq,
                    why: "prev_hash does not link to the previous record",
                });
            }
            let payload = AuditPayload {
                seq,
                at_ms,
                actor: &actor,
                action: &action,
                subject: &subject,
                detail: detail.as_deref(),
            };
            let canonical = serde_json::to_string(&payload).unwrap_or_default();
            if audit_hash(&expected_prev, &canonical) != hash {
                return Err(StoreError::BrokenAuditChain {
                    seq,
                    why: "hash mismatch",
                });
            }
            expected_prev = hash;
            count += 1;
        }
        Ok(count)
    }

    /// The most recent audit records, newest first (web audit view).
    pub fn recent_audit(&self, limit: usize) -> Result<Vec<AuditRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT seq, at_ms, actor, action, subject, detail, prev_hash, hash
             FROM audit ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(AuditRecord {
                seq: r.get(0)?,
                at_ms: r.get(1)?,
                actor: r.get(2)?,
                action: r.get(3)?,
                subject: r.get(4)?,
                detail: r.get(5)?,
                prev_hash: r.get(6)?,
                hash: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn audit_chain_append_verify_tamper() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..3 {
            let mut r = AuditRecord::new("operator", "test.event", &format!("s{i}"), None);
            store.append_audit(&mut r).unwrap();
            assert_eq!(r.seq, i + 1);
            assert_eq!(r.hash.len(), 64);
        }
        assert_eq!(store.verify_audit_chain().unwrap(), 3);

        {
            let conn = store.lock();
            conn.execute("UPDATE audit SET actor = 'mallory' WHERE seq = 2", [])
                .unwrap();
        }
        let err = store.verify_audit_chain().unwrap_err();
        assert!(
            matches!(err, StoreError::BrokenAuditChain { seq: 2, .. }),
            "{err}"
        );
    }
}
