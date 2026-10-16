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

- **Inbound**: `GET /_matrix/client/v3/sync?timeout=25s&since=…` long-poll;
  `m.room.message` events from joined rooms. `next_batch` persists to
  `meta["matrix.next_batch"]` **after** dispatch — at-least-once with
  token dedupe, mirroring the Telegram offset. A crash between the two
  replays the event.
- **Outbound**: `PUT …/rooms/<id>/send/m.room.message/<txn>`; replies go to
  the room the operator last wrote from; bodies over 3500 chars split on
  newline boundaries.
- **HTTP**: the client speaks plain HTTP/1.1 — point `homeserver` at a local
  homeserver or your TLS-terminating reverse proxy; it does not do TLS
  itself.
- **Approvals**: text commands (`/y`, `/n`) — the documented channel
  feature mismatch (no inline keyboards in this subset); the card carries
  the question, the expiry and the hint.

## Isolation and parity (exit criteria)

- `apps/ka-agent/tests/isolation_e2e.rs`: instance A works (turn, memory,
  grant); instance B — separate data root — sees no episode, no grant, no
  identity, no memory row of A's. Nothing crosses.
- `apps/ka-agent/tests/matrix_e2e.rs`: the same suite shape as Telegram —
  scripted turn + `next_batch` persistence across restart, guarded approval
  round-trip over `/y`, unlinked sender ignored and audited while the linked
  operator is served.
