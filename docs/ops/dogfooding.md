# Dogfooding — Phase 1 exit (SEED.md §13.10, §9.3)

**Start date:** ____________  **End date (14 days later):** ____________
**Instance:** this machine, this data root, Telegram/Matrix linked.

The 14-day window runs on the *installed* instance (systemd user unit +
nightly backup timer), driven by real usage — not by tests. The exit review
(§9.3) happens after the window with the evidence below.

## Preconditions (done before day 1)

- [x] `cargo test --workspace` green; CI green on `main`.
- [x] `ka-agent doctor` clean: config validates, state at latest schema,
      bubblewrap present, no FAIL lines.
- [x] `ka-agent plugin keygen` run (plugins can install/sign).
- [x] systemd user unit installed and active:
      `systemctl --user status ka-agent`.
- [x] nightly backup timer installed:
      `systemctl --user list-timers ka-agent-backup.timer`.
- [x] restore drill passed once from the produced nightly
      (`docs/contract/ops.md` § restore).

## Daily log

| Date | Uptime | Turns | Notes (bugs, UX, surprises) |
|---|---|---|---|
|  |  |  |  |
|  |  |  |  |
|  |  |  |  |

## Evidence to collect during the window

- `journalctl --user -u ka-agent --since …` — no unexpected restart loops.
- `ka-agent status` snapshots — budgets, crashes, queue drain.
- At least one `ka-agent backup` + one **restore drill** into a fresh root
  (record the date below).
- At least one plugin install/update/disable cycle.
- At least one restart driven by `ka-agent restart` (systemd brings it
  back).

## Phase 1 exit review (§9.3) — checklist

- [ ] Two weeks of resident operation with no data loss (backups verified).
- [ ] Every crash accounted for (supervisor notes in the journal).
- [ ] Approvals round-trip from real usage (cards → decisions).
- [ ] Memory model used in practice (remember/forget/compiled).
- [ ] History search used and useful.
- [ ] Budget guard observed at least once or explicitly justified.
- [ ] Known gaps from the window filed as issues; nothing silent.

**Decision:** ____________ (exit Phase 1 / extend the window)
