# Web UI contract

## Binding and the public URL

`[web] bind` is an IP (default `127.0.0.1`; IPv6 literals work) and
`[web] public_url` is the absolute http(s) base printed in login/pairing
links. A wildcard bind is not a URL: the local URL stays `127.0.0.1`.
`ka-agent setup web` writes both; `doctor` warns when the panel is bound
beyond loopback (pairing codes travel over it — put TLS/reverse proxy in
front, `docs/design/web-relay.md`).

## Roles (ADR-0002)

- `ka-agent pair web --role <r>` → a one-time code; `POST /pair` redeems
  it and sets a server-side `HttpOnly; SameSite=Strict` session cookie for
  a fresh `web:<id>` identity with that role.
- A role session is **chat-only**: `/`, `/chat/send`, `/events` and the
  static chat script. Every other page answers `403 this web session is
  chat-only`. `/events` is filtered to envelopes stamped with the role's
  own `web:<id>` source, so approval cards and other roles' turns never
  reach it.
- The operator's one-shot login (`/auth?token=…`) keeps the full panel:
  status, profiles (`/profiles`: profiles on disk, configured roles,
  linked identities with role reassignment), config form (`/settings`),
  tasks, memory, skills, approvals, audit.
- `POST /profiles/create|use` and `POST /identities/role` run the same
  CLI/store operations as the terminal and are audited with the admin
  role; they are not offered under a pinned `KA_AGENT_DATA_DIR`.

_M7, SEED.md §10 Phase 4; redesigned v0.8._

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
   Path=/; Max-Age=86400` (`; Secure` when `[web] public_url` is https) and
   `Location: /`.
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
| GET | `/static/chat.js` | the chat script (same-origin, CSP-gated, `no-store`) |
| GET | `/static/ui.js` | the UI script: theme preference (system/light/dark via `localStorage` + `data-theme`), loaded on every page |
| POST | `/chat/send` | a chat message or command (`text=`; requires the `X-Ka-Agent: 1` header, so only the panel's own JS can post) → JSON `{"reply"}` or `{"ok":true}` |
| GET | `/events` | the SSE stream (one `data:` frame per sequenced bus envelope; `id:` carries the `seq` so a reconnect with `Last-Event-ID` replays exactly the gap) |
| POST | `/api/notify` | `{"text"}` → a notice on every surface. **Bearer-only**: `Authorization: Bearer <web.api_token>`; `404` while no token is configured, `401` for a wrong one; 16 KiB bodies, 30 req/min |
| POST | `/api/ask` | `{"text","background","wait"}` → queues a turn (`api` / `api-bg` source); with `wait:true` the reply is held until the turn settles (300 s cap) and carries `{"ok","queued","reply","stop"}`; same bearer auth as `/api/notify` |
| GET | `/pair` | the public pairing form (a code from `ka-agent pair web --role <r>`); `POST /pair` redeems it and sets the role cookie |
| GET | `/sessions` | recent sessions by role with an "open" action (adopts that role's latest conversation; a running turn is never disturbed) |
| POST | `/sessions/open` | `role=` switches the active role when settled |
| POST | `/backup` | run the binary's own `backup` (default directory, pruned to 7) and show its output; restore stays in the CLI |
| GET | `/status` | the operator report (state, model, mode/effort, budget bars, queue/approvals, memory, worker, role/tools, worker stderr tail) plus the settings form |
| GET | `/profiles` | profiles on disk, configured `[[role]]` entries (each links to the editor), linked identities with role/link/unlink, the `[tools] disable` baseline, and the “new web user” code form |
| GET | `/roles/new` · `/roles/edit/<name>` | the role editor (name, mode, allow/disable, workspace, budget, commands) |
| POST | `/roles/save` · `/roles/delete` | write `[[role]]` through the validated config editor and hot-reload; delete refuses while identities/crons reference the role unless `force=1` |
| POST | `/tools/disable` | write the profile-wide `[tools] disable` baseline |
| POST | `/identities/link` · `/identities/unlink` | bind/unbind a channel identity and its role (audited) |
| POST | `/identities/role` | reassign one identity's role (validated against the config, audited) |
| POST | `/profiles/create` · `/profiles/use` | run the profile CLI and reload the page (refused under a pinned data root) |
| POST | `/profiles/remove` | the same with `--yes`; the form requires the profile name typed back |
| GET | `/models` | current model + key state, mode/effort pickers, the persona editor, and the provider catalog with one-click switching |
| POST | `/models/set` · `/models/mode` · `/models/effort` · `/persona` | apply the change and show the agent's own reply (errors keep the page with the reason) |
| GET | `/config` | the config file path, the safe (web-flagged) editor, every registered key with kind/apply/default/current, and the raw TOML |
| GET | `/queue` | messages waiting for the next turn, with remove/clear |
| POST | `/queue/remove` · `/queue/clear` | drop one queued message by id, or the whole queue |
| GET | `/grants` | scoped tool grants (`ka-agent grant list`) with add/remove |
| POST | `/grants/add` · `/grants/rm` | update the store's grant list → `303 /grants` |
| POST | `/keys/provider` · `/keys/secret` · `/keys/secret/rm` | write one provider key (engine `.env`) or secret file (0600), or remove a secret; values are never echoed |
| GET | `/keys` | provider key and secret **names** with presence, the current model's key state, the fix (`ka-agent key set …`), and the set forms (values are written 0600 and never rendered or audited) |
| GET | `/extensions` | MCP servers (add/remove below), plugins, hooks and the live tool inventory (plugin install stays in the CLI) |
| POST | `/mcp/add` · `/mcp/rm` | append/remove one `[[mcp]]` server (stdio command+args or http url, `KEY=VALUE` env/headers lines, `secret:name` refs) and hot-reload |
| GET | `/doctor` | the in-process health checks (config, model key, store, data-root permissions, web publication, channel tokens) plus the full `ka-agent doctor --json` (pin, contract hash, sandbox) |
| POST | `/doctor/fix` | run the binary's safe repairs (0700 roots, 0600 secret files) and show their output |
| GET | `/logs` | the tail (300 lines) of `<data>/logs/daemon.log`, the same file `ka-agent logs` reads |
| POST | `/pair/issue` | mint a web pairing code for a role from the admin UI (audited; shown once) |
| POST | `/upload` | one attachment (`X-Ka-Filename`, raw body, `X-Ka-Agent: 1`, max 8 MiB) under the caller role's `attachments/`; replies with the `@attachments/…` ref |
| GET | `/file/<name>` | serve one direct child of the caller role's `attachments/` or workspace; traversal and symlink escapes are refused |
| POST | `/settings` | the status page form, one control per `config_keys::KEYS` entry marked `web` (see below): format-preserving writes + live reload; each key shows when it applies; unknown keys are ignored; invalid values are refused |
| GET | `/usage` | spend per day **and model** (30 d), CSS bars, today vs the cap |
| GET | `/tasks` | reminders and crons, next fire first, with role/tools, the create forms (same grammar as `/cron add` and `/remind`), and pause/resume/run-now/remove per cron |
| POST | `/tasks/add` | `kind=cron\|reminder` + `args=` runs the router's own command and shows its reply |
| POST | `/tasks/calendar` | `args=` runs the router's `/calendar` (add/today) and shows its reply |
| POST | `/tasks/cron` | `id=` + `action=pause\|resume\|trigger\|remove` — the same `cron_ops` the CLI uses |
| POST | `/tasks/cancel` | `id=` (prefix ok) cancels a reminder or cron → `303 /tasks`; unknown/ambiguous ids render an explaining page |
| POST | `/tasks/snooze` | `id=` + `when=` (the `/remind` grammar: `30m`, `18:30`, `tomorrow 9am`, …) moves one reminder's next fire → `303 /tasks`; crons keep their cadence |
| GET | `/skills` | the skill library and a matrix of which engine home (admin + each role) has each skill, plus pending proposals |
| POST | `/skills/toggle` | `name=` + `role=` + `enable=1\|0` — provisioning into one role's engine home |
| GET | `/contacts` | linked identities + unknown contacts with their `ka-agent identity link` command |
| GET | `/history` | search form; `?q=` runs FTS over episodes (whitespace-separated terms AND together; a quoted span stays one phrase) |
| GET | `/history/show/<turn>` | one turn: inbound text + the outbound answer |
| GET | `/memory` | report (bytes/budget), remember-now and propose forms, the review inbox (candidates with role and diffs), accepted entries, commit history with undo, compiled text |
| POST | `/memory/remember` | accepts one fact now (facts.md + commit) and shows the result |
| POST | `/memory/undo` | reverts the last memory commit |
| POST | `/memory/edit` | stages a candidate (`text=`) → `303` back to `/memory` |
| POST | `/memory/candidate` | `id=` + `decision=y\|n` applies a review decision → `303` |
| POST | `/memory/forget` | `id=` removes one accepted entry → `303 /memory`; an unknown id renders an error page instead of a silent redirect |
| GET | `/approvals` | pending asks with allow/deny/allow-for-session forms |
| POST | `/approvals/decide` | `id=` + `decision=y\|n\|session` → `303 /approvals` (JSON when the `X-Ka-Agent` header is present — the inline chat card's fetch path) |
| GET | `/audit` | the last 50 audit-chain records (seq, time, actor, **role**, action, subject) |
| GET | `/intents` | the last 20 MCP intents and their status |

Every page above renders inside a grouped sidebar (Chat, Operator, System);
Approvals, Memory and Queue carry live badges that `chat.js` bumps from the
SSE stream, so they stay current without a reload. The session cookie gains
`Secure` when `[web] public_url` is https; HTML responses are `no-store`
under the strict CSP.

## What stays in the CLI

OS-level and destructive operations are not web routes by design:
`service install|status|uninstall` (systemd/launchd), `restore`,
`plugin install|remove` (the panel lists them), `update` (the chat's
`/update` still works), `profile` under a pinned `KA_AGENT_DATA_DIR`, and
`models refresh`. Everything else has a page or a chat command — including
roles/identities/MCP editing, a one-click backup and the full doctor.

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
