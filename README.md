# Ka Agent

A resident assistant on the [Ka](https://github.com/jaltez/ka) coding engine:
one always-on daemon per operator that keeps working across sessions and
channels — durable memory, tasks, approvals, Telegram — while all model
reasoning and tool execution stay in Ka engines that Ka Agent starts, watches
and stops.

**Status: M1 core complete and verified.** Working today (`cargo test
--workspace`: 75 tests green; `ka-agent chat` drives the full stack end to
end through a scripted engine speaker):

- **`ka-agent-core`** — domain types and state machines: ids, sessions/turns,
  approvals, tasks, queue, outbox, budgets, audit, strict `ka-agent.toml`
  config. No I/O.
- **`ka-agent-store`** — SQLite persistence (schema, migrations, audit chain).
- **`ka-agent-port`** — the `KaPort` trait and the crate-backed worker
  boundary (`docs/contract/`).
- **`ka-agent-runtime`** — session manager, durable queue, single-flight
  submission, approvals with deny-on-expiry, budgets, worker supervisor with
  backoff and a crash-loop breaker.
- **`ka-agent` (the binary)** — `ka-agent chat` (local console surface:
  `/help /status /new /stop /y /n /quit`), `ka-agent doctor`, and the hidden
  `ka-agent __worker` the runtime supervises.

Telegram, memory, plugins and the scheduler come in later milestones
(see `SEED.md` §10).

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
  docs/
    contract/             # ka-port.md, worker-protocol.md
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

## The ka pin

The Ka crates (`ka-protocol`, `ka-engine`, `ka-dialect`) are pinned to an
exact git revision — `a56aae2d1b3a4c31c4910979e1033002eacc53a0` (ka
workspace 0.3.0, unreleased) — via a **local file:// mirror** of
`https://github.com/jaltez/ka`:

```toml
ka-engine = { git = "file:///home/javier/projects/ka", rev = "a56aae2d1b3a4c31c4910979e1033002eacc53a0" }
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
