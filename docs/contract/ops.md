# Ops contract — packaging, supervision, backups (SEED.md §9.1, §13.10)

## Install (user units)

```
cp dist/systemd/ka-agent.service ~/.config/systemd/user/
cp dist/systemd/ka-agent-backup.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ka-agent ka-agent-backup.timer
```

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
ka pin revision (verified against the manifest by `check-ka-deps.sh`), the
contract-suite hash (sha256 of the ten embedded `docs/contract/*.md` — the
behavioral contract this binary shipped with), config/data/state/
workspace state, bubblewrap version (the plugin sandbox prerequisite),
linked identities, installed/enabled plugins, and the last backup age.
