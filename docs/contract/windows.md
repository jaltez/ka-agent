# Windows support contract (plan Phase 4, v0.12)

The binary builds and runs on Windows x86_64 (`x86_64-pc-windows-msvc`,
shipped as `ka-agent-x86_64-pc-windows-msvc.zip`). The architecture is the
same as on Unix: one daemon owns the data root, chat/CLI clients talk to it
over the admin endpoint, and workers are subprocesses speaking NDJSON over
pipes. The workspace forbids `unsafe`, so every platform call goes through a
safe crate (`rustix` on Unix, `crossterm` for the console on Windows).

## What differs

| Area | Unix | Windows |
|---|---|---|
| Admin endpoint | Unix socket `<root>/admin.sock`, 0600, `SO_PEERCRED` uid check | Named pipe `\\.\pipe\ka-agent-<sha256(root)[..16]>`, created with the default DACL (creator + SYSTEM + administrators); `first_pipe_instance` makes a second daemon on the same root fail instead of stealing clients |
| Worker kill | `/bin/kill -s KILL -- -<pgid>` (the worker is its own process group) | `taskkill /T /F /PID <pid>` (the whole child tree) |
| Raw mode | `rustix` termios, `OPOST` kept, bracketed paste + focus reports | `crossterm` raw mode and console events; the drawing layer emits console commands instead of ANSI (see `term/paint.rs`) |
| Service | systemd user units / launchd plists | Task Scheduler entries `\KaAgent` (default profile) and `\KaAgent-<profile>`, `/SC ONLOGON /RL LIMITED`, run as `ka-agent -p <profile> run`; start/stop/status through `schtasks.exe` |
| Permissions | 0700 data dirs, 0600 secrets | No POSIX modes: the data tree lives under `%USERPROFILE%\.ka-agent` and inherits the profile's ACL; `restrict_dir` is a no-op and doctor does not report modes |
| Home | `$HOME` | `$HOME` when set, else `%USERPROFILE%` (config `~` expansion and the data root agree) |
| Plugins | bubblewrap sandbox (Linux only) | `plugin install`/`enable` refuse with "plugins need Linux"; doctor says the same |
| Logs | `journalctl --user` (Linux), `<root>/logs/daemon.log` (macOS) | `<root>/logs/daemon.log` (`ka-agent logs [-f]`) |
| Shells | `sh -c` for hooks and cron gates | `cmd /C` (already in the runtime) |

`ka-agent run`, `chat`, `status`, `doctor`, `service …`, `logs`, the web
panel and the channels are the same code on both platforms.

## Chat on Windows

The inline terminal client runs when both the console input and output are
TTYs (Windows Terminal, conhost with VT support through crossterm). Keys are
read as console events and mapped to the same vocabulary as the Unix byte
decoder (Enter/Shift-Enter, arrows, Ctrl+A/E/K/U/W, Ctrl-R search, Tab
completion, Ctrl-C clear → stop → quit). When the console cannot be put in
raw mode, the line reader takes over exactly as it does on Unix.

## Paths

- Data root: `%USERPROFILE%\.ka-agent` (or `KA_AGENT_DATA_DIR`,
  `KA_AGENT_HOME`); profiles live under `profiles\<name>`.
- Config: `<root>\config.toml` — the same TOML, the same key registry.
- Instance lock: `<root>\instance.lock` (`fs2` uses `LockFileEx`).

## Verification

- CI builds the whole workspace, clips lib+bin targets with `-D warnings`
  and builds the release binary on `windows-latest`.
- The Unix CI keeps running every test; the Windows job proves the build
  (the e2e suites drive Unix paths, signals and sockets).
- Cross-checks run on the development machine:
  `cargo check --workspace --target x86_64-pc-windows-gnu`,
  `cargo clippy --workspace --lib --bins --target x86_64-pc-windows-gnu`,
  `cargo build --release -p ka-agent --target x86_64-pc-windows-gnu`.
- The named-pipe name derivation, the scheduled-task naming and the paint
  op list are unit-tested; the console backend itself needs a Windows host
  to exercise (the release job is the first real run).
