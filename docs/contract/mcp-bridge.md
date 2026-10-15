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

`/remind <Ns|Nm|Nh|Nd> <text>` creates a task in the store. A timer
(15 s tick in the daemon) fires due reminders as `⏰ <text>` messages
through the runtime's normal submission path (single-flight, queue,
approvals all apply).
