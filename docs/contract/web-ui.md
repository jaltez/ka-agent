# Web UI contract (M7, SEED.md §10 Phase 4; redesigned v0.8)

A loopback-only HTTP/1.1 server inside the daemon (`ka-agent-web`), serving
server-rendered HTML plus one embedded stylesheet (no external assets, no
CDN, no asset pipeline; light/dark via `prefers-color-scheme`). The chat
page loads `/static/chat.js` — the only script, same-origin under a strict
CSP — for SSE streaming; it builds DOM nodes only (never `innerHTML`), so
model Markdown can never become markup. Answers are rendered Markdown: the
same subset as every other surface (bold, `code`, http(s) links, bullets,
headings, fences), server-side for history and client-side for live turns
(streamed raw, redrawn rendered when the turn settles).

## Enabling it

| Step | Command |
|---|---|
| Start the daemon (UI on by default) | `ka-agent run` |
| Override the port for one run | `ka-agent run --web-port 8480` |
| Turn the UI off | `ka-agent config set web.enabled false` |
| Mint a login link | `ka-agent dashboard` (one-shot URL; respects the configured port) |

`[web] enabled` defaults to `true` and `port` to `8480`, so a plain
`ka-agent run` serves the panel on `127.0.0.1:8480`; `--web-port` overrides
the port for that run and `web.enabled = false` disables it. `ka-agent
dashboard` mints a one-shot token in the daemon's store (`meta` key
`web_auth_token`) with a 10-minute TTL and prints
`http://127.0.0.1:<port>/auth?token=<token>`.

## Auth flow

1. `GET /auth?token=<token>` — validates and **consumes** the token (a second
   use is `401`), then answers `303` with
   `Set-Cookie: ka_session=<32 random bytes hex>; HttpOnly; SameSite=Strict;
   Path=/; Max-Age=86400` and `Location: /`.
2. Every other route requires a valid `ka_session` cookie; otherwise `401`.
3. Sessions live in the daemon process (memory only) and expire after 24 h;
   a daemon restart invalidates them.

## Routes

Everything except `/healthz`, `/auth` and the bearer-token `/api/*` group
requires a valid session cookie.

| Method | Path | Content |
|---|---|---|
| GET | `/healthz` | `ok` (unauthenticated liveness) |
| GET | `/auth?token=…` | login, sets the cookie |
| GET | `/` | chat: bubbles (last 20 turns, a first-visit hint when there are none), SSE live updates, Markdown rendered; the composer carries a `🕶` button that starts a private conversation (`/new private`) |
| GET | `/static/chat.js` | the one script (same-origin, CSP-gated, `no-store`) |
| POST | `/chat/send` | a chat message or command (`text=`; requires the `X-Ka-Agent: 1` header, so only the panel's own JS can post) → JSON `{"reply"}` or `{"ok":true}` |
| GET | `/events` | the SSE stream (one `data:` frame per sequenced bus envelope; `id:` carries the `seq` so a reconnect with `Last-Event-ID` replays exactly the gap) |
| POST | `/api/notify` | `{"text"}` → a notice on every surface. **Bearer-only**: `Authorization: Bearer <web.api_token>`; `404` while no token is configured, `401` for a wrong one; 16 KiB bodies, 30 req/min |
| POST | `/api/ask` | `{"text","background","wait"}` → queues a turn (`api` / `api-bg` source); with `wait:true` the reply is held until the turn settles (300 s cap) and carries `{"ok","queued","reply","stop"}`; same bearer auth as `/api/notify` |
| GET | `/status` | the operator report (state, model, today vs budget, queue/approvals, memory, worker) plus the settings form |
| POST | `/settings` | the status page form, one control per `config_keys::KEYS` entry marked `web` (see below): format-preserving writes + live reload; each key shows when it applies; unknown keys are ignored; invalid values are refused |
| GET | `/usage` | spend per day **and model** (30 d), CSS bars, today vs the cap |
| GET | `/tasks` | reminders and crons, next fire first; each reminder row has a snooze form, every row a cancel button |
| POST | `/tasks/cancel` | `id=` (prefix ok) cancels a reminder or cron → `303 /tasks`; unknown/ambiguous ids render an explaining page |
| POST | `/tasks/snooze` | `id=` + `when=` (the `/remind` grammar: `30m`, `18:30`, `tomorrow 9am`, …) moves one reminder's next fire → `303 /tasks`; crons keep their cadence |
| GET | `/skills` | the skill library with enable/disable per skill |
| POST | `/skills/toggle` | `name=` + `enable=1\|0` → `303 /skills` |
| GET | `/contacts` | linked identities + unknown contacts with their `ka-agent identity link` command |
| GET | `/history` | search form; `?q=` runs FTS over episodes (whitespace-separated terms AND together; a quoted span stays one phrase) |
| GET | `/history/show/<turn>` | one turn: inbound text + the outbound answer |
| GET | `/memory` | report (bytes/budget), proposal form, the review inbox (candidates with diffs), accepted entries, compiled text |
| POST | `/memory/edit` | stages a candidate (`text=`) → `303` back to `/memory` |
| POST | `/memory/candidate` | `id=` + `decision=y\|n` applies a review decision → `303` |
| POST | `/memory/forget` | `id=` removes one accepted entry → `303 /memory`; an unknown id renders an error page instead of a silent redirect |
| GET | `/approvals` | pending asks with allow/deny forms |
| POST | `/approvals/decide` | `id=` + `decision=y\|n` → `303 /approvals` (JSON when the `X-Ka-Agent` header is present — the inline chat card's fetch path) |
| GET | `/audit` | the last 50 audit-chain records (seq, time, actor, action, subject) |
| GET | `/intents` | the last 20 MCP intents and their status |

Every page above renders inside the shared sidebar navigation (the chat,
history, audit and intents pages included), whose Approvals and Memory
items carry live badges: `chat.js` bumps them from the SSE stream so they
stay current without a reload.

Anything else is `404` — including `GET` on a mutation path, so no page
can be triggered by a link.

## Settings and live rows (v0.11)

The settings form is generated from the config key registry (`plan 1.5`):
one control per `KEYS` entry marked `web: true` — currently the model
cluster (`model`, `fallback`, `effort`, `background_model`), behavior
(`sandbox`, `busy_messages`, `mode`, `approval_ttl_secs`,
`worker_idle_timeout_secs`), budgets (`budgets.daily_usd`,
`budgets.daily_tokens`), `timezone`, `calendar.digest_time`,
`search.provider`, `notify.quiet_hours`, `approvals.auto_review`, the
`grants` trio and `language`; the list follows the registry, so a key
newly marked `web` appears here without further changes. The label is the
TOML path, the control follows the key's kind (enum and bool become
selects, numbers and strings inputs, lists accept comma-separated words),
the value is the file's current value, and the apply column says `now` /
`next turn` / `restart`.

A POST is validated against the same registry before anything is written:

- `int` keys must parse and fall inside the declared `min..=max` (a
  browser-side `min`/`max` mirrors the bound, but the server enforces it),
  `float` keys must be finite and `>= min`, enum keys must be members, and
  bools must be exactly `true` or `false`.
- `model`, `background_model` and every `fallback` entry go through
  `ka_agent_port::catalog::validate_selector`, the check the CLI's
  `config set` uses, so an unknown selector is refused here too.
- Every field is validated first; if any is refused, the response names the
  key and says `refused, nothing was written` — no key of that submission is
  persisted.

Only after the whole form passes is each value applied with
`config_file::set` and the live-reload path, so a saved key takes effect
without a restart when its registered timing allows it.

The chat stream renders tool activity as muted rows inside the turn's
bubble — `▸ <tool> <detail>` while the call runs, replaced by
`✓ <tool> <ms>ms` (or `✗ <tool> <error>`) when it finishes — matching the
terminal client's wording. Rows are built with DOM nodes, never
`innerHTML`.

## Security checklist (SEED §10 exit criteria)

Reviewed 2026-10-16 (M7.1) by a read-only security pass over the HTTP
surface; every finding fixed and covered by tests:

- [x] **Loopback only.** The bind address is the literal `127.0.0.1`; there is
      no configuration that widens it. The e2e test reads the listening
      socket from `/proc/net/tcp` and asserts `0100007F` (127.0.0.1).
- [x] **Auth on every page.** Only `/auth` and `/healthz` are reachable
      without a session cookie; the e2e test asserts `401` on all six pages.
- [x] **Single-use login token.** Consumed on first use; replay is `401`.
- [x] **Cookie flags.** `HttpOnly`, `SameSite=Strict`, `Path=/`, `Max-Age`.
- [x] **CSRF.** Mutations are POST-only, the cookie is `SameSite=Strict`, and
      cookies are the only credential — no cross-site form can carry them.
- [x] **XSS.** Every user-derived string goes through `escape_html`
      (`&`, `<`, `>`, `"`, `'`). The e2e test seeds a turn containing
      `<script>` and an event handler and asserts the page contains the
      escaped entity and no raw tag.
- [x] **No external surface.** No CDN, no external asset, no outbound request
      is made by the UI; the responses are inline HTML + a small inline
      stylesheet.
- [x] **Parser hardening.** One `Content-Length` per request, validated as
      `u64` and capped before any arithmetic (an overflowing value is
      refused, never sliced); the head+body phase runs under one 30 s
      deadline, so a dribbling connection cannot hold a task; concurrent
      connections are capped at 64 (beyond that the connection is dropped
      without being read). Covered by `malformed_requests_do_not_kill_the_server`.
- [x] **Single-use token under concurrent redemption.** The token is
      consumed by a store-side compare-and-swap (`Store::consume_meta`):
      exactly one racing `GET /auth` wins.
- [x] **Constant-time token compare.** The stored token is compared in
      constant time; `/auth` timing leaks no prefix.
- [x] **No privilege beyond the daemon.** The UI reads the same store the
      daemon owns and calls the same runtime methods the console and Telegram
      surfaces call (`remember`, `decide`, `history_search`, `memory_report`).

## Telegram parity (SEED §10 exit criterion)

The approvals inbox matches the Telegram surface: same source
(`Approval::kind` → `KaAsk`), the question, up to 20 detail lines, the
`expires HH:MM` footer, and the same two decisions (Allow/Deny), applied
through the same `Runtime::decide`. Both surfaces are per-session, so an ask
raised by the live engine appears in both; neither can settle an ask the
engine no longer holds.

## Verification

- `crates/ka-agent-web/tests/http_units.rs` — escaping and URL decoding,
  including malformed percent escapes.
- `apps/ka-agent/tests/web_e2e.rs` — the real binary over real sockets:
  unauthenticated `401` on every page, login → cookie flags → page matrix,
  single-use token replay, POST-only mutations, loopback-only bind, the
  memory editor round-trip, XSS escaping of seeded turn text, refusal of an
  oversized declared `Content-Length` (no unbounded buffering), settings
  refusals for an out-of-range `approval_ttl_secs` and an unknown model
  selector (with the config file left untouched), an escaped persona name
  in the page title, the sidebar on every page (history/audit/intents
  included), the first-visit chat hint, AND-ing history search, the
  per-model usage split, the forget-unknown-id error page, reminder
  snooze, and the `final_chunk` settle on the SSE stream.

An approval decision only lands when the live engine still has the ask
pending (`Runtime::decide` → `UnknownApproval` otherwise); the e2e test
asserts that an undeliverable decision renders a page and leaves the row
untouched. The full approve/deny round-trip against a live engine is covered
by `chat_e2e.rs` (approval flow) — the web UI calls the same
`Runtime::decide`.
