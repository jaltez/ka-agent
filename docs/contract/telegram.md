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
- Auth: private chats only, and `from.id` must be in `telegram.allow_user_ids`
  — for messages AND callback queries. Unauthorized traffic gets no reply,
  one rate-limited stderr note per minute, and an audit row
  (`telegram:<uid>` / `telegram.unauthorized`).
- Commands route locally and never reach the engine: `/start`, `/help`,
  `/status`, `/new`, `/stop`; any other `/word` gets `unknown command: /word`.
  Plain text goes to `runtime.handle_message(text, "telegram:<uid>")`.
- Files/photos: text-only in M2 (`file uploads arrive in the next milestone`).
- Bad token (getMe 401/404): one `! [auth]` message to the known chat, then
  the channel exits (the daemon exits). Network errors: 1 s→60 s backoff.

## Outbound (TelegramSurface)

- No per-delta streaming: the delivery boundary is `idle` (SEED.md §3.7).
  While a turn runs: typing action every 4 s plus one editable progress
  message (edits throttled to 1 per 3 s, last 3 notes buffered).
- Final answer: `on_final_chunk` sends one message per chunk (runtime chunks
  ≤3800 chars at paragraph boundaries); the surface closes unbalanced ```
  fences per chunk and spaces sends ≥1 s (official per-chat guidance ~1/s).
- Approval cards (SEED.md §5.5): question + detail (first 20 lines) + expiry,
  inline keyboard `✅ Allow` / `⛔ Deny`, `callback_data = a:<last-12-of-id>:y|n`
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
