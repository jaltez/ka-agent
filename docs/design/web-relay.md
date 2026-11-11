# Design: the web relay and control UI

Status: proposed (2026-11). Prereq: in-process RBAC (ADR-0002) — the
daemon already partitions sessions, memory, tools and budgets per role.

## Goal

Give Ka Agent a first-class browser surface, not a bolt-on chat box:

- **Chat** (`/`): the same agent the TUI and Telegram reach, with streaming
  replies, tool/plan cards, approval state, files and the common controls
  (model, mode, effort, stop, retry, new session).
- **Admin** (`/admin`): the operator's control panel — approvals, profiles,
  roles and identities, config (schema-driven), cron/reminders, memory
  review, skills, audit, status/doctor, logs.
- **Remote relay** (`ka-agent relay` or a reverse proxy in front of
  `[web]`): a custom domain, TLS, the agent on a home machine, the UI on a
  VPS or behind Tailscale.

Setup stays CLI/TUI (`ka-agent setup`, `ka-agent pair web`): a web server
cannot authenticate anyone before an operator exists, and a wizard over an
unauthenticated socket is a liability. Once the token/code exists, the UI
takes over.

## What the field does (and what we take)

Researched with Tavily, 2026-11 (`tvly search/extract`).

| Project | Shape | Auth | Session model | Lesson |
| --- | --- | --- | --- | --- |
| **Hermes Agent** | Daemon with a built-in **OpenAI-compatible API server** (`/v1/models`, `/v1/chat/completions`, `/v1/responses`, `/health`); Open WebUI/LobeChat/LibreChat point at it | Bearer `API_SERVER_KEY` | Explicit `X-Hermes-Session-Id` / `X-Hermes-Session-Key`; a request may not bind a session another surface owns (**409 permission_denied**) | A compatibility shim buys every OpenAI frontend cheaply; session ownership must be explicit and enforced |
| **Hermes community WebUI** | Self-hosted MIT SPA, chat + config + phone use | account/session | local sessions | One coherent UI beats many admin pages; mobile parity matters |
| **OpenClaw** | **Gateway** process + Vite/Solid **Control UI** served at `/` (configurable `basePath`, custom `root`) | auth at the **WebSocket handshake**; token or password; secrets kept in `sessionStorage`, out of the URL | DMs share the main session, each group chat its own; multi-agent routing | The Control UI is an **admin surface** — never public by default; loopback bind, Tailscale Serve/SSH tunnel recommended; static SPA served by the gateway |
| **Open WebUI** | Mature chat frontend with accounts | its own accounts | its own | Don't rebuild a user-account system in v1; roles + pairing codes are enough |

Best practices taken:

1. **Two surfaces, two realms**: public role chat and admin control. The
   admin realm never shares a cookie/session with a role login.
2. **Transport-level auth**: the token/cookie is checked before the event
   stream opens; no data before authentication; secrets never in a URL.
3. **Explicit session identity**: the browser names the session it is
   continuing; binding another role's/surface's session is a hard 409.
4. **Loopback by default**; public exposure is an explicit choice
   (`[web] bind`, TLS, or a reverse proxy/Tailscale), with a doctor check.
5. **Static SPA served by the daemon** with a configurable base path and
   root, so a package can ship a prebuilt UI and an operator can override
   it.
6. **One event model**: the UI consumes the same sequenced `Envelope`
   stream as the TUI/web panel (seq → `Last-Event-ID` resume), so replays
   and reconnects cannot duplicate or lose a chunk.

## Architecture

```
browser ──HTTPS──► reverse proxy (Caddy/nginx/Tailscale)
                        │  (or direct: [web] tls)
                        ▼
             ka-agent daemon · ka-agent-web router
             ├── GET /                     static SPA (embedded, overridable)
             ├── POST /api/v1/auth/*       pairing code → role session cookie
             ├── GET  /api/v1/events       SSE: Envelope stream (role-filtered)
             ├── POST /api/v1/chat         submit / steer / stop / new
             ├── GET  /api/v1/sessions     the caller role's sessions
             ├── GET  /api/v1/approvals    admin realm only
             ├── /api/v1/admin/*           profiles, roles, identities, config,
             │                             cron, memory, skills, audit, logs
             └── /v1/*                     optional OpenAI-compatible shim
                        │
                        ▼
                 Runtime (one per profile)
                 RBAC: role ← identity ← cookie/token
```

- **v1 lives in the daemon** (`ka-agent-web` already serves the panel,
  SSE, approvals and pages): one process, direct `Runtime`, no new IPC.
- **The relay is the same router** mounted by a future `ka-agent relay`
  binary that talks to a daemon over the existing admin socket. The router
  is factored behind a `ControlApi` trait so both deployments share it.
- **Profiles**: the admin API reuses the profile operations (list/create/
  use/remove) extracted from `apps/ka-agent/src/profile_cli.rs` into a
  `ka-agent-profiles` library; creating a profile from the UI runs the same
  code the CLI does (one-time config copy, free web port, roles/tools kept).

## Identity and auth

Reuse the RBAC model; do not invent accounts.

- **Admin realm**: the existing `[web]` token (`web.token` /
  `admin.token`) is the operator's login. `/admin` and `/api/v1/admin/*`
  accept only it (or a session cookie derived from it). Same rules as the
  TUI: role `admin`.
- **Role realm**: `ka-agent pair web --role guest` prints a one-time code
  (the Telegram/Matrix pairing machinery: TTL, 5 attempts, constant-time
  compare, audited). The browser posts the code to `/api/v1/auth/pair`; the
  daemon creates/uses the `web:<user>` identity with that role and returns
  an `HttpOnly; SameSite=Strict` session cookie (signed, rotating) plus a
  CSRF token. A role login can never reach `/admin`.
- **Role resolution**: extend `role_for_source` to `web:<user>` (identity
  role) while bare `web` stays admin (loopback panel, CLI). That is the only
  RBAC change the relay needs.
- **Session binding**: the cookie carries the session it may resume
  (`web:<user>` → that role's session). A request naming a session of
  another role/surface is refused with 409, mirroring Hermes.
- Cookies are `Secure` when TLS is on; mutations require the CSRF header;
  login attempts and admin mutations are audited with the acting role.

## Event stream

`GET /api/v1/events` is SSE (the panel already does this):

- Each line is the runtime's `Envelope` (`seq`, `at_ms`, `source`, `ev`).
- `Last-Event-ID` replays from the ring; `seq` is monotonic per runtime, so
  the UI dedupes and resumes exactly.
- The stream is filtered by the caller's role: a role login receives only
  its own turn-scoped events plus global notices; approval cards and other
  admin-only events are never written to a role stream (the daemon already
  stamps `source`/`None`).
- WebSocket is a later optimization for typing indicators/steering; SSE +
  POST keeps the server simpler and works through every proxy.

## Chat UX (v1)

- Composer with `@file` references, image/voice upload (reuse
  `attachments`), Enter/Shift-Enter, stop (`/stop`), retry.
- Streaming answer with Markdown + code blocks (same renderer rules as the
  panel), tool rows (name, duration, first error line, expandable output),
  plan checklist, context meter, compacting state.
- Header: role badge, model/mode/effort pickers (role-clamped; the server
  enforces the ceiling), budget state, session switcher (`/new`, `/resume`,
  history search).
- Approvals: an inline banner linking to the admin surface ("waiting for
  the operator"); a role never gets allow/deny buttons.
- Files: download links and inline images from the outbox/attachments.

## Admin UX (v1)

Schema-driven, so new config keys appear without UI work
(`config_keys::KEYS` already carries kind, bounds, default, apply class and
a doc line; `Apply` tells the page whether a change is live, next turn or
restart):

| Page | Source |
| --- | --- |
| Status / Doctor | `Runtime::status_report`, `doctor` checks |
| Approvals | `pending_approval_cards`, `decide` (admin-only) |
| Profiles | `list_profiles` + create/use/remove |
| Roles & identities | `[[role]]`, `identity link/role/list` |
| Config | `config schema`, `config get/set/unset`, hot-reload report |
| Cron / reminders | `scheduled_tasks`, `add_cron`, cancel/snooze |
| Memory | `pending_candidates`, `memory_entries`, accept/reject |
| Skills | library + `[enabled@role]` provisioning |
| Audit | `recent_audit` (role column), `verify_audit_chain` |
| Logs / usage | `ka-agent logs`, `usage` |

Every mutation goes through the same runtime/CLI code path (no parallel
logic), is audited with the admin role, and reports the reload class.

## Frontend

- **Stack**: Vite + React + TypeScript + Tailwind, shadcn-style primitives
  (owned components, no runtime UI dependency). A `web/` directory with
  `npm run build` → `dist/` embedded with `include_dir!` (override with
  `[web] root`). Development: `npm run dev` with a proxy to the daemon.
- **Design**: one accent, neutral surfaces, monospace for code/tool output,
  density closer to a terminal than a marketing page; dark/light from the
  OS, keyboard-first (⌘K command palette, `/` focus, `g` chords), mobile
  layout at parity with Telegram.
- **State**: one event stream store keyed by `seq`; server state is the
  truth (no optimistic session state), optimistic only for the composer.
- **i18n**: the daemon already has `language` (en/es); the UI ships both.

## Security checklist

- Loopback bind by default; `doctor` warns on `0.0.0.0` without TLS.
- Admin token ≥ 32 random bytes, 0600, never rendered after login; roles
  use pairing codes (single-use, TTL, attempts, constant-time).
- CSP (no inline scripts except the built bundle's nonce), HSTS with TLS,
  `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`.
- CSRF token on every mutation; `SameSite=Strict` cookies.
- No secrets in URLs or logs; redact tokens in error pages.
- Every authorization decision is server-side and role-derived; the client
  never asserts a role.
- Rate limits: login attempts, chat submissions per identity, SSE
  connections per role.
- Public exposure documented as reverse proxy/Tailscale; built-in TLS
  (rustls) for the direct case.

## UI system (v0.14)

- **No build step**: one server-rendered HTML page per surface, one
  embedded stylesheet, one script (`chat.js`). Tokens drive light/dark from
  the OS; components are plain classes (`.card`, `.btn`, `.tablewrap`,
  `.field`, `.chip`, `.badge`, `.flash`, `.empty`, `.bar`, `.statgrid`,
  `.grid2`, `.pagehead`) so a new page is markup, not a framework.
- **Shell**: grouped sidebar (Chat, Operator, System) with live badges for
  approvals, memory review and queue; every page has a title, a one-line
  purpose and (where useful) actions. The chat shell is full-height with a
  sticky composer.
- **Forms** are POST-only and answer with the agent's own reply (no silent
  redirects on failure); destructive actions require the name typed back.
- **Chat**: SSE with `seq` resume, streaming text and a caret, tool rows
  with output drawers, plan lists, context meter, attachments both ways,
  inline image thumbnails, retry on transient errors, `/` to focus,
  auto-scroll unless the reader scrolled up.
- **Extensibility**: `config_keys::KEYS` drives the config table/form and
  `COMMANDS` drives the chat, so new keys and commands appear without new
  UI code; a page is `nav_page_sub(runtime, path, title, subtitle, body)`.

## Deploying behind a custom domain (v1 recipe)

1. Keep the daemon on loopback (`[web] enabled = true`, default port
   8480); never bind `0.0.0.0` without TLS.
2. Put a reverse proxy in front. Caddy:

   ```
   agent.example.com {
       reverse_proxy 127.0.0.1:8480
       encode zstd gzip
   }
   ```

   nginx: `proxy_pass http://127.0.0.1:8480;` with
   `proxy_buffering off;` for `/events` (SSE) and
   `proxy_read_timeout 1h;`. Tailscale Serve/Funnel is the no-DNS option
   (OpenClaw's recommendation too).
3. Pair browsers with `ka-agent pair web --role <r>` over the TLS URL;
   codes are one-time and short-lived, so a plaintext hop is the only
   thing to avoid.
4. `ka-agent doctor` (future check) should flag a public bind without TLS
   and a pairing code past its TTL.

## Shipped in this change (v1 slice)

- `web:<user>` RBAC sources and `ka-agent pair web --role`.
- `POST /pair` → role session cookie; role pages are chat-only; `/events`
  is filtered to the role's own source; admin pages return 403.
- `/profiles` (profiles on disk, configured roles, linked identities with
  role reassignment; profile create/use run the CLI), `/settings` config
  form, plus the existing status/tasks/memory/skills/approvals/history/
  usage/contacts/audit/intents pages.
- Chat header with role/mode/model chips, pending-approvals chip and
  new/retry/stop actions.
- Attachments both ways: `POST /upload` (raw body, 8 MiB) lands under the
  caller role's tree and returns the `@attachments/…` ref;
  `GET /file/<name>` serves the role's own files with traversal/symlink
  checks; `AgentEvent::File` renders as a download link.
- Admin-issued web pairing codes (`POST /pair/issue`, Profiles page);
  profile create/use/remove from the Profiles page (remove requires the
  name typed as confirmation); role editor writing `[[role]]`; identity
  link/unlink; `[tools] disable` baseline; MCP add/remove writing
  `[[mcp]]` — all re-validated as a whole config and hot-reloaded.
- A theme preference (system/light/dark) persisted in `localStorage`,
  with a strict-CSP UI script loaded on every page.
- `[web] bind` / `[web] public_url`, `ka-agent setup web`, and a `doctor`
  warning for non-loopback binds; startup/`dashboard` print the public URL.
- Admin parity pages: models/mode/effort/persona, config (schema + safe
  editor + raw TOML), queue, grants, keys/secrets (names, presence, secure
  set forms and removal), extensions (MCP editor/plugins/hooks/live
  tools), calendar digest, in-process + full doctor with `--fix`, daemon
  log tail, role/identity management, and task creation with
  pause/resume/trigger/remove.
- A grouped app shell with a token-based design system (cards, tables,
  forms, chips, empty states, budget bars), page subtitles, a live chat
  toolbar, tool-output drawers, retry on transient errors, `/` focus, and
  mobile layouts.
- Still designed, not built: browser voice recording/transcription,
  built-in TLS/ACME, the standalone `ka-agent relay` binary, and the
  OpenAI-compatible `/v1` shim.

## Phases

1. **RBAC web identities**: `web:<user>` source, pairing `ka-agent pair web
   --role`, role resolution, 409 session binding.
2. **Relay API**: auth/session cookie, SSE stream with `Last-Event-ID`,
   chat submit/steer/stop/new, sessions list; role-filtered.
3. **Chat UI**: shell, streaming, tool/plan cards, composer, pickers.
4. **Admin UI**: approvals, status, identities/roles, config form, cron,
   memory, skills, audit.
5. **Profile lifecycle + setup hand-off**: profile create/use/remove from
   the admin page; `ka-agent setup` prints the dashboard URL and token.
6. **Relay deployment**: `ka-agent relay --upstream`, TLS/domain docs,
   OpenAI-compatible `/v1` shim (optional, for third-party clients).

Each phase ships green: the daemon's existing tests stay, and the web layer
gets router tests with a fake control API plus one browser-level smoke test.
