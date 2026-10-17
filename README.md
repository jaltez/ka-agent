# Ka Agent

A resident assistant on the [Ka](https://github.com/jaltez/ka-harness) coding engine:
one always-on daemon per operator that keeps working across sessions and
channels — durable memory, tasks, approvals, Telegram — while all model
reasoning and tool execution stay in Ka engines that Ka Agent starts, watches
and stops.

**Status: M11 complete and verified.** Working today (`cargo test
--workspace`: 181 tests green; Telegram + Matrix + memory + history + MCP
bridge + scheduler + loopback web UI + sandboxed third-party plugins +
cross-channel identity linking + ops packaging (systemd units, backups,
doctor) + cron/@refs/AGENTS.md/calendar):

- **`ka-agent-core`** — domain types and state machines: ids, sessions/turns,
  approvals, tasks, queue, outbox, budgets, audit, strict `ka-agent.toml`
  config. No I/O.
- **`ka-agent-store`** — SQLite persistence (schema, migrations, audit chain).
- **`ka-agent-port`** — the `KaPort` trait and the crate-backed worker
  boundary (`docs/contract/`).
- **`ka-agent-runtime`** — session manager, durable queue, single-flight
  submission, approvals with deny-on-expiry, budgets, worker supervisor with
  backoff and a crash-loop breaker.
- **`ka-agent-memory`** — stable memory: Markdown files with provenance
  (`id`/`source`/`accepted` HTML comments), git-backed (every accepted
  change is a commit), compiled into the engine's `MEMORY.md` with a 6 KB
  budget (ka re-reads it per prompt, so updates need no worker restart).
  Candidate review (`/memory review` + `accept`/`reject`), `/remember`,
  `/forget`, `/memory` report.
- **`ka-agent-channels`** — channels: Telegram (Bot API long polling,
  allowlisted via identities, approval keyboards) and **Matrix** (CS API v3
  `/sync` + `/send`, text-command approvals) behind one `Channel` trait with
  a shared command router; `MultiSurface` fans runtime output to every
  enabled channel. Identities (§8.1, ADR-0001): `ka-agent identity
  link|list|unlink` gates every channel, audited, with automatic migration
  of the deprecated Telegram `allow_user_ids`.
- **`ka-agent-channels` (Telegram)** — Telegram Bot API channel: long polling with a
  persisted offset (at-least-once, deduped), numeric-id allowlist for
  private chats, local command routing (`/help /status /new /stop`),
  throttled delivery with typing indicator and progress edits, fence-balanced
  chunking, approval cards with inline keyboards, secret store (env +
  0600-file tiers).
- **`ka-agent-plugins`** — the plugin host (SEED §10 Phase 5): strict
  manifest v1, ed25519-signed archives with TOFU pinning, install/enable
  lifecycle (`ka-agent plugin …`), and an MCP-stdio runtime inside a
  bubblewrap sandbox (`--unshare-net`, read-only system, private tmp, 30 s
  wall clock, 64 KiB output cap, only declared secrets). The bridge
  advertises plugin tools to engines as `kagent.<tool>` behind the same
  grants and intents as first-party tools. Contract and authoring:
  `docs/contract/plugins.md`; example: `examples/plugins/probe`.
- **`ka-agent-web`** — the loopback-only web UI (SEED §10 Phase 4):
  server-rendered HTML, no JS/asset pipeline, session auth via a one-shot
  CLI-issued login link, and every user-derived string HTML-escaped. Pages:
  dashboard, history (+ per-turn view), memory (with an entry editor),
  approvals, audit, intents.
- **`ka-agent` (the binary)** — `ka-agent run` (daemon: runtime + optional
  Telegram channel + admin unix socket + optional web UI + graceful
  SIGINT/SIGTERM), `ka-agent status` (admin socket client), `ka-agent chat`
  (local console surface), `ka-agent web-token`, `ka-agent backup|restore`,
  `ka-agent restart` (graceful stop; systemd brings it back), `ka-agent
  doctor` (ka pin, contract-suite hash, sandbox, plugins, identities,
  backup age), and the
  hidden `ka-agent __worker` the runtime supervises.

The MCP bridge is live: the engine gets `kagent.*` tools (memory_propose,
notes_search/append/read) through the `[[mcp]]` overlay — model-proposed
memories flow into `/memory review`. The scheduler fires `/remind`
reminders. The web UI is live too (`ka-agent run --web-port 8480`, then
`ka-agent web-token`). Third-party plugins and multi-user come next
(see `SEED.md` §10). Memory and history are live:
`/remember <fact>`, `/memory`, `/forget <id>`, `/history <query>`,
`/history show <turn_id>` — on both the console and Telegram. Real Telegram
use: set `[telegram]` in the config, store the bot token
(`KA_AGENT_SECRET_TELEGRAM_BOT_TOKEN` or
`<data>/secrets/telegram.bot_token`), allow your numeric user id, then
`ka-agent run`.

## Layout

```
ka-agent/
  Cargo.toml              # workspace; ka crates pinned to an exact git rev
  apps/
    ka-agent/             # the binary: daemon, admin CLI, hidden __worker
  crates/
    ka-agent-core/        # domain types & state machines; no I/O
    ka-agent-port/        # KaPort trait + worker protocol; the ONLY crate
                          # (besides apps/ka-agent) that may depend on ka-*
    ka-agent-store/       # SQLite schema, migrations, backups, audit chain
    ka-agent-runtime/     # session manager, queue, approvals, supervisor
    ka-agent-memory/      # stable memory: git-backed Markdown + compiled MEMORY.md
    ka-agent-channels/    # Telegram channel (polling, allowlist, approvals)
    ka-agent-web/         # loopback-only web UI (session auth, HTML pages)
  docs/
    contract/             # ka-port.md, worker-protocol.md, web-ui.md, …
    ka-interface-validation.md
  examples/
    config/               # ka-agent.toml, engine.toml
  scripts/
    check-ka-deps.sh      # CI gate: ka-* deps stay inside the port
  SEED.md
```

## Build & test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The workspace denies `unwrap`/`expect`/`todo`/`unimplemented`/`dbg` and
forbids `unsafe_code`; CI (`.github/workflows/ci.yml`) runs fmt, clippy,
tests and the dependency gate.

## Configuration

`ka-agent.toml` (default `~/.config/ka-agent/ka-agent.toml`), strict —
unknown keys are errors. See `examples/config/ka-agent.toml` for a fully
commented copy; the schema lives in
`crates/ka-agent-core/src/config.rs`.

```toml
workspace = "~/projects/ka-agent-workspace"
mode = "guarded"
approval_ttl_secs = 900
```

Secrets never go in this file: the Telegram token is referenced by name in
the secret store (`telegram.token_secret`).

## Ops

Run it as a resident service (ADR-0001: one instance per operator):

```
cp dist/systemd/ka-agent.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now ka-agent
```

Nightly backups with the included timer (`ka-agent-backup.timer`,
`Persistent=true`), restore with `ka-agent restore <dir>` (refuses a
non-empty root; the drill is `ops_e2e.rs`). `ka-agent doctor` prints the
ka pin, the contract-suite hash, sandbox prerequisites and instance state.
Details: `docs/contract/ops.md`; the dogfooding checklist and the Phase 1
exit review live in `docs/ops/`.

## Web UI

Loopback-only, off by default:

```sh
ka-agent run --web-port 8480     # daemon + web UI
ka-agent web-token --port 8480   # prints a one-shot login URL (10 min TTL)
```

Open the printed URL: it sets an `HttpOnly; SameSite=Strict` session cookie
and redirects to the dashboard. Every page requires that cookie; the login
token is single-use, and mutations are POST-only. Pages: dashboard
(session/strand/budget/crashes), history search + per-turn view, memory
(entry editor + accepted entries + the exact compiled text), approvals
(with allow/deny), audit and intents. Details and the security checklist:
`docs/contract/web-ui.md`.

## The ka pin

The Ka crates (`ka-protocol`, `ka-engine`, `ka-dialect`) are pinned to an
exact git revision — `a56aae2d1b3a4c31c4910979e1033002eacc53a0` (ka
workspace 0.3.0, unreleased) — via a **local file:// mirror** of
`https://github.com/jaltez/ka-harness`:

```toml
ka-engine = { git = "file:///home/javier/projects/ka-harness", rev = "a56aae2d1b3a4c31c4910979e1033002eacc53a0" }
```

Upgrades are deliberate: bump the pin, keep the port contract suite green
(SEED §11.3). CI rewrites the pin to the public URL before building. When ka
0.3.0 ships, swap the `git` pins for `version = "=0.3.0"` crates.io
dependencies and delete the CI rewrite step.

## Further reading

- `SEED.md` — the product and architecture seed (§3.4 boundary, §3.8 errors,
  §11 layout).
- `docs/ka-interface-validation.md` — Phase 0 evidence: gap verdicts G1–G20,
  runtime probes, findings F-1…F-8.
- `docs/contract/ka-port.md`, `docs/contract/worker-protocol.md` — the Ka
  boundary contracts.
- `docs/contract/cron-and-refs.md` — cron grammar and recurrence semantics,
  `@` reference budget, AGENTS.md seeding, ICS digest limits.
- `docs/contract/web-ui.md` — routes, auth flow, security checklist.
