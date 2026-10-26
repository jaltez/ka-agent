//! The Matrix channel (SEED.md §10 Phase 6): the second adapter proving the
//! channel trait. Client-Server API v3 subset — `GET /sync` long-poll in,
//! `PUT …/send` out, `POST …/join` for invites — against one homeserver
//! with one bot account, over `reqwest` (http and https).
//!
//! Identity: inbound senders gate on the store's `identities` table as
//! `matrix:<user_id>` (§8.1); anything else is audited and ignored, except a
//! `/pair <code>` message which redeems a one-time pairing code (§5.2).
//!
//! Outbound: operator turns stream as one message edited with `m.replace`
//! (first draft after 80 characters or 2 s, then one edit every 2 s with
//! 80 new characters), with `m.typing` while the turn runs; scheduled turns
//! (`cron`/`briefing`) arrive as one message at the end. Approval cards stay
//! text (`/y`/`/n`) and also accept a ✅/❌ `m.annotation` reaction. Inbound
//! `m.image`/`m.file`/`m.video` land in `<workspace>/attachments`, and
//! `AgentEvent::File` is uploaded and posted as media.
//!
//! Delivery is at-least-once with `next_batch` dedupe, mirroring the
//! Telegram offset: events are dispatched first, then `matrix.next_batch`
//! is persisted; a crash between the two replays the event. Outbound
//! messages go through a dedicated sender task, so a reply never waits
//! behind the `/sync` long-poll.

//! Split from matrix.rs (moves only).

pub(crate) mod channel;
pub(crate) mod client;
pub(crate) mod surface;

pub use channel::MatrixChannel;
pub use client::login;
pub use surface::MatrixSurface;

pub struct MatrixConfig {
    pub homeserver: String,
    pub user_id: String,
    pub token: String,
    /// Voice-message transcription (`m.audio`); `None` answers them with
    /// the setup hint.
    pub transcriber: Option<crate::voice::Transcriber>,
}

/// Long-poll window for `/sync` (the daemon's shutdown latency bound).
const SYNC_TIMEOUT_SECS: u64 = 25;
/// Matrix event bodies cap far above this; keep replies short and split.
const REPLY_CAP: usize = 3500;
/// Outbound messages held while no room is known; the oldest drop first.
const BACKLOG_CAP: usize = 200;
/// On the very first sync, timeline events older than boot minus this are
/// history, not new messages.
const FIRST_SYNC_GRACE_MS: i64 = 60_000;
const NEXT_BATCH_KEY: &str = "matrix.next_batch";
const ROOM_KEY: &str = "matrix.room_id";
const ENCRYPTED_NOTICE: &str = "This room is end-to-end encrypted and Ka Agent can't read it yet — talk to me in an unencrypted room or DM.";
const PAIRED_REPLY: &str = "✓ Paired. I only answer you from now on.";
