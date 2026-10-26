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
  for messages AND callback queries. Unauthorized updates get no text
  reply (an unauthorized callback query gets an empty answer, which only
  closes its spinner), one rate-limited stderr note per minute, and an
  audit row (`telegram:<uid>` / `telegram.unauthorized`).
- Commands route through the shared router (`commands::route`) and never
  reach the engine. The bot-command menu registers `/help /status /new
  /stop /resume /model /mode /remember /forget /memory /history /remind
  /reminders /cron /calendar /skills /skill /approvals /y /n /steer`
  (the shared table's `telegram` flag); `/persona /effort /undo /queue
  /tools` also work from the phone but stay out of the menu, and `/quit`
  refuses from any remote channel. `/start` is an alias of `/help` and
  ignores a deep-link payload: `/start <payload>` answers the command
  list (never `unknown command /<payload>`). Any other `/word` gets
  `unknown command /word — /help lists them`. Free text expands
  `@references` and goes to `runtime.handle_message`.
- Callback queries are answered exactly once. Data this build does not
  understand (unknown prefix, malformed payload, unknown verdict) gets an
  empty `answerCallbackQuery` and no state change; an unauthorized
  callback also gets only an empty answer (it closes the spinner and
  reveals nothing), plus the audit row every unauthorized update gets.
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
- Accepted free text gets a `👀` reaction (`setMessageReaction`) at once —
  a receipt, kept after the answer (clearing costs a call per message).
- Voice notes / audio (`voice`, `audio`): with `[voice]` configured the file
  is downloaded (20 MB / 10 min caps), sent to the OpenAI-compatible
  `/audio/transcriptions` endpoint, echoed back as `🎙 “…”`, and handled
  exactly like typed text. Without `[voice]`: one reply naming
  `ka-agent setup voice`, nothing submitted. Failures are a visible
  `! [voice] …`.
- Callback `m:<short>:y|n` settles a memory proposal card (below).
- Bad token (getMe 401/404): one `! [auth]` message to the known chat, then
  the channel exits (the daemon exits). Network errors: 1 s→60 s backoff.

## Outbound (TelegramSurface)

- Streaming (`telegram.stream`, default on): deltas accumulate per turn
  and are pushed with Bot API 9.5 `sendMessageDraft` (stable non-zero
  `draft_id` per turn, plain text, ≤ 4095 characters, ≤ 1 request/s, only
  after ≥ 40 new characters, one request in flight). The throttle
  compares the **truncated** draft length, so text growing only past the
  cap never re-sends an identical draft. Refusals are classified by the
  Bot API `error_code` and `parameters.retry_after`, which the client
  keeps on the error: **429** (flood control) is transient — no draft
  request until `retry_after` elapses and streaming stays on;
  **400/404** (bad request / method not found, i.e. a server that predates
  drafts) turns drafts off for the process and the turn still ends with
  the final message; **any other code** is logged and leaves the flag
  unchanged. The `telegram.stream` key has `Apply::Restart`, so the off
  switch lasts until the daemon restarts. The delivery boundary stays
  `idle` (SEED.md §3.7): the final answer is always sent as normal
  messages.
  While a turn runs: typing action every 4 s plus one editable progress
  message (edits throttled to 1 per 3 s, last 3 notes buffered).
- Final answer: the `final_chunk` envelope sends one message per chunk (runtime chunks
  ≤3800 chars at paragraph boundaries); the surface closes unbalanced ```
  fences per chunk and spaces sends ≥1 s (official per-chat guidance ~1/s).
- Approval cards (SEED.md §5.5): question + detail (first 20 lines) +
  expiry as a relative time (`expires in 14m` — wall-clock timezones are
  never guessed), inline keyboard `✅ Allow` / `⛔ Deny`,
  `callback_data = a:<last-12-of-id>:y|n`
  (≤64 bytes, asserted by test). Decisions answer the engine ask, audit, and
  edit the card to `[Approved]/[Denied]/[Expired]` with the keyboard removed.
  "Always" is never offered from Telegram (gap G12).
- Formatting: answers are rendered by `ka_agent_core::markdown` (Telegram
  HTML subset, everything escaped first) and sent with `parse_mode: HTML`;
  a refusal (`can't parse entities`) or an over-long render falls back to
  the plain text.
- Memory proposal cards: each new `memory_propose` candidate (found by the
  runtime's maintenance tick, at most 3 per tick plus a "N more" notice) is
  posted with `✅ Remember` / `✖ Discard` (`callback_data =
  m:<short-id>:y|n`); the press accepts/rejects it and edits the card to
  its outcome with the keyboard removed.
- Draft labels (plan 3.3): while tools run and no answer text has streamed,
  the draft shows `⋯ <tool label>` (the same `tool_label` wording every
  surface uses) through the same 1 s / growth throttle as answer text; the
  first streamed delta replaces it. Scheduled turns (`cron`, `cron:*`,
  `briefing`) never open a draft.
- Files the operator asks for (`AgentEvent::File`): jpg/jpeg/png/webp up to
  10 MB go out as `sendPhoto` (multipart with the caption), everything else
  as `sendDocument`. The tool cap is 50 MB, so uploads use a dedicated
  10-minute HTTP timeout (`MEDIA_HTTP_TIMEOUT`, not the 40 s polling
  timeout). A caption longer than Telegram's 1024-character limit is
  truncated before the request to 1023 characters plus `…` — the file
  still goes out; an empty caption sends no caption field. A read error or
  a refused send falls back to a notice carrying the file's absolute path.
- Skill proposals: `skill_propose` posts a card with `✅ Accept` /
  `✖ Reject` (`callback_data = sk:a|r:<id>`, verdict first, then the full
  id or its short suffix; ≤ 64 bytes), routed through the same
  `/skills accept|reject <id>` command path as the console; the press
  installs (or discards) and edits the card to the outcome. A second press
  of the same card (a double tap racing the edit) finds no pending
  proposal: it answers `already installed` / `already rejected` and leaves
  the card's outcome untouched.

## Secrets

`telegram.token_secret` resolves via `ka_agent_core::secret`:
`$KA_AGENT_SECRET_<NAME>` first, then `<data>/secrets/<name>` (0600 file,
single line). The OS-keyring tier is a later addition; the lookup order is
fixed.
