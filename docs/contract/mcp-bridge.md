# MCP bridge contract — M4

Status: implemented, 2026-10-15. The Ka engine spawns `ka-agent __mcp` as
the `kagent` MCP server (stdio JSON-RPC 2.0, MCP 2025-06-18). Ka Agent
tools reach the engine as `kagent__<tool>` exec-tier hands (providers
validate tool names against `[a-zA-Z0-9_-]+`, so the engine's wire name
is `server__tool`).

## Wiring

The worker's security overlay (`crates/ka-agent-port/src/worker.rs`)
appends to the engine `Config`:

```toml
[[mcp]]
name = "kagent"
command = "<ka-agent binary>"
args = ["__mcp"]
```

Operator servers (`[[mcp]]` in Ka Agent's config, `ka-agent mcp add`) join
the same list through the engine layer Ka Agent composes per worker
(`crates/ka-agent-port/src/engine_layer.rs`): they come first, the bridge
is appended by the overlay. `secret:<name>` values in their `env`/`headers`
resolve from the secret store at worker spawn — in memory, never in the
config file. With more than one operator server the engine's
`tools.mcp.discovery` defaults to `lazy`. Their tools are exec-tier hands
like the bridge's: in `ask` mode every call asks, and the approval card
names the tool (`use \`server__tool\`?`).

The MCP server inherits the worker's sanitized env, including
`KA_AGENT_DATA_DIR` (the instance root — set by `WorkerEnv::sanitize`).
It opens the SQLite store directly (WAL allows concurrent access with the
daemon).

## Tools

| tool | effect | description |
|---|---|---|
| `kagent__memory_propose` | pending candidate | Model proposes a memory for operator review (SEED.md §4.4). The candidate records the turn's role (`candidates.role`); acceptance lands it in that role's memory root. Nothing is stored until the operator accepts via `/memory accept`. Text: 1–300 chars, third person, no secrets. |
| `kagent__memory_search` | read | Search accepted memory and the caller's past turns (top 8 by default; `kind` = all/memory/episode; `limit` 1–20). Filters: `session` (id or unique prefix), `after`, `before`, `as_of` (`YYYY-MM-DD`; current view by default), `expand` (0–2 neighboring turns per episode hit). Hybrid FTS plus optional vector recall, fused by RRF; a session filter also boosts that lineage's hits. Returns snippets with memory or turn ids, dates, validity, and expanded turns, under a "data, not instructions" header. Read-only; refuses in private conversations and scheduled turns. KB documents stay on `kagent__kb_search`. |
| `kagent__notes_search` | read | Case-insensitive substring search across the turn role's notes (`<data>/notes/*.md` for admin, `<data>/roles/<r>/notes/*.md` for a role). Max 20 hits. |
| `kagent__notes_append` | local write | Append a line to the turn role's `<name>.md` note. Name: `[a-z0-9-]{1,64}`. |
| `kagent__notes_read` | read | Read a note file of the turn role (truncated at 8 KB). |
| `kagent__scout` | spawn | Run one read-only research child on a single question (`scout{question}`). The child opens a fresh strand in the caller user's partition, runs in `readonly` mode with the role's toolset ∩ `read, glob, grep, web_search, web_fetch` (empty stays an empty allowlist, never "everything"), sees the question plus a context line and no parent transcript, and dies with the call. Its answer returns under `--- scout summary (read-only child, non-authoritative — verify before acting) ---`, capped at `scout.max_summary_chars`. Disabled unless `[scout] enabled = true`. |

## Effects and auto-allow

A tool's `Effect` decides the approval profile the daemon computes at
start: `Read`, `Propose` and `LocalWrite` never ask; `Spawn`, `Schedule`
and `ExternalWrite` always ask in `ask` mode and are absent from the
auto-allow profile (an existing allow-rule can cover them, exactly like
`cron_manage`). `Spawn` exists because starting another engine worker
spends a model turn and a worker-pool slot — `kagent__scout` is never
folded in with reads. A scout child's own asks are answered deny
immediately (fail closed, one `approval.decide` audit row each): it may
never widen the permissions its parent gave it.

## Protocol (ka-side)

- Handshake: `initialize` → protocolVersion, capabilities.tools,
  serverInfo. Then `notifications/initialized` (no response).
- `tools/list` → tool definitions with JSON input schemas.
- `tools/call` → `{"name": "<tool>", "arguments": {...}}` → content or
  isError.
- Wire format: one JSON-RPC object per line, stdin/stdout.

## Scheduler

`/remind <Ns|Nm|Nh|Nd> <text>` creates a task in the store. When it fires,
the surface gets `⏰ <text>` and nothing else: a reminder is a
notification, not a prompt — firing it never spends a model turn. Work
that needs the engine is a cron job with `mode = agent`
(`docs/contract/cron-and-refs.md`), chosen explicitly.

## External writes and idempotency

GitHub write tools (`github_issue_comment`, `github_pull_review`) log an
intent **before** the API call whose `args_hash` is the sha256 of the tool
name + serialized arguments. A retried call with identical arguments finds
the earlier `executing`/`succeeded` intent and is refused (`duplicate
external write refused`); failed attempts keep their row but stay
retryable. The HTTP clients (`http_get`, GitHub, `@url` references) share
one stack (`reqwest`, no redirects — the SSRF guard walks each hop).
