# Ka Agent v1 redesign

Status: **proposal — awaiting approval** · baseline v0.4.2 (`ea5d8b6`) · 2026-10-22

Ka Agent should become the resident agent people leave running: useful on day
one, pleasant on the phone, cheap and quiet when idle, and trustworthy with
keys and memory. This document reviews every aspect of the agent against
that bar and proposes a change for each gap. Every item has an id,
evidence, an effort (S < 1 day, M = days, L ≥ week) and an **acceptance
check** — the command or test that proves it is done.

## 0. Where we stand

### Measured baseline (release build, this machine)

| Metric | Value |
|---|---|
| Binary | 8.8 MB (`opt-level="z"`, fat LTO, stripped — `Cargo.toml:71-76`) |
| CLI cold run (`--version`, `status`) | ~0 s wall, 5.7–6.9 MB max RSS |
| Idle daemon, no worker | **9.7 MB RSS** (worker spawned lazily, reaped after 15 min idle) |
| Code | 28.4 k lines Rust across 11 crates; 36 test suites |

The footprint already matches the "lightweight" alternatives (ZeroClaw
advertises < 5 MB / 3.4 MB binary; PicoClaw < 10 MB) [S11]. Efficiency is
a strength to **protect**, not the thing to fix.

### What users of resident agents say (research summary)

- **Wanted:** lives in Telegram; persistent memory; morning briefing; email/
  calendar/notes; self-hosted and cheap ($5–25/month); voice notes [S1][S2][S3].
- **Hated:** setup "extremely difficult"; opaque automatic memory is
  "unsettling"; memory pollution/forgetting; hallucinated facts; proactive
  nudges become "noise"/"spam" when untuned; runaway API spend; approval
  fatigue (users approve ~93–97 % of prompts by reflex) [S2][S4][S5][S6][S7].
- **Security is the category's scandal:** 36 % of ClawHub skills with
  prompt injection, 1,184 malicious skills, 135 k exposed OpenClaw
  instances, API keys in `.bak` files [S8][S9]. Hermes wins reviews
  precisely by "treating security as a starting constraint" [S9].
- **Phone UX that feels natural:** streaming replies, typing indicator,
  inline buttons, reactions as acknowledgement, voice-note transcription,
  rich formatting [S10]. Bot API 9.5 (Mar 2026) gives every bot
  `sendMessageDraft` for native streaming in private chats [S12].

### Ka Agent's position

Strong where the category is weak: operator-gated git-backed memory,
host-issued pairing (bot never talks to strangers), signed sandboxed
plugins, deny-on-expiry approvals, budgets, 9.7 MB idle. Weak where users
judge in the first hour: **few things it can actually do** (no web search,
no MCP, no voice, read-only calendar), an **unstyled web panel**, a
**terminal chat that prints raw Markdown**, a 28-command flat CLI, and no
update path.

A key discovery: the ka engine (pinned `=0.3.0`) already ships `web_search`
(Tavily/Brave/Bing), `web_fetch`, a full MCP client (`[[mcp]]`, eager or
lazy discovery), a fallback model chain (`[fallback]`) and spend guards
(`[guards]`) — `ka-engine-0.3.0/src/{hands/web.rs,mcp.rs,config.rs:123,260,398,444}`.
Ka Agent builds the engine config in `crates/ka-agent-port/src/worker.rs:296-318`
and exposes **none** of it. The largest capability jump costs no engine change.

---

## 1. Capabilities & integrations

### E1 · Web search and fetch — *S*
- **Now:** the model's only network tool is `http_get` behind per-host
  grants (`crates/ka-agent-channels/src/mcp.rs:218`). Engine `web_search`
  is never configured (`worker.rs:296-318`), so "what's the weather / news /
  price" fails.
- **Change:** `[search] provider = "tavily"|"brave"|"bing"` (+ optional
  `base_url`) in Ka Agent config → engine `[[search]]` with `api_key_env`
  (`TAVILY_API_KEY`/`BRAVE_API_KEY`/`BING_API_KEY`) read from the engine
  `.env` tier. Setup offers it as an optional step ("Give Ka web search?
  free Tavily key: 1,000 searches/month"). `web_fetch` stays visible;
  private targets still refused unless granted. Doctor reports search state.
- **Sources:** [S3] wishlist; engine `hands/web.rs:55-110`.
- **Acceptance:** unit test — worker engine-config builder emits the search
  provider for `[search]` and omits it otherwise; `ka-agent doctor` prints
  `✓ web search tavily (key set)` / `! web search off`.

### E2 · MCP servers — *M*
- **Now:** no third-party tools; every competitor has `mcp add` [R§8].
- **Change:** `[[mcp]] name, command, args, env, url, headers` in config;
  `ka-agent mcp add <name> -- <cmd…>` / `--url`, `mcp list`, `mcp rm`.
  Secret values referenced as `env = { TOKEN = "secret:github.token" }`
  and resolved from the secret store at worker spawn (never written to
  config). Passed through to engine `[[mcp]]`; `tools.mcp.discovery =
  "lazy"` when > 1 server to save context. Guarded mode keeps every MCP call
  behind the engine's permission ask (same card on phone/web).
- **Sources:** [S13] most-used personal MCPs (Gmail, Calendar, Todoist,
  Home Assistant); research §8.
- **Acceptance:** e2e — `mcp add` writes a valid config; a worker spawned
  with `[[mcp]]` pointing at the repo's own `ka-agent __mcp` bridge under a
  different name lists that server's tools (`kagent2.notes_search`) in the
  engine inventory; `mcp rm` removes it.

### E3 · Model fallback — *S*
- **Now:** a provider outage or rate limit kills the turn.
- **Change:** `fallback = ["openai/gpt-5.1"]` → engine `[fallback] models`
  (≤ 2 hops). `/status` shows the fallback chain.
- **Acceptance:** unit test on engine-config builder; `config set fallback`
  validates each selector against the catalog.

### E4 · Calendar digest correctness — *S*
- **Now:** times shown "exactly as the ICS states them (`Z` kept, no
  timezone conversion)" and no `RRULE` expansion (`crates/ka-agent-channels/src/calendar.rs:1-8`):
  a UTC event reads wrong for every non-UTC operator; weekly meetings vanish.
- **Change:** convert `DTSTART` (Z, TZID, floating) to the operator zone
  with jiff; expand `RRULE` FREQ=DAILY/WEEKLY(/BYDAY)/MONTHLY within the
  digest window, honouring `EXDATE`.
- **Acceptance:** unit tests: a `…T150000Z` event renders `17:00` for
  `Europe/Madrid` in summer; a weekly BYDAY=MO,WE rule yields Wednesday's
  occurrence; an EXDATE is skipped.

### E5 · Model-side history recall — *S*
- **Now:** FTS5 episodic search exists but only for the operator (`/history`).
- **Change:** bridge tools `history_search(query, limit≤10)` and
  `history_show(turn)` (bounded snippets), read-only, no grant needed.
- **Acceptance:** ops e2e — bridge call returns a seeded turn by keyword.

---

## 2. Onboarding & setup

### A1 · Honest first-contact messages — *S*
- **Now:** `ka-agent chat` on a fresh machine prints "config … invalid: read
  …: No such file or directory" (`apps/ka-agent/src/chat.rs:59`,
  `daemon.rs:94`); `ka-agent` without a TTY dumps 28-command help.
- **Change:** missing config → "Ka Agent isn't set up yet — run `ka-agent
  setup` (about 3 minutes)". Invalid config keeps the parse error. No-arg
  non-TTY prints a 5-line "start here" (setup, chat, status, dashboard, help).
- **Acceptance:** e2e — `chat` and `run` with no config exit 1 with
  "isn't set up yet"; never "No such file".

### A2 · Setup finishes with a "you're live" card — *S*
- **Now:** setup ends after the first reply; the user must discover the
  phone, dashboard, briefing.
- **Change:** final card: where to talk (Telegram @bot / terminal / dashboard
  URL), 3 example prompts tailored to enabled capabilities ("remind me at
  18:30…", "search the web for…", "every day at 8 send me my calendar"), and
  the one command to see health (`ka-agent status`).
- **Acceptance:** setup_e2e `--yes` flow output contains the card heading
  and the example list.

### A3 · Optional capability steps in the wizard — *S*
- **Change:** after channels: web search (E1), daily briefing (G1), voice
  (D2) — each one prompt, skippable, defaults off; `setup search|briefing|voice`
  sections.
- **Acceptance:** `setup search` with a piped key writes `[search]` and the
  key; doctor shows it.

### A4 · Import from other agents — *M*
- **Change:** `ka-agent import hermes|openclaw|claude|codex [--dry-run]`:
  their MEMORY/USER files → **memory candidates** (operator still accepts);
  `SKILL.md` dirs → `~/.ka-agent/skills`. Lowers switching cost for the
  users the research says are frustrated with OpenClaw [S8][S9].
- **Acceptance:** e2e with a fake `~/.hermes/memories/MEMORY.md` and a skill
  dir: `--dry-run` lists N candidates + 1 skill, writes nothing; real run
  creates them.

---

## 3. CLI

### B1 · A CLI you can read — *S*
- **Now:** 28 top-level commands in one list, help strings citing internal
  docs ("SEED.md §8.1", `main.rs` Command enum).
- **Change:** top-level help shows the everyday set (setup, chat, status,
  dashboard, model, config, service, logs, doctor, usage, memory, profile,
  cron, approvals, update); the rest (identity, grant, secret, key, plugin,
  skills, backup, restore, export, purge, pair, mcp, import, run, restart)
  stay working, `hide = true`, listed in an "Advanced" footer. No `SEED.md`
  references in user-facing help.
- **Acceptance:** `ka-agent --help` lists ≤ 16 commands and contains no
  "SEED.md"; every hidden command still parses (`ka-agent grant --help`
  exits 0).

### B2 · `--json` for scripts — *S*
- **Change:** `status`, `doctor`, `usage`, `profile list`, `cron list`
  accept `--json` (one document on stdout; `{ok:false,error}` on failure).
- **Acceptance:** `ka-agent doctor --json | python3 -c 'import json,sys;json.load(sys.stdin)'`
  exits 0; same for the others.

### B3 · Cron and approvals from the shell — *S*
- **Now:** crons only from chat (`commands.rs:692`); approvals only from chat/web.
- **Change:** `ka-agent cron list|add <when> <text>|rm|pause|resume|run <id>`
  and `ka-agent approvals [list|y <id>|n <id>]` over the admin socket.
- **Acceptance:** daemon e2e: `cron add "every day at 08:00" hi` then
  `cron list` shows it; `approvals list` on an empty instance prints "none".

### B4 · Shell completions — *S*
- **Change:** `ka-agent completions bash|zsh|fish` (clap_complete), profile
  names completed after `-p`.
- **Acceptance:** `ka-agent completions bash` exits 0 and defines
  `_ka__agent()`; sourcing it in bash completes `ka-agent -p w<Tab>` to a
  profile named `work`.

---

## 4. Terminal chat

### C1 · Rendered Markdown — *S*
- **Now:** answers print raw `**bold**`, list markers and fences
  (observed; `apps/ka-agent/src/chat.rs:250`).
- **Change:** a small Markdown→ANSI renderer (headings, bold/italic, inline
  code, fenced code dimmed with language tag, lists, links as `text (url)`)
  when stdout is a TTY; plain text when piped (tests and scripts unchanged).
  Shared Markdown tokenizer with the Telegram (`tg_format.rs`) and web
  renderers so all three agree.
- **Acceptance:** unit tests for the renderer (ANSI and plain modes);
  existing chat_e2e (piped) untouched and green.

### C2 · Less noise, more context — *S*
- **Now:** `· session Idle` after every turn (`chat.rs:294`); banner
  without channels/profile until attached.
- **Change:** drop session-status lines except Paused/Degraded (which carry
  the remedy); prompt shows profile when non-default (`work ❯`); footer
  `· 1.2 s · $0.0012 · 420 tok`.
- **Acceptance:** chat_e2e asserts no "session Idle" line after a turn.

---

## 5. Phone channels

### D1 · Streaming replies on Telegram — *M*
- **Now:** no deltas on Telegram (`telegram.rs:580` no-op); users wait for
  the whole answer behind a typing indicator.
- **Change:** in private chats use Bot API 9.5 `sendMessageDraft` with a
  stable `draft_id`, throttled to ≥ 1 update/s and ≥ 40 new chars; final
  answer via the existing HTML `sendMessage`. If the API answers 400/404
  (older Bot API servers, groups), fall back silently to today's behaviour
  and remember it for the process. Config `telegram.stream = true` (default).
- **Sources:** [S12] Bot API changelog; OpenClaw/Hermes issues adopting it.
- **Acceptance:** telegram_e2e with FakeBotServer: a multi-step scripted
  answer produces ≥ 1 `sendMessageDraft` then one `sendMessage`; when the
  fake answers 400 to drafts, only `sendMessage` is used and the answer
  still arrives.

### D2 · Voice notes — *M*
- **Now:** voice messages are ignored; photos/documents land in the inbox
  (`telegram.rs:1004`).
- **Change:** `[voice] provider = "openai"|"groq"|"local"` with
  OpenAI-compatible `/audio/transcriptions` (`whisper-1`,
  `whisper-large-v3-turbo` on Groq, or a local whisper server URL). Voice
  note → download (`getFile`) → transcribe → shown back as `🎙 "…"` and
  submitted as the turn text. Unconfigured: one reply "voice notes need
  `ka-agent setup voice`". Size cap 20 MB, duration cap 10 min.
- **Sources:** [S10][S14].
- **Acceptance:** telegram_e2e with a fake STT endpoint: a voice update
  yields a turn whose inbound text is the fake transcript; unconfigured
  yields the setup hint and no turn.

### D3 · Receipt reaction — *S*
- **Change:** `setMessageReaction 👀` when a free-text message is accepted
  (queued or started) — the acknowledgement users expect [S10]. Kept as a
  receipt rather than cleared on answer: clearing costs a call per message
  and is ambiguous once messages queue.
- **Acceptance:** fake captures `setMessageReaction` for an inbound message.

### D4 · Memory proposals as one-tap cards — *S/M*
- **Now:** `memory_propose` candidates wait for `/memory review`
  (`commands.rs:558`); nothing reaches the phone, so memory rarely grows.
- **Change:** each proposal (batched max 1 card / 10 min) is posted as a card
  with ✅ Remember / ✖ Discard (Telegram inline buttons, Matrix reply
  `y`/`n`, web buttons, terminal `/memory review`), reusing approval-card
  plumbing. Keeps operator-only writes (the anti-pollution property users
  value [S5]) while removing the friction.
- **Acceptance:** telegram_e2e — a bridge `memory_propose` produces a card;
  pressing ✅ (callback) moves it to `memory/facts.md` and commits.

### D5 · Matrix parity — *S*
- **Change:** send `formatted_body` (HTML from the shared renderer) with
  plain `body` fallback; voice (D2) via `m.audio`.
- **Acceptance:** matrix fake captures `format: org.matrix.custom.html`.

---

## 6. Memory

### F1 · Visible memory budget — *S*
- **Now:** compiled memory truncates silently at 6 KB
  (`crates/ka-agent-memory/src/lib.rs:14`); only `/memory` shows it.
- **Change:** `status` line `memory 4.1/6 KB · 2 to review`; doctor warns
  ≥ 90 % and when files were omitted.
- **Acceptance:** unit test on status render with seeded memory sizes.

### F2 · Poisoning guard on proposals — *S*
- **Change:** before queueing a candidate, reject/flag invisible Unicode
  (bidi controls, zero-width), credential-shaped strings (key patterns), and
  instruction-shaped text ("ignore previous…"); flagged cards say why.
- **Sources:** Hermes memory scanning; Reddit "poisoned memory" [S5].
- **Acceptance:** unit tests: RTL-override and `sk-…`-shaped proposals are
  refused with a reason; plain facts pass.

---

## 7. Proactivity & scheduling

### G1 · Daily briefing preset — *S/M*
- **Now:** a calendar digest exists (no-agent) but nothing composes "your
  day".
- **Change:** `ka-agent briefing on 08:00` (and setup step): an agent-mode
  cron with a curated prompt — calendar (E4), today's reminders, pending
  approvals/memory cards, optional web headlines (if E1) — that must answer
  in ≤ 12 lines and say "nothing notable" when so. One message, not a
  heartbeat: research shows 3–5 proactive messages/day is the tolerance
  ceiling and untuned proactivity reads as spam [S6][S15].
- **Acceptance:** `briefing on 08:00` creates exactly one cron with
  `source = "briefing"`; `briefing off` removes it; `briefing now` fires it
  (scripted-speaker e2e sees the prompt).

### G2 · Quiet hours — *M*
- **Change:** `[notify] quiet_hours = "22:00-07:00"`: cron/briefing
  results and notices produced in the window are held and delivered as one
  digest at its end; reminders the operator set explicitly still fire;
  approvals still post (they expire).
- **Acceptance:** runtime test with a fixed clock: a cron result at 23:00
  is held, delivered at 07:00 with its timestamp.

### G3 · Recurrence grammar — *S*
- **Change:** `every weekday at 9`, `every mon,wed,fri at 18:00`,
  `every month on the 1st at 9` in `crates/ka-agent-core/src/when.rs`.
- **Acceptance:** `when.rs` unit tests for each form incl. DST change.

### G4 · Cron run history and preflight — *S*
- **Change:** each fire recorded in `runs` (outcome, cost); `cron runs
  <id>`. Before an agent-mode fire: no model/key or budget exhausted → job
  marked `blocked`, one notice, zero tokens.
- **Acceptance:** runtime test: a fire with budget exceeded records
  `blocked` and submits nothing.

### G5 · No cron loops — *S*
- **Change:** turns started by a cron run with `KA_AGENT_TURN_SOURCE=cron`
  visible to the bridge (meta row keyed by live turn); `cron_manage
  schedule` refuses inside cron-fired turns.
- **Acceptance:** ops e2e — bridge schedule call with the cron marker set
  is refused.

---

## 8. Approvals & trust

### H1 · Live mode switch with visible badge — *S*
- **Change:** `/mode guarded|accept_edits|plan|free` (console/web; phone
  only to *tighten*); non-guarded mode shows a badge in prompt, `/status`,
  web header.
- **Acceptance:** chat_e2e `/mode plan` → status line shows `mode plan`.

### H2 · Fewer, better prompts — *M*
- **Now:** every risky call asks; "always" never offered from channels.
  Approval fatigue is a security bug, not just UX [S7].
- **Change:** opt-in `approvals.auto_review = true` mapping to the engine's
  `[guards]` auto-review (fast model may auto-allow unmistakably safe exec
  calls, never deny — `ka-engine config.rs:123-140`); "Allow for this
  session" on console/web only, scoped to tool + exact args, cleared on
  `/new`.
- **Acceptance:** unit test on engine-config builder for `auto_review`;
  runtime test: a session-allowed (tool,args) pair is auto-approved once
  more, a different arg still asks, `/new` clears it.

---

## 9. Web panel

### I1 · A panel people want to open — *M*
- **Now:** unstyled HTML (observed: default serif, blue links, raw
  `**bold**`, list items run together; `crates/ka-agent-web/src/pages.rs`).
- **Change:** one embedded stylesheet (< 8 KB, light/dark via
  `prefers-color-scheme`, mobile viewport), left nav with badges (approvals,
  memory to review), chat as bubbles with server-rendered sanitized
  Markdown (shared renderer, C1), streaming via the existing SSE, inline
  approval and memory cards, status as cards. No external assets; strict
  CSP header.
- **Acceptance:** web_e2e: chat page contains the stylesheet link and CSP
  header; a reply with `**x**` renders `<strong>x</strong>` and escapes
  `<script>`; screenshot attached to the phase report.

### I2 · Settings you can change in the browser — *M*
- **Now:** settings table lists CLI commands to type.
- **Change:** form for model (catalog select), mode, timezone, daily budget,
  search provider, quiet hours, briefing time → `config_file::set` + live
  apply; restart-needed keys say so.
- **Acceptance:** web_e2e POST `/settings` with `budgets.daily_usd=3` →
  config file has `daily_usd = 3.0` and `/status` reflects it.

### I3 · Usage page — *S*
- **Change:** last 30 days, CSS bars, today vs budget (same data as
  `ka-agent usage`).
- **Acceptance:** web_e2e `/usage` lists seeded days.

---

## 10. Profiles

### J1 · Complete lifecycle — *S*
- **Change:** `profile remove` uninstalls the profile's units itself
  (stop → uninstall → delete) instead of refusing; `profile rename`;
  `profile create --clone-memory` (fresh git repo, first commit records
  source).
- **Acceptance:** layout_e2e: rename moves dir + config and `active_profile`;
  clone-memory copies `memory/*.md` but not `state.db`.

### J2 · One bot token, one daemon — *S*
- **Now:** doctor warns; two daemons still fight (409) at runtime.
- **Change:** daemon start takes an exclusive lock
  `~/.ka-agent/locks/telegram-<sha256(token)[..16]>`; conflict → refuses to
  start the channel with "token in use by profile X".
- **Acceptance:** e2e — second daemon on another profile with the same
  token logs the refusal and does not poll.

---

## 11. Updates & distribution

### K1 · `ka-agent update` — *M*
- **Now:** re-run `curl | sh`, then `service install` per profile.
- **Change:** `ka-agent update [--check] [--version X]`: fetch release
  metadata, download the platform tarball + `.sha256`, verify, atomic
  replace of the running binary, then restart every installed profile
  service. `status`/`doctor` show "update available" (cached daily check,
  `update.check = false` opts out).
- **Acceptance:** e2e against a local fixture HTTP server serving a fake
  release: `update --check` reports the newer version; `update` replaces a
  copied binary in a temp dir and refuses on checksum mismatch.

### K2 · Release reliability — *S*
- **Now:** v0.4.2 macOS x86_64 starved twice for a runner.
- **Change:** one macOS job builds both darwin targets sequentially and
  uploads both; same tag never needs manual reruns.
- **Acceptance:** next tag's Release run has 3 build jobs, 8 assets.

---

## 12. Security & robustness

### L1 · Private data tree — *S*
- **Now:** `~/.ka-agent` and profile dirs created 0755
  (`crates/ka-agent-runtime/src/paths.rs:80`, observed) — memory, state.db
  and logs readable by other local users (secrets are 0600).
- **Change:** create base, profile root and subdirs 0700; doctor fixes
  existing trees with `--fix` and warns otherwise.
- **Acceptance:** layout_e2e asserts mode 0700 on a fresh tree; doctor
  warns on a 0755 tree.

### L2 · DNS-rebinding-safe fetch — *M*
- **Now:** private-target check is on the hostname
  (`mcp.rs:833`), before resolution.
- **Change:** resolve, reject private IPs (unless granted), and connect to
  the pinned IP (reqwest `resolve` override) for `http_get` and `@url` refs.
- **Acceptance:** unit test with a resolver stub mapping a public name to
  127.0.0.1 → refused without `http:get:private`.

### L3 · Log rotation — *S*
- **Now:** launchd/foreground `logs/daemon.log` grows unbounded
  (`service.rs:185,541`).
- **Change:** daemon rotates at 5 MB, keeps 3.
- **Acceptance:** unit test on the rotating writer.

---

## 13. Proposed phases (≤ 4 releases)

| Phase | Theme | Items |
|---|---|---|
| **P1 · v0.5** | Useful on day one | E1, E2, E3, A1, A2, A3, B1, C1, C2, L1, K2 |
| **P2 · v0.6** | Phone-first | D1, D2, D3, D4, D5, F1, F2, G1 |
| **P3 · v0.7** | Control & trust | B2, B3, B4, G2, G3, G4, G5, H1, H2, E4, E5, J1, J2, L3 |
| **P4 · v0.8** | Polish & reach | I1, I2, I3, K1, A4, L2 |

Each phase ends with: the gate (fmt, clippy `-D warnings`, `cargo test
--workspace`, `check-ka-deps.sh`), every item's acceptance check, a
real-binary smoke of each changed surface, CI green, tagged release with 8
assets.

**Explicitly not proposed:** a skills marketplace (the category's malware
vector [S8]); group chats/multi-user (ADR-0001); hosted/cloud offering;
heartbeat-style periodic polling (G1/G2 cover proactivity without the cost
or the noise).

---

## Sources

- [S1] OneClaw — personal AI agents on Reddit: https://www.oneclaw.net/blog/personal-ai-agents-reddit
- [S2] Hermes Agent reviews: https://www.trustpilot.com/review/hermes-agent.nousresearch.com · https://fast.io/resources/hermes-agent-review-2026
- [S3] Vellum — best AI assistant per Reddit: https://www.vellum.ai/blog/best-ai-assistant-reddit · r/SelfHostedAI: https://www.reddit.com/r/SelfHostedAI/comments/1w00dde/my_phone_is_now_my_personal_ai_assistant
- [S4] OpenClaw honest 30-day review: https://clawtank.dev/blog/openclaw-honest-review
- [S5] Memory pain points: https://www.reddit.com/r/AI_Agents/comments/1r2puny/has_anyone_actually_solved_the_memory_problem_for · https://www.reddit.com/r/AI_Agents/comments/1w73l7y/i_built_my_ai_agents_a_local_longterm_memory_4
- [S6] Why proactive AI is annoying: https://www.notis.ai/blog/why-proactive-ai-is-actually-super-annoying
- [S7] Approval fatigue: https://www.developersdigest.tech/blog/approval-fatigue-agent-security-bug · https://grith.ai/blog/permission-fatigue-security-failure
- [S8] Skill supply chain: https://snyk.io/blog/toxicskills-malicious-ai-agent-skills-clawhub · https://conscia.com/blog/the-openclaw-security-crisis
- [S9] XDA — Hermes delivers what OpenClaw promised: https://www.xda-developers.com/openclaw-promised-self-hosted-ai-assistant-hermes-agent-delivers
- [S10] Telegram bot UX: https://core.telegram.org/bots/features · https://www.getopenclaw.ai/blog/openclaw-telegram
- [S11] Lightweight agents: https://pinggy.io/blog/zeroclaw_lightweight_openclaw_alternative · https://zeroclaw.net/zeroclaw-vs-openclaw-vs-picoclaw
- [S12] Bot API changelog (sendMessageDraft, 9.3/9.5): https://core.telegram.org/bots/api-changelog · https://github.com/openclaw/openclaw/issues/31999
- [S13] Personal MCP servers: https://mcpservers.org/servers/home-assistant-mcp · https://flowsavvy.app/google-calendar-mcp
- [S14] Telegram voice transcription self-hosted: https://www.reddit.com/r/homeassistant/comments/1qs1n95/i_built_a_selfhosted_telegram_voice_transcription
- [S15] Heartbeat cost and cadence: https://openclawready.com/blog/openclaw-heartbeats · https://myclaw.ai/learn/day-6-automation
- [R] Earlier competitor comparison (Hermes, OpenClaw, Claude Code, Codex, OpenCode, Goose, Letta), summarised in §0; the per-dimension tables informed every section.
