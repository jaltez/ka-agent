# Telegram channel contract — M2

Status: implemented, 2026-10-15, verified against a fake Bot API
(`ka_agent_channels::fake::FakeBotServer`) plus app-level e2e with the
scripted engine. Limits and semantics follow `docs/ka-interface-validation.md`
§4 (official Bot API pages, 2026-10-15).

## Inbound (long polling)

- `getUpdates` long poll (timeout 25 s, `allowed_updates = ["message","callback_query"]`).
- The offset persists in the store (`meta: telegram.offset`) and is advanced
  to `update_id + 1` **after** dispatch: at-least-once delivery, deduplicated
  by offset. A crash between dispatch and persist re-delivers the tail;
  consumers are idempotent (queue single-flight, store dedupe).
- Auth: private chats only, and `from.id` must be linked in the
  `identities` table (`ka-agent identity link telegram <id>`; the
  deprecated `telegram.allow_user_ids` migrates there on daemon start) —
  for messages AND callback queries. Unauthorized traffic gets no reply,
  one rate-limited stderr note per minute, and an audit row
  (`telegram:<uid>` / `telegram.unauthorized`).
- Commands route through the shared router (`commands::route`) and never
  reach the engine: `/start /help /status /new /stop /resume /remember
  /forget /memory /history /remind /cron /calendar /skills /y /n`; any
  other `/word` gets `unknown command: /word`. Free text expands
  `@references` and goes to `runtime.handle_message`.
- On startup (after the `getMe` token check) the channel registers the
  command list via `setMyCommands` so clients autocomplete it;
  best-effort — a refusal never disables the channel.
- Files/photos (SEED.md §5.4): the largest photo variant or the document
  is downloaded (`getFile` + `/file/bot<token>/<path>`, 20 MB cap) into
  `<workspace>/attachments/` — filenames sanitized to `[A-Za-z0-9._-]`
  (no leading dot, no `..` runs, ≤ 64 chars), collisions get `-N`
  suffixes, never overwrite. The reply names the file as an
  `@attachments/…` reference the operator can paste into the next
  message (`@` expansion resolves against the workspace). A failed
  download is a visible `! [attachment] …` reply, never a silent drop.
- Bad token (getMe 401/404): one `! [auth]` message to the known chat, then
  the channel exits (the daemon exits). Network errors: 1 s→60 s backoff.

## Outbound (TelegramSurface)

- No per-delta streaming: the delivery boundary is `idle` (SEED.md §3.7).
  While a turn runs: typing action every 4 s plus one editable progress
  message (edits throttled to 1 per 3 s, last 3 notes buffered).
- Final answer: `on_final_chunk` sends one message per chunk (runtime chunks
  ≤3800 chars at paragraph boundaries); the surface closes unbalanced ```
  fences per chunk and spaces sends ≥1 s (official per-chat guidance ~1/s).
- Approval cards (SEED.md §5.5): question + detail (first 20 lines) +
  expiry as a relative time (`expires in 14m` — wall-clock timezones are
  never guessed), inline keyboard `✅ Allow` / `⛔ Deny`,
  `callback_data = a:<last-12-of-id>:y|n`
  (≤64 bytes, asserted by test). Decisions answer the engine ask, audit, and
  edit the card to `[Approved]/[Denied]/[Expired]` with the keyboard removed.
  "Always" is never offered from Telegram (gap G12).
- Plain text only (no `parse_mode`) — engine markdown is never interpreted as
  Telegram entities.

## Secrets

`telegram.token_secret` resolves via `ka_agent_channels::secret`:
`$KA_AGENT_SECRET_<NAME>` first, then `<data>/secrets/<name>` (0600 file,
single line). The OS-keyring tier is a later addition; the lookup order is
fixed.
