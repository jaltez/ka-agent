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
| Start the daemon with the UI | `ka-agent run --web-port 8480` |
| Mint a login link | `ka-agent dashboard` (one-shot URL; respects the configured port) |

The port default is none (off). `ka-agent dashboard` mints a one-shot token in the
daemon's store (`meta` key `web_auth_token`) with a 10-minute TTL and prints
`http://127.0.0.1:<port>/auth?token=<token>`.

## Auth flow

1. `GET /auth?token=<token>` — validates and **consumes** the token (a second
   use is `401`), then answers `302` with
   `Set-Cookie: ka_session=<32 random bytes hex>; HttpOnly; SameSite=Strict;
   Path=/; Max-Age=86400` and `Location: /`.
2. Every other route requires a valid `ka_session` cookie; otherwise `401`.
3. Sessions live in the daemon process (memory only) and expire after 24 h;
   a daemon restart invalidates them.

## Routes

| Method | Path | Content |
|---|---|---|
| GET | `/healthz` | `ok` (unauthenticated liveness) |
| GET | `/auth?token=…` | login, sets the cookie |
| GET | `/` | chat: bubbles (last 20 turns), SSE live updates, Markdown rendered |
| GET | `/usage` | spend per day (30 d), CSS bars, today vs the cap |
| POST | `/settings` | the status page form (model, mode, timezone, daily budget): format-preserving writes + live reload; invalid values are refused |
| GET | `/history` | search form; `?q=` runs FTS over episodes |
| GET | `/history/show/<turn>` | one turn: inbound text + the outbound answer |
| GET | `/memory` | report (bytes/budget), proposal form, the review inbox (candidates with diffs), accepted entries, compiled text |
| POST | `/memory/edit` | stages a candidate (`text=`) → `303` back to `/memory` |
| POST | `/memory/candidate` | `id=` + `decision=y\|n` applies a review decision → `303` |
| GET | `/approvals` | pending asks with allow/deny forms |
| POST | `/approvals/decide` | `id=` + `decision=y\|n` → `302 /approvals` |
| GET | `/audit` | the last 50 audit-chain records (seq, time, actor, action, subject) |
| GET | `/intents` | the last 20 MCP intents and their status |

Anything else is `404` — including `GET` on a mutation path, so no page can
be triggered by a link.

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

Not covered by the UI on purpose: chat (the console and Telegram remain the
chat surfaces) and any write beyond memory proposals and approval decisions.

## Verification

- `crates/ka-agent-web/tests/http_units.rs` — escaping and URL decoding,
  including malformed percent escapes.
- `apps/ka-agent/tests/web_e2e.rs` — the real binary over real sockets:
  unauthenticated `401` on every page, login → cookie flags → page matrix,
  single-use token replay, POST-only mutations, loopback-only bind, the
  memory editor round-trip, XSS escaping of seeded turn text, and refusal of
  an oversized declared `Content-Length` (no unbounded buffering).

An approval decision only lands when the live engine still has the ask
pending (`Runtime::decide` → `UnknownApproval` otherwise); the e2e test
asserts that an undeliverable decision renders a page and leaves the row
untouched. The full approve/deny round-trip against a live engine is covered
by `chat_e2e.rs` (approval flow) — the web UI calls the same
`Runtime::decide`.
