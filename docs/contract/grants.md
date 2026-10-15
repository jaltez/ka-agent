# Grants contract — M5

Status: implemented, 2026-10-15. Grants are scope strings stored in the
store's `meta` table as a JSON array. They gate scoped tools in the MCP
bridge (SEED.md §8.2).

## CLI

```sh
ka-agent grant add http:get:example.com
ka-agent grant add github:issues:read
ka-agent grant add http:get:*
ka-agent grant list
ka-agent grant rm http:get:example.com
```

## Enforcement

- The MCP bridge (`ka-agent __mcp`) loads grants from the store **on every
  tools/call** (never cached; the daemon may change them between calls).
- Unscoped tools (always available): `memory_propose`, `notes_search`,
  `notes_append`, `notes_read`.
- Scoped tools (require a matching grant): `http_get` (`http:get:<host>`),
  `github_issues_list` (`github:issues:read`), `github_pulls_get`
  (`github:pulls:read`).
- Wildcards: `http:get:*` matches any host in that tier.
- Denial: the tool returns `isError` naming the required scope so the model
  can tell the operator what to grant.
- SSRF guard for `http_get`: private/loopback hosts require the additional
  `http:get:private` grant.

## Store

Grants persist in `meta(key = "grants")` as `{"scopes":["..."]}`. The MCP
server opens the store directly (same SQLite WAL instance as the daemon).
