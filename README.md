# Ka Agent

A resident assistant on the [Ka](https://github.com/jaltez/ka-harness) coding
engine: one always-on daemon per operator that keeps working across sessions
and channels — durable memory, tasks, approvals, Telegram — while all model
reasoning and tool execution stay in Ka engines that Ka Agent starts, watches
and stops.

## Quickstart

```sh
cargo install --path apps/ka-agent    # or: cargo build --release -p ka-agent

ka-agent init                         # writes ~/.config/ka-agent/ka-agent.toml + data root
ka-agent key set ANTHROPIC_API_KEY    # paste the provider key (stdin; 0600, never printed)
ka-agent doctor                       # verify: config, key, git, sandbox, contract hash

ka-agent chat                         # talk to it in the terminal
ka-agent run                          # resident daemon (channels, scheduler, approvals)
```

From Telegram: `ka-agent secret set telegram.bot_token`, then set
`[telegram] enabled = true` and `token_secret = "telegram.bot_token"` in the
config, `ka-agent identity link telegram <your-numeric-id>`, and
`ka-agent run`. From Matrix, the same shape under `[matrix]`
(`docs/contract/matrix.md`).

Two rules of the house, both enforced by code:

- **Every config is explicit.** Missing or invalid config is an error, never
  a silent default (`init` writes the first one).
- **Nothing external happens unapproved.** External writes need a grant, are
  logged as intents (deduplicated by content hash), and approvals deny on
  expiry.

## What's in

**Status: working and verified.** `cargo test --workspace`: 186 tests green,
including e2e over the real binary (scripted engine, fake Bot API, fault
injection). Telegram + Matrix + memory + history + MCP bridge + scheduler +
loopback web UI + sandboxed third-party plugins + cron/`@refs`/AGENTS.md +
calendar digest + first-run setup.

- **`ka-agent-core`** — domain types and state machines: ids, sessions/turns,
  approvals, tasks, queue, outbox, budgets, audit, strict `ka-agent.toml`
  config. No I/O.
- **`ka-agent-store`** — SQLite persistence (WAL, schema v4, migrations,
  audit chain, FTS5 history, intent log with content-hash idempotency).
- **`ka-agent-port`** — the `KaPort` trait and the crate-backed worker
  boundary (`docs/contract/ka-port.md`, `worker-protocol.md`). The only
  crate that touches the `ka-*` engine crates.
- **`ka-agent-runtime`** — session manager, durable queue, single-flight
  submission, approvals with deny-on-expiry, budgets, worker supervisor with
  backoff and a crash-loop breaker, and one maintenance loop (approval
  expiry + scheduler + idle-worker reap) that sleeps until the next due
  task instead of polling. An idle worker is stopped after
  `worker_idle_timeout_secs` (default 15 min) so a resident daemon pays
  for its engine only while it works.
- **`ka-agent-memory`** — stable memory: Markdown with provenance comments,
  git-backed (every accepted change is a commit), compiled into the engine's
  `MEMORY.md` under a 6 KB budget (ka re-reads it per prompt). Candidates
  from the model land in `/memory review`; `/memory accept` takes a full id
  or any unique prefix.
- **`ka-agent-channels`** — Telegram (long polling, identity gating, typing
  indicator + progress edits, approval keyboards, command autocompletion via
  `setMyCommands`) and Matrix behind one `Channel` trait with a shared
  command router; `MultiSurface` fans runtime output to every enabled
  channel. One HTTP stack (`reqwest`) with a manual-redirect SSRF guard.
- **`ka-agent-plugins`** — signed (`ed25519`, TOFU-pinned) plugin archives
  with an MCP-stdio runtime inside a bubblewrap sandbox (`--unshare-net`,
  read-only system, 30 s wall clock, 64 KiB output cap).
  Contract: `docs/contract/plugins.md`; example: `examples/plugins/probe`.
- **`ka-agent-web`** — the loopback-only web UI: server-rendered HTML, no
  JS, one-shot CLI-issued login link, every user-derived string escaped.
  Dashboard, history, memory editor, approvals, audit, intents.
- **`ka-agent` (the binary)** — `init` · `key set|list` · `secret set|list`
  · `chat` · `run` · `status` · `restart` · `doctor` · `memory path|show|edit`
  · `skills list|enable|disable` · `identity` · `grant` · `plugin`
  · `backup|restore` · `export` · `purge` · `web-token` · hidden `__worker` / `__mcp`.

The engine gets `kagent.*` tools (memory_propose, notes_search/append/read,
http_get, GitHub, cron_manage) through the `[[mcp]]` overlay — model-proposed
memories flow into `/memory review`. `/remind` fires as a notification (no
model turn); `/cron add every …` runs prompts on a schedule. Skills are the
operator's `SKILL.md` library: `ka-agent skills enable <name>` provisions it
for the engine and `/skill <name> <text>` runs one turn with it
(`docs/contract/skills.md`). Send a photo or file on Telegram and it lands
in `<workspace>/attachments/` with an `@reference` you can paste back.

## Layout

```
ka-agent/
  Cargo.toml              # workspace; ka crates pinned to =0.3.0 on crates.io
  apps/ka-agent/          # the binary (daemon, admin CLI, hidden worker)
    src/{main,chat,daemon,doctor,setup,identity,plugin_cli,grant,ops,memory_cli}.rs
  crates/
    ka-agent-core/        # domain types & state machines; no I/O
    ka-agent-port/        # KaPort trait + worker protocol; the ONLY crate
                          # (besides apps/ka-agent) that may depend on ka-*
    ka-agent-store/       # SQLite schema, migrations, backups, audit chain
    ka-agent-memory/      # stable memory: git-backed Markdown + MEMORY.md
    ka-agent-channels/    # Telegram + Matrix channels, shared router
    ka-agent-web/         # loopback-only web UI (session auth, HTML pages)
    ka-agent-plugins/     # signed archives + bubblewrap sandbox host
  docs/
    contract/             # the behavior contracts (hash-embedded in doctor)
    ka-interface-validation.md
  examples/config/        # ka-agent.toml, engine.toml
  scripts/check-ka-deps.sh  # CI gate: ka-* deps stay inside the port
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
tests and the dependency gate. Builds are plain crates.io dependencies —
no local mirrors, no vendored repos.

## Configuration

`ka-agent.toml` (default `~/.config/ka-agent/ka-agent.toml`, or
`$KA_AGENT_CONFIG`), strict — unknown keys are errors. `ka-agent init`
writes a commented starter; the full schema lives in
`crates/ka-agent-core/src/config.rs`, a fully commented copy in
`examples/config/ka-agent.toml`.

Secrets never go in the file:

- Provider key: `ka-agent key set ANTHROPIC_API_KEY` (writes the engine's
  `~<data>/engine-home/.config/ka/.env`, 0600 — exactly what ka's own key
  prompt would write).
- Channel/connector tokens: `ka-agent secret set <name>` (0600 file under
  `<data>/secrets/`, referenced by name from the config).

Data root: `~/.local/share/ka-agent` (or `$KA_AGENT_DATA_DIR`). Everything
instance-owned lives there: state.db, memory/, notes/, secrets/, engine
homes, workspace, backups.

## Ops

Run it as a resident service (ADR-0001: one instance per operator):

```sh
cp dist/systemd/ka-agent.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now ka-agent
```

Nightly backups with the included timer (`ka-agent-backup.timer`,
`Persistent=true`), restore with `ka-agent restore <dir>` (refuses a
non-empty root; the drill is `ops_e2e.rs`). `ka-agent doctor` prints the ka
pin, the contract-suite hash, provider-key and git state, sandbox
prerequisites and instance state. Details: `docs/contract/ops.md`.

## Web UI

Loopback-only, off by default:

```sh
ka-agent run --web-port 8480     # daemon + web UI
ka-agent web-token --port 8480   # prints a one-shot login URL (10 min TTL)
```

The URL sets an `HttpOnly; SameSite=Strict` session cookie and redirects to
the dashboard. Mutations are POST-only. Details and the security checklist:
`docs/contract/web-ui.md`.

## The ka pin

The Ka crates (`ka-protocol`, `ka-engine`, `ka-dialect`, `ka-strand`) are
pinned to the exact released version — `=0.3.0` from crates.io — declared in
`[workspace.dependencies]` and reported by `doctor` (`KA_PIN` in
`ka-agent-port`). `scripts/check-ka-deps.sh` fails CI if the constant and
the manifest drift apart, or if any `ka-*` dependency escapes the port.

Upgrades are deliberate: bump the pin, keep the port contract suite green
(SEED §11.3).

## Further reading

- `SEED.md` — the product and architecture seed (§3.4 boundary, §3.8 errors,
  §11 layout).
- `docs/ka-interface-validation.md` — Phase 0 evidence: gap verdicts G1–G20,
  runtime probes, findings F-1…F-8.
- `docs/contract/` — one contract per subsystem; `doctor` embeds their
  sha256 so support can compare behavior builds were built against.
