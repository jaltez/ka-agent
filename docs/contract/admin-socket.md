# Admin socket protocol — M2

Status: implemented, 2026-10-15. The daemon (`ka-agent run`) always binds one
admin unix socket; it is the local, privileged control plane (SEED.md §5.1:
policy changes are CLI-only).

## Endpoint

- Path: `<KA_AGENT_DATA_DIR-or-default>/admin.sock`
- Permissions: `0600`, owner = the daemon user. A stale socket file is removed
  at startup.
- One request per connection; newline-delimited JSON; connection: close.

## Authentication

`SO_PEERCRED` via `UnixStream::peer_cred()`: the connecting process's uid
must equal the daemon's uid, otherwise the reply is `{"error":"forbidden"}`
and the connection closes. (Combined with the 0600 mode this is same-user
only.)

## Operations

| op | request | reply |
|---|---|---|
| status | `{"op":"status"}` | `{"ok":true,"session":"sess_…","status":"Idle","strand":"s…"\|null,"budget_usd":0.0,"budget_tokens":0,"crashes":0,"queue":0}` |
| — | anything else | `{"error":"unknown op"}` |
| — | invalid JSON | `{"error":"bad json"}` |

`ka-agent status` is the reference client (3 s timeout; friendly hint when the
daemon is not running).

## Deliberately absent

No write operations over the socket in M2: pairing, grants, budget overrides
and policy changes stay on the local CLI/file plane by design; nothing here
can approve, prompt or reconfigure the engine.
