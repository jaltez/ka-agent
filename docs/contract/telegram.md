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
  /reminders /cron /calendar /skills /skill /approvals /y /n /steer
  /cleanup /update` (the shared table's `telegram` flag); `/persona
  /effort /undo /queue /tools` also work from the phone but stay out of
  the menu, and `/quit` refuses from any remote channel.
- `/new private` starts an incognito conversation from the phone: the
  reply confirms it (`🕶 private session …`), inbound media lands under
  `attachments/.private-<short-id>/` instead of the shared inbox, and the
  runtime records usage but no text (see `memory.md`). The directory is
  destroyed when the conversation ends.
- `/start` from a linked chat is an alias of `/help` and ignores a
  deep-link payload: `/start <payload>` answers the command list (never
  `unknown command /<payload>`). From an **unlinked** private chat,
  `/start <payload>` redeems the payload as a pairing code — the one-tap
  form of `/pair` that `ka-agent pair telegram` and setup print as
  `https://t.me/<bot>?start=<code>` — with `/pair`'s exact semantics
  (10-min TTL, 5 attempts, constant-time compare). A wrong payload burns
  an attempt and is answered like a wrong `/pair` (nothing), never with
  the hourly owner hint; plain `/start` from an unlinked chat gets that
  hint at most once an hour. Any other `/word` gets
  `unknown command /word — /help lists them`. Free text expands
  `@references` and goes to `runtime.handle_message`.

- Native pickers: a **bare** `/model`, `/mode`, `/effort`, `/approvals`,
  `/queue`, `/forget`, `/skills` or `/history` (a lone word, `@botname`
  stripped like the router does) from a linked private chat answers
  with ONE card message carrying an inline keyboard instead of the
  router's text listing; a bare `/cleanup` is intercepted the same way
  but acts directly — it deletes the tracked messages and replies in
  plain text, no card. Every argument form (`/model <selector>`, `/mode
  <n>`, `/skills review`, `/history <query>`, …) still routes through
  the shared router unchanged. Sends/edits are best-effort: a refusal
  logs to stderr and drops the card. Callback data shapes (all ≤ 64
  bytes; unknown data under these prefixes — `mm:nope:1`, an
  out-of-range index — gets the empty answer and no state change, like
  any other unknown prefix):
  - `/model`: the shared `model_header` (current model + key state) plus
    `pick a provider:`, one button per provider of
    `Runtime::models_listing` — text `name (N)`, the current vendor
    dotted (`· anthropic (17)`), rows of 3, at most 24 per page with
    `‹ prev` / `… more` paging (`mp:<off>`, digits only). A `✕` button
    (`mx`) collapses the card to its `model: <current>` line.
  - `mp:<vendor>` swaps the card to that provider's models: the shared
    `model_vendor_listing` text capped at 12 lines over rows of 2 model
    buttons (12 per page) — button text is the bare model id truncated
    to 24 chars, `callback_data = mm:<vendor>:<idx>` where `<idx>`
    indexes the provider's **full** models list, so later pages still
    pick real models. `‹ prev` / `more ›` page via `mp:<vendor>:<off>`;
    `‹ providers` (`mp:0`) returns to the provider grid.
  - `mm:<vendor>:<idx>` picks `models[idx]` through `Runtime::set_model`:
    the toast says `model → <selector>` (or the error text), and the
    card settles to the same reply the router's switch produces
    (`model → … (next message uses it)` / `! [model] …`) with the
    keyboard removed.
  - `/mode`: `mode: <current>` plus one row per mode (`name — desc`,
    the desc capped at 20 chars, current dotted), `callback_data =
    mo:<mode>`. The press runs `Runtime::set_mode` with source
    `telegram:<uid>` exactly as `/mode <name>` would — the phone may
    only tighten; a loosening press toasts the refusal text and settles
    the card to `! [mode] …`.
  - `/effort`: `effort: <current>` (or `effort: model default`) plus one
    row of the five levels, `callback_data = ef:<level>`. The press runs
    `Runtime::set_effort` and settles the card to its reply with the
    keyboard removed.
  - `/approvals` (empty list → plain `no pending approvals`, no card):
    one line per pending approval (`[id8] question — expires in …`),
    one `✅ Allow` / `⛔ Deny` row each (`ap:<last-12-of-id>:y|n`, at
    most 10 rows, excess noted as `N more — /approvals lists them`),
    `✕` close. The press resolves the id exactly like the `a:` card and
    runs the same decision; the card settles to the outcome, and a
    dead-worker decision toasts `expired`.
  - `/queue` (empty → plain `the queue is empty`): one line per queued
    message (`#n [source] first-80-chars`), a `✕` row per item
    (`qx:<last-12-of-id>` → `Runtime::remove_queued`, toast `removed`)
    and a final `Clear all` row (`qclr` → `clear_queue`, the card
    settles to its reply).
  - `/forget` (empty → plain `nothing is remembered yet`): eight
    remembered facts per page (`[id8] first-80-chars`), a `✖` row per
    entry (`fx:<id8>` — prefix-resolved exactly like `/forget <id>`,
    ambiguous → toast `ambiguous id`, then `Runtime::forget`, toast
    `forgot <id8>`), `fp:<off>` paging, `✕` close.
  - `/skills` with pending proposals: one line per proposal and the
    **same** `sk:a:<id>` / `sk:r:<id>` rows the proposal cards use (the
    identical handler and data shape), at most 10 rows. With none
    pending the card lists the installed skills (the router's bare
    `/skills` text, capped 12 lines) over a lone `✕`.
  - `/history` (empty → plain `no history yet`): the `/history` line
    format (`[id8] HH:MM you: … → …`, 6 per page), a `▸` detail row per
    turn (`hv:<off>:<id8>` — the card edits to `> <inbound>` plus the
    answer, capped 12 lines, with a `‹ back → hp:<off>` row), `hp:<off>`
    paging, `✕` close. The generic `✕` (`x`) on any picker card
    collapses it to its first line.
- Callback queries are answered exactly once. Data this build does not
  understand (unknown prefix, malformed payload, unknown verdict) gets an
  empty `answerCallbackQuery` and no state change; an unauthorized
  callback also gets only an empty answer (it closes the spinner and
  reveals nothing), plus the audit row every unauthorized update gets.
- On startup (after the `getMe` token check) the channel registers the
  command list via `setMyCommands` so clients autocomplete it, and sets
  the bot profile via `setMyDescription` (the empty-chat "What can this
  bot do?" screen) and `setMyShortDescription` (the chat-list blurb);
  all best-effort — a refusal never disables the channel.
- Files/photos (SEED.md §5.4): the largest photo variant or the document
  is downloaded (`getFile` + `/file/bot<token>/<path>`, 20 MB cap) into
  `<workspace>/attachments/` — filenames sanitized to `[A-Za-z0-9._-]`
  (no leading dot, no `..` runs, ≤ 64 chars), collisions get `-N`
  suffixes, never overwrite. **Without a caption** the reply names the
  file as an `@attachments/…` reference the operator can paste into the
  next message (`@` expansion resolves against the workspace). **With a
  caption** the caption is the operator's message: it is submitted as a
  turn (👀 receipt, router, `@reference` expansion, engine) with every
  saved file riding along as `caption\n@attachments/…` refs — there is
  no bare ack to lose the question. A failed download is a visible
  `! [attachment] …` reply, never a silent drop.
- Albums (media groups): messages sharing a `media_group_id` are each
  saved at once, then answered **once** after a 750 ms debounce (the
  group map is in-memory; a restart re-delivers via the offset).
  Caption-less albums get one `saved N files — reference them with @…`
  ack naming every ref; an album with a caption on any member becomes
  one caption turn carrying all the refs.
- Unsupported media kinds (`video_note`, `video`, `animation`,
  `sticker`, `location`, `contact`) get an explicit
  `! [media] <kind> isn't supported yet` reply (human kind names, e.g.
  `video note`) — no turn, no 👀, never a silent drop.
- Accepted free text gets a `👀` reaction (`setMessageReaction`) at once —
  a receipt, kept after the answer (clearing costs a call per message).
- Engine errors render as structured cards, not raw dumps: bold
  `<emoji> <title>`, one explanation line, one remedy hint, and the
  first 300 characters of the raw message escaped in a `<code>` block
  (`errfmt::friendly_error`; card ≤ 500 chars by construction). When
  the error is **retryable** (the engine's `retryable` flag, surfaced on
  the `error` envelope) the card carries a `↻ Retry` inline button
  (`callback_data = retry:last`); pressing it answers the callback with
  a toast and re-submits the newest `error`/`interrupted` turn's stored
  text through `Runtime::retry_last` — never automatically. A press
  with nothing to retry answers `nothing to retry` and changes nothing.
  The sent card is tracked for cleanup: bare `/cleanup` deletes every
  tracked error card and notice (sequential, best-effort — Telegram
  refuses deletions past 48 h, which is skipped) and replies
  `removed N message(s)`; any **successful** turn
  (`turn_finished{stop:"done"|"length"}`) first deletes every card
  tracked before its start, so a failed attempt is cleaned up by its
  own successful retry. Tracking is in-memory (a restart forgets the
  ids; `/cleanup` then finds nothing).
  In-turn notes have the engine's local `(esc cancels)` hint stripped:
  Esc does nothing on the phone; `/stop` is the remote interrupt.
- `/update [check]`: release check and permission-gated self-update,
  through the same machinery `ka-agent update` uses (release metadata,
  checksum, atomic image swap, the release pinned to the one the daemon
  validated). Refused before anything downloads when a turn is in
  flight (`/stop` it first — an update restarts the daemon; a paused or
  degraded session has no live turn and may update) or when the daemon
  cannot write its own binary's directory (a system-wide install owned
  by root): the reply says so and names the manual path (`ka-agent
  update` on the machine). One install runs at a time. The install
  child never restarts services itself; afterwards this daemon stops
  itself — only when a supervisor owns it (so it comes back on the new
  binary) and no turn started meanwhile — otherwise it keeps running
  the old binary and the reply says exactly which case happened. The
  self-restart fires ~10 s after the reply, so the answer lands first.
  `check` only reports (`0.13.0 → 0.14.0 available — /update installs
  it`); the periodic discovery Notice and `/status` name `/update`.
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
- Final answer: the `final_chunk` envelope (carrying its `turn` id)
  sends one message per chunk (runtime chunks ≤3800 chars at paragraph
  boundaries); the surface closes unbalanced ``` fences per chunk and
  spaces sends ≥1 s (official per-chat guidance ~1/s). Every
  `sendMessage`/`sendMessageHTML` tolerates 429 flood control: sleep
  `retry_after` (clamped 1..=60 s) and retry exactly once — a second
  refusal returns the error. A final chunk that still cannot be sent
  after the retry is requeued in the outbox (`status = 'queued'`), so
  the startup drain redelivers it after the next restart (private
  sessions write no outbox rows — nothing to requeue). The plain-text
  fallback for an over-long render splits at paragraph boundaries
  (hard-break at 4000 chars) and sends the pieces sequentially; notices
  and error cards are bounded by construction and never split.
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
- Files the operator asks for (`AgentEvent::File`): jpg/jpeg/png/webp up
  to 10 MB go out as `sendPhoto`; mp4/m4v/webm/mkv and gif up to 50 MB go
  out inline as `sendVideo` / `sendAnimation` (multipart part `video` /
  `animation`, so they play in the chat instead of downloading);
  everything else — and any refused or over-limit inline send — falls
  back to `sendDocument`, so the file is never lost. Each file first
  fires one matching `sendChatAction` (`upload_photo` for the photo
  split, `upload_document` otherwise): the phone shows an upload
  indicator instead of "typing…" while a big file goes out. The tool cap
  is 50 MB, so uploads use a dedicated 10-minute HTTP timeout
  (`MEDIA_HTTP_TIMEOUT`, not the 40 s polling timeout). A caption longer
  than Telegram's 1024-character limit is truncated before the request
  to 1023 characters plus `…` — the file still goes out; an empty
  caption sends no caption field. A read error or a refused send falls
  back to a notice carrying the file's absolute path.
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

## Turn watchdog notices

The runtime's staged watchdog (`turn_watchdog.silent_secs`, default 180,
and `turn_watchdog.abort_secs`, default 900; 0 disables either stage,
`abort_secs` must exceed `silent_secs` when both are armed, hot-reload
`Apply::Live`) watches a turn that is open but silent — the engine is
alive and emitting nothing. At `silent_secs` of silence the phone gets
one `⏳ still working — no engine output for N min; /stop cancels` notice
(once per silent stretch; any engine event resets it), and at
`abort_secs` the turn is aborted exactly like `/stop`
(`⏱ no engine activity for N min — aborting; /retry re-sends it`). A
pending approval gate is legitimate blocking and never triggers either
stage.
