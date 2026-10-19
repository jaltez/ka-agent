# Ops contract — packaging, supervision, backups (SEED.md §9.1, §13.10)

## Install (user units)

```
ka-agent service install
```

The units are generated from the running binary (`ka-agent service print`
shows them): `ka-agent.service`, `ka-agent-backup.service` and
`ka-agent-backup.timer` under `$XDG_CONFIG_HOME/systemd/user/` (Linux) or
`~/Library/LaunchAgents/` (macOS). Install is idempotent and upgrade-safe:
it rewrites the definitions with the current binary and restarts the
daemon. It refuses while a foreground `ka-agent run` or chat holds the data
root.

- `ka-agent.service` — `Restart=always` (a clean exit is also a restart;
  `ka-agent restart` drives exactly that), start-rate limited (5 in 10 min
  → the unit stops until the operator intervenes), `ProtectSystem=strict`
  with write access only to the data root and config dir.
- One instance per operator (ADR-0001): a second operator = a second user
  or template instance with its own `KA_AGENT_DATA_DIR`.

## Backups and restore

- `ka-agent backup <dir>` — consistent snapshot: `state.db` via SQLite
  `VACUUM INTO` (safe while the daemon writes), `memory/` (git history
  included), `notes/`, and the config file. Staged then renamed; a
  pointer is recorded at `<data>/backups/latest.json` (`doctor` reports
  its age).
- `ka-agent restore <dir>` — unpacks into the data root; **refuses a
  non-empty root** without `--force`, and refuses while a daemon is
  running (admin.sock present). The drill: restore into a fresh root,
  then `doctor`, then a chat turn — `ops_e2e.rs` does exactly this.

## Restart

- `ka-agent restart` — graceful stop over the admin socket
  (`{"op":"shutdown"}`; the daemon exits 0). With the unit installed,
  systemd brings it straight back; without one it simply stops.
- The daemon registers its SIGINT/SIGTERM handlers before any channel or
  socket starts, so an early kill can never bypass the graceful path.

## doctor

`ka-agent doctor` reports, for support and §6.5: the binary version, the
ka pin (the exact crates.io version, verified against the manifest by
`check-ka-deps.sh`), the contract-suite hash (sha256 of the thirteen
embedded `docs/contract/*.md` — the behavioral contract this binary
shipped with), config presence and validity (a missing config says
`ka-agent setup`, a broken one says why), data/state/workspace state, the
engine provider-key count (ka's `.env` tier — `ka-agent key set`), git
presence (memory is git-backed; a missing git is a FAIL), bubblewrap
version (the plugin sandbox prerequisite), enabled skills, linked
identities, installed/enabled plugins, and the last backup age. Read-only;
every check names what to do about it.

## First-run setup

`ka-agent setup` writes the config (workspace under the data root),
creates the data directories, the state store and the git-backed memory
tree, and prints the next steps. `ka-agent key set <NAME>` writes the
engine provider key (0600, upsert); `ka-agent secret set <name>` writes
channel/connector tokens. All long-running paths are strict: a missing or
invalid config is an error pointing at `init`, never a silent default.
