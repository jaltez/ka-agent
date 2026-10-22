# MCP bridge contract — M4

Status: implemented, 2026-10-15. The Ka engine spawns `ka-agent __mcp` as
the `kagent` MCP server (stdio JSON-RPC 2.0, MCP 2025-06-18). Ka Agent
tools reach the engine as `kagent.<tool>` exec-tier hands.

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
like the bridge's: in `guarded` mode every call asks, and the approval card
names the tool (`use \`server.tool\`?`).

The MCP server inherits the worker's sanitized env, including
`KA_AGENT_DATA_DIR` (the instance root — set by `WorkerEnv::sanitize`).
It opens the SQLite store directly (WAL allows concurrent access with the
daemon).

## Tools

| tool | effect | description |
|---|---|---|
| `kagent.memory_propose` | pending candidate | Model proposes a memory for operator review (SEED.md §4.4). Nothing is stored until the operator accepts via `/memory accept`. Text: 1–300 chars, third person, no secrets. |
| `kagent.notes_search` | read | Case-insensitive substring search across `<data>/notes/*.md`. Max 20 hits. |
| `kagent.notes_append` | local write | Append a line to `<data>/notes/<name>.md`. Name: `[a-z0-9-]{1,64}`. |
| `kagent.notes_read` | read | Read a note file (truncated at 8 KB). |

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
