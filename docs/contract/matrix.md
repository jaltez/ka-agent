# Matrix channel + identity linking contract (Phase 6)

## Identities (SEED.md §8.1)

Exactly one operator per instance (ADR-0001). The `identities` table
(schema v3) links channel identities to that operator:

```
$ ka-agent identity link telegram 12345
$ ka-agent identity link matrix @javier:example.org
$ ka-agent identity list
  telegram:12345            via cli              at 12:01
  matrix:@javier:…          via cli              at 12:02
```

- Pairing is **CLI-only** and audited (`identity.link`, `identity.unlink`).
- Every channel gates inbound traffic on this table: `telegram:<uid>`,
  `matrix:<user_id>`. Anything else is audited
  (`telegram.unauthorized`, `matrix.unauthorized`) and **never answered**.
- The legacy `[telegram] allow_user_ids` is a deprecated migration source:
  the daemon links each entry at startup (provenance `config-migration`,
  idempotent) and tells the operator to remove the key.
- Unlinking is immediate policy; a channel stops answering that identity on
  the next message.

## The channel trait (`channels::Channel`)

`fn name() -> &'static str` + `fn run(shutdown) -> boxed future`. Telegram
and Matrix implement it; the daemon spawns one task per enabled channel over
the same runtime and store. Outbound runtime traffic (notes, answers,
approval cards, errors) fans out through `MultiSurface` to every enabled
channel surface, so the operator sees it wherever they are; each channel
also sends its own direct command replies through its transport.

Command semantics are shared (`channels::commands`): every channel parses
the same `/commands` — including `/y`/`/n` text approvals — and returns the
same replies. Only the transport differs.

## Matrix (CS API v3 subset)

Config:

```toml
[matrix]
enabled = true
homeserver = "http://127.0.0.1:8008"   # local reverse proxy or homeserver
user_id = "@ka-agent:example.org"      # the bot account; own events skipped
token_secret = "matrix.access_token"   # KA_AGENT_SECRET_MATRIX_ACCESS_TOKEN
                                        # or <data>/secrets/matrix.access_token
```

- **Inbound**: `GET /_matrix/client/v3/sync?timeout=25000&since=…` long-poll
  (milliseconds, matching `SYNC_TIMEOUT_SECS = 25`);
  `m.room.message` events from joined rooms. `next_batch` persists to
  `meta["matrix.next_batch"]` **after** dispatch — at-least-once with
  token dedupe, mirroring the Telegram offset. A crash between the two
  replays the event.
- **Outbound**: `PUT …/rooms/<id>/send/m.room.message/<txn>`; replies go to
  the room the operator last wrote from; bodies over 3500 bytes split on
  newline boundaries.
- **Streaming**: on an operator turn the reply streams as one message
  edited in place. All thresholds count **bytes** (`String::len`), not
  characters. The first draft is sent once 80 bytes accumulated or 2 s
  passed since the first delta; further `m.replace` edits
  (`m.new_content` + `m.relates_to: {rel_type: "m.replace", event_id}`)
  follow every 2 s with at least 80 new bytes. The final edit carries the
  shared renderer's `formatted_body` (HTML). Scheduled turns (`cron`,
  `briefing`) are not streamed: their text arrives once, at the end.
  A failed draft send backs off one interval instead of retrying in a
  tight loop. If the final chunk lands while the first draft is still in
  flight, settlement waits for that send's event id and edits the draft
  (never a second message); if the homeserver refuses the final edit, the
  full text is posted as its own message so the answer is not truncated.
- **Room pinning**: the first send pins the turn to one room; its edits,
  final edit and typing stay there even if the operator writes from another
  linked room meanwhile.
- **Typing**: `PUT …/rooms/<id>/typing/<user>` with `timeout: 30000` when an
  operator turn starts, refreshed while it runs; finishing the turn
  (`TurnFinished`) and the final delivery clear it (`timeout: 0`) in the
  pinned room.
- **Reminders**: `AgentEvent::Reminder` is posted as its own message,
  `⏰ <text>` (the same prefix the console and terminal use).
- **Attachments in**: `m.image`/`m.file`/`m.video` download through
  `/_matrix/client/v1/media/download/<server>/<id>` (falling back to
  `/_matrix/media/v3/download/…` on 404) into `<workspace>/attachments/`,
  and the room gets the `@attachments/<file>` reference to paste back.
  The operator's `body` is the filename when usable; otherwise one is
  minted from the message type and mxc id. Both are sanitized
  (`[A-Za-z0-9._-]`, no leading dot, ≤ 64 bytes), so an mxc id tail cannot
  steer the write outside the inbox; collisions get a `-N` suffix and are
  created exclusively (never overwrite, even for concurrent saves).
- **Attachments out**: `AgentEvent::File` uploads via
  `POST /_matrix/media/v3/upload` and posts `m.image`/`m.file` with the
  caption as the body. The 50 MB size limit is checked from the file's
  metadata before it is read; the caption is capped to one message body
  (3500 bytes — media bodies cannot split like a reply).
- **HTTP**: the client speaks plain HTTP/1.1 — point `homeserver` at a local
  homeserver or your TLS-terminating reverse proxy; it does not do TLS
  itself.
- **Approvals**: text commands (`/y`, `/n`) and a ✅/❌ `m.annotation`
  reaction on the card message (a bounded event-id → approval map resolves
  it). Both decide through the same runtime call.

## Isolation and parity (exit criteria)

- `apps/ka-agent/tests/isolation_e2e.rs`: instance A works (turn, memory,
  grant); instance B — separate data root — sees no episode, no grant, no
  identity, no memory row of A's. Nothing crosses.
- `apps/ka-agent/tests/matrix_e2e.rs`: the same suite shape as Telegram —
  scripted turn + `next_batch` persistence across restart, ask-mode
  approval round-trip over `/y`, unlinked sender ignored and audited while
  the linked operator is served.
