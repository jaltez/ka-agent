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

## Operator grants (`[grants]`) — paths, installs, full access

Status: implemented 2026-10-29. `[grants]` widens (or narrows) what the
engine may do without an approval card. Scope-string grants above stay
as they were; this section is the TOML surface.

```toml
[grants]
install = false     # app installs without asking (default false)
full = false        # everything without asking (default false)
advisory = true     # keep the one-line full-access warning (default true)

[[grants.path]]
dir = "/srv/data"   # absolute; duplicates are a config error
access = "allow"    # allow | ask | deny ("always" still reads as allow)
```

### Path grants

- `allow`: the dir is writable without asking. Two pieces: it joins
  the engine sandbox's `allow_write` (so bash redirects inside it
  compute no sandbox-grant ask), and the `__guard` hook pre-approves
  `write`/`edit` calls whose resolved target lands inside the dir
  (`{"decision":"allow"}`) — per path, so any number of granted dirs
  behaves identically. The operator's own engine-layer `[[rules]]`
  pass through untouched.
- `ask`: the engine's normal ask flow decides (identical to no entry).
- `deny`: blocked in every mode, for bash (path-shaped tokens,
  redirects and `path=` values) and the file tools (`write`/`edit`
  always; `read`/`grep` too). The error names the entry:
  `denied by grant for <dir> — remove or change the [[grants.path]]
  entry to allow it`.

### Install gate

`grants.install = false` (default): install-class commands are denied
with `app installs are denied — set grants.install = true to allow
them`. Install-class means the first program of any shell segment
(after `sudo`/`doas`) is `apt`/`apt-get`/`dnf`/`yum`/`pip`/`pip3`/
`pipx`/`cargo`/`go` with an `install` subcommand, `dpkg -i/--install`,
`pacman -S…`, `npm install|i|add -g/--global` (local installs never
match), or `nix-env -i/-iA/--install`. Plain `apt list` or `pip show`
never match.

`grants.install = true`: the `__guard` pre-approves the call
(`{"decision":"allow"}` on stdout). The engine's catastrophic
hardstops (fork bomb, `rm -rf /`, raw devices, `curl | sh`) still ask —
they outrank every hook, engine-side.

### Full access

`grants.full = true`: the OS sandbox is off, installs are granted and
deny dirs are ignored; bash and file writes are pre-approved. ka-agent's
own files stay protected in every mode: the enumerated file list always,
and the whole data root (minus the workspace and `attachments/`) plus
the binary whenever no operator gate remains (`mode = "allow"` or
`full`).

With `grants.advisory = true` (default), `/status` shows
`⚠ full access (grants.full): nothing asks` and `doctor` a warn line
(plus the `--json` check entry); `advisory = false` silences both
without changing access. `doctor` also lists
`grants: N paths (…), install on/off`.

### Why `mode = "allow"` composes no sandbox

The pinned engine (ka rev 193ad5d, `crates/ka-engine/src/voice.rs`
~2720-2760) poses a "sandbox grant for `…`" ask even when the
permission gate already returned `Gate::Allow` — an exec call that
computes sandbox grants (paths beyond the policy) asks regardless. So
`mode = "allow"` (and `grants.full`) must keep the sandbox off
entirely; with it on, bash would still raise approval cards outside
the workspace. In `ask` mode the sandbox stays on: the operator's card
covers both the command and its grants.

### Enforcement points

- Engine layer (`ka-agent-port/src/engine_layer.rs`): sandbox off in
  allow/full, `allow_write` widening. No `[[rules]]` are composed for
  grant dirs — the hook owns that.
- `ka-agent __guard` (`apps/ka-agent/src/guard.rs`): the
  `pre_tool_use` hook for `bash|write|edit|read|grep`. Grant decisions
  arrive as `KA_AGENT_GUARD_GRANTS` JSON
  (`{"install":bool,"full":bool,"hard":bool,"deny":[…],"allow":[…]}`,
  set by the daemon; absent = all-false, behavior unchanged).
- `hard` marks that no operator gate remains for bash (allow mode or
  full): then the whole data root is protected for bash, not just the
  enumerated own files — in ask mode the approval card is the
  operator's veto instead.

### Ordering: hook pre-approve vs operator engine rules

The pinned engine runs `pre_tool_use` hooks before the permission gate
(`crates/ka-engine/src/voice.rs:2594-2625`, rev 193ad5d), and a hook
pre-approve may only *downgrade an ask*: with `hook_allowed` set it
computes the normal gate first and IGNORES the pre-approve when that
gate is a `Gate::Deny` (`voice.rs:2638-2660`, "hook pre-approve ignored").
Rules are evaluated inside `gate` (`voice.rs:3113`), so the effective
order is:

1. the guard's own blocks (protected files, deny grants) — exit 2,
   before anything engine-side;
2. an operator engine-layer **deny** rule — stands, the pre-approve is
   ignored (no bypass);
3. the hook pre-approve (allow grant / install / full);
4. an operator **ask** rule or the mode gate — downgraded to allow by
   the pre-approve.

So a `deny` grant and an operator deny rule both outrank every grant;
only ask-class decisions are skipped. The guard checks deny/protected
before printing any allow, which keeps that order true on its side too.
